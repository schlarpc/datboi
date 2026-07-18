/* glue.c — the ex-7z guest core (D110): 7z container parsing via the
 * vendored decoder's own 7zArcIn, plus a datboi-written STREAMING folder
 * decode. The SDK's SzArEx_Extract materializes a whole solid folder in
 * memory (a 4 GiB archive folder would blow the 1 GiB guest cap); this
 * decode is dictionary-bounded instead: LzmaDec/Lzma2Dec stream into
 * their circular dictionary window and every produced span is flushed
 * straight to the member splitter, which routes byte ranges to the
 * requested host sinks (slot-indexed — one guest pass serves the whole
 * D89 batch) and drops the rest.
 *
 * v1 folder shapes (a subset of 7zDec.c's CheckSupportedFolder, chosen
 * for streamability): one coder (Copy / LZMA / LZMA2), or a main coder
 * followed by a Delta filter (Delta is chunk-safe by design; its state
 * rides across flushes). Branch filters (BCJ/BCJ2/ARM…) and PPMd refuse
 * with SZ_ERROR_UNSUPPORTED — the host falls back and the container
 * stays literal, exactly the pre-ex-7z posture.
 *
 * Verification: a running CRC32 per REQUESTED member (checked against
 * the header's per-file CRC when defined) plus the folder CRC when
 * defined. The host additionally hash-tees every sink (D4), so a bug
 * here wastes CPU but cannot corrupt the store.
 *
 * Determinism (D5/D46): the only externals are the three datboi_* hooks
 * the Rust guest implements over the WIT resources, plus libc
 * memcpy/memset/malloc — all satisfied at link time, zero imports.
 */

#include <stdlib.h>
#include <string.h>

#include "7z.h"
#include "7zCrc.h"
#include "CpuArch.h"
#include "Delta.h"
#include "Lzma2Dec.h"
#include "LzmaDec.h"

/* Hooks implemented by the Rust guest (src/lib.rs) over the WIT
 * resources of the in-flight export call. */
extern UInt64 datboi_input_len(void);
extern size_t datboi_input_read_at(UInt64 off, Byte *buf, size_t n);
extern void datboi_sink_write(UInt32 slot, const Byte *buf, size_t n);

/* Error codes past the SDK's SRes range (SRes values pass through). */
#define EX7Z_ERR_MEMBER_CRC 100
#define EX7Z_ERR_RANGE 101

/* Look buffer for header parsing + packed-stream reads; CHUNK bounds a
 * single decode/flush step. Both stay far under the host's 16 MiB
 * read-at ceiling. */
#define LOOK_BUF_SIZE (1 << 20)
#define CHUNK (1 << 18)

/* Method ids (mirrors 7zDec.c). */
#define k_Copy 0
#define k_Delta 3
#define k_LZMA2 0x21
#define k_LZMA 0x30101

static void *ex_alloc(ISzAllocPtr p, size_t size)
{
  (void)p;
  return size == 0 ? NULL : malloc(size);
}
static void ex_free(ISzAllocPtr p, void *addr)
{
  (void)p;
  free(addr);
}
static const ISzAlloc g_alloc = { ex_alloc, ex_free };

/* ---- ISeekInStream over the host `file` resource ---- */

typedef struct
{
  ISeekInStream vt;
  UInt64 pos;
} CHookInStream;

static SRes HookInStream_Read(ISeekInStreamPtr pp, void *buf, size_t *size)
{
  Z7_CONTAINER_FROM_VTBL_TO_DECL_VAR_pp_vt_p(CHookInStream)
  const size_t want = *size;
  /* Host read-at is exact except at EOF; a short read IS EOF, which is
   * exactly the ISeekInStream contract. */
  const size_t got = want == 0 ? 0 : datboi_input_read_at(p->pos, (Byte *)buf, want);
  p->pos += got;
  *size = got;
  return SZ_OK;
}

static SRes HookInStream_Seek(ISeekInStreamPtr pp, Int64 *pos, ESzSeek origin)
{
  Z7_CONTAINER_FROM_VTBL_TO_DECL_VAR_pp_vt_p(CHookInStream)
  Int64 base = 0;
  if (origin == SZ_SEEK_CUR)
    base = (Int64)p->pos;
  else if (origin == SZ_SEEK_END)
    base = (Int64)datboi_input_len();
  else if (origin != SZ_SEEK_SET)
    return SZ_ERROR_PARAM;
  const Int64 target = base + *pos;
  if (target < 0)
    return SZ_ERROR_PARAM;
  p->pos = (UInt64)target;
  *pos = target;
  return SZ_OK;
}

/* ---- the open archive handle ---- */

typedef struct
{
  CHookInStream in;
  CLookToRead2 look;
  Byte *lookBuf;
  CSzArEx db;
} Ex7z;

void *ex7z_open(int *err)
{
  CrcGenerateTable();
  Ex7z *p = (Ex7z *)calloc(1, sizeof(Ex7z));
  if (!p)
  {
    *err = SZ_ERROR_MEM;
    return NULL;
  }
  p->in.vt.Read = HookInStream_Read;
  p->in.vt.Seek = HookInStream_Seek;
  p->in.pos = 0;
  p->lookBuf = (Byte *)malloc(LOOK_BUF_SIZE);
  if (!p->lookBuf)
  {
    free(p);
    *err = SZ_ERROR_MEM;
    return NULL;
  }
  LookToRead2_CreateVTable(&p->look, False);
  p->look.realStream = &p->in.vt;
  p->look.buf = p->lookBuf;
  p->look.bufSize = LOOK_BUF_SIZE;
  LookToRead2_INIT(&p->look)
  SzArEx_Init(&p->db);
  const SRes r = SzArEx_Open(&p->db, &p->look.vt, &g_alloc, &g_alloc);
  if (r != SZ_OK)
  {
    SzArEx_Free(&p->db, &g_alloc);
    free(p->lookBuf);
    free(p);
    *err = r;
    return NULL;
  }
  return p;
}

void ex7z_close(void *hv)
{
  Ex7z *p = (Ex7z *)hv;
  if (!p)
    return;
  SzArEx_Free(&p->db, &g_alloc);
  free(p->lookBuf);
  free(p);
}

UInt32 ex7z_num_files(void *hv)
{
  return ((Ex7z *)hv)->db.NumFiles;
}

void ex7z_file_info(void *hv, UInt32 i, UInt64 *size, UInt32 *crc, int *crc_defined,
    int *is_dir, int *solid)
{
  const CSzArEx *db = &((Ex7z *)hv)->db;
  *size = db->UnpackPositions[i + 1] - db->UnpackPositions[i];
  *crc_defined = SzBitWithVals_Check(&db->CRCs, i) ? 1 : 0;
  *crc = *crc_defined ? db->CRCs.Vals[i] : 0;
  *is_dir = SzArEx_IsDir(db, i) ? 1 : 0;
  const UInt32 folder = db->FileToFolder[i];
  *solid = (folder != (UInt32)-1
      && db->FolderToFile[folder + 1] - db->FolderToFile[folder] > 1) ? 1 : 0;
}

size_t ex7z_file_name_utf16(void *hv, UInt32 i, UInt16 *dest)
{
  return SzArEx_GetFileNameUtf16(&((Ex7z *)hv)->db, i, dest);
}

/* ---- member splitter: folder unpack stream → per-member sinks ---- */

typedef struct
{
  const CSzArEx *db;
  UInt32 file;     /* current db file index within the folder's range */
  UInt32 file_end; /* one past the folder's last file index */
  UInt64 remain;   /* bytes left in the current file */
  const UInt32 *wf; /* requested db file indices, ascending */
  const UInt32 *ws; /* parallel sink slots */
  UInt32 nw, wc;
  int cur_wanted;
  UInt32 cur_slot;
  UInt32 crc; /* running CRC of the current wanted file */
  UInt32 folder_crc;
} Split;

static void split_open_file(Split *sp)
{
  const CSzArEx *db = sp->db;
  sp->remain = db->UnpackPositions[sp->file + 1] - db->UnpackPositions[sp->file];
  sp->cur_wanted = (sp->wc < sp->nw && sp->wf[sp->wc] == sp->file) ? 1 : 0;
  if (sp->cur_wanted)
  {
    sp->cur_slot = sp->ws[sp->wc];
    sp->crc = CRC_INIT_VAL;
  }
}

static int split_close_file(Split *sp)
{
  if (sp->cur_wanted)
  {
    if (SzBitWithVals_Check(&sp->db->CRCs, sp->file)
        && CRC_GET_DIGEST(sp->crc) != sp->db->CRCs.Vals[sp->file])
      return EX7Z_ERR_MEMBER_CRC;
    sp->wc++;
  }
  sp->file++;
  return 0;
}

static int split_feed(Split *sp, const Byte *buf, size_t n)
{
  sp->folder_crc = CrcUpdate(sp->folder_crc, buf, n);
  while (n != 0)
  {
    while (sp->remain == 0)
    {
      const int r = split_close_file(sp);
      if (r)
        return r;
      if (sp->file >= sp->file_end)
        return SZ_ERROR_DATA; /* folder produced more bytes than its files hold */
      split_open_file(sp);
    }
    size_t take = n;
    if ((UInt64)take > sp->remain)
      take = (size_t)sp->remain;
    if (sp->cur_wanted)
    {
      datboi_sink_write(sp->cur_slot, buf, take);
      sp->crc = CrcUpdate(sp->crc, buf, take);
    }
    sp->remain -= take;
    buf += take;
    n -= take;
  }
  return 0;
}

/* Close out the trailing (possibly zero-size) files and verify counts. */
static int split_finish(Split *sp, UInt32 folderIndex)
{
  while (sp->file < sp->file_end)
  {
    if (sp->remain != 0)
      return SZ_ERROR_DATA; /* folder ended before its files did */
    const int r = split_close_file(sp);
    if (r)
      return r;
    if (sp->file < sp->file_end)
      split_open_file(sp);
  }
  if (sp->wc != sp->nw)
    return EX7Z_ERR_RANGE;
  if (SzBitWithVals_Check(&sp->db->db.FolderCRCs, folderIndex)
      && CRC_GET_DIGEST(sp->folder_crc) != sp->db->db.FolderCRCs.Vals[folderIndex])
    return SZ_ERROR_CRC;
  return 0;
}

/* ---- emit: optional Delta filter between decoder and splitter ----
 * The filter must NOT run in place on the LZMA dictionary (matches
 * reference the UNFILTERED coder output), so filtered spans copy
 * through a scratch buffer first. */

typedef struct
{
  Split *sp;
  int has_delta;
  unsigned delta;
  Byte delta_state[DELTA_STATE_SIZE];
  Byte *scratch; /* CHUNK bytes, only when has_delta */
} Emit;

static int emit(Emit *e, const Byte *buf, size_t n)
{
  if (!e->has_delta)
    return split_feed(e->sp, buf, n);
  while (n != 0)
  {
    size_t take = n < CHUNK ? n : CHUNK;
    memcpy(e->scratch, buf, take);
    Delta_Decode(e->delta_state, e->delta, e->scratch, take);
    const int r = split_feed(e->sp, e->scratch, take);
    if (r)
      return r;
    buf += take;
    n -= take;
  }
  return 0;
}

/* ---- streaming main-coder decoders ---- */

static int decode_copy(ILookInStreamPtr in, UInt64 inSize, UInt64 outSize, Emit *e)
{
  if (inSize != outSize)
    return SZ_ERROR_DATA;
  while (inSize != 0)
  {
    const void *b;
    size_t sz = CHUNK;
    if ((UInt64)sz > inSize)
      sz = (size_t)inSize;
    RINOK(ILookInStream_Look(in, &b, &sz))
    if (sz == 0)
      return SZ_ERROR_INPUT_EOF;
    const int r = emit(e, (const Byte *)b, sz);
    if (r)
      return r;
    inSize -= sz;
    RINOK(ILookInStream_Skip(in, sz))
  }
  return SZ_OK;
}

/* The documented dictionary-interface streaming shape (LzmaDec.h):
 * decode into the circular window, flush each produced span, reset
 * dicPos on wrap. Window = min(declared dict, folder output) — a
 * declared-huge dictionary on a small folder costs nothing, and a
 * genuinely huge one hits the allocator → clean refusal under the
 * wasmtime memory cap (the ruled bomb posture). */
static int lzma_pump(CLzmaDec *st, ILookInStreamPtr in, UInt64 inSize, UInt64 outSize,
    Emit *e, int lzma2, CLzma2Dec *st2)
{
  UInt64 remainOut = outSize;
  while (remainOut != 0)
  {
    const void *inBuf = NULL;
    size_t look = CHUNK;
    if ((UInt64)look > inSize)
      look = (size_t)inSize;
    RINOK(ILookInStream_Look(in, &inBuf, &look))

    SizeT dicLimit;
    ELzmaFinishMode fm;
    if (remainOut < (UInt64)(st->dicBufSize - st->dicPos))
    {
      dicLimit = st->dicPos + (SizeT)remainOut;
      fm = LZMA_FINISH_END;
    }
    else
    {
      dicLimit = st->dicBufSize;
      fm = LZMA_FINISH_ANY;
    }
    SizeT inProcessed = look;
    const SizeT before = st->dicPos;
    ELzmaStatus status;
    const SRes res = lzma2
        ? Lzma2Dec_DecodeToDic(st2, dicLimit, (const Byte *)inBuf, &inProcessed, fm, &status)
        : LzmaDec_DecodeToDic(st, dicLimit, (const Byte *)inBuf, &inProcessed, fm, &status);
    inSize -= inProcessed;
    if (res != SZ_OK)
      return res;
    const SizeT produced = st->dicPos - before;
    if (produced != 0)
    {
      const int r = emit(e, st->dic + before, produced);
      if (r)
        return r;
      remainOut -= produced;
    }
    RINOK(ILookInStream_Skip(in, inProcessed))
    if (st->dicPos == st->dicBufSize)
      st->dicPos = 0;
    if (inProcessed == 0 && produced == 0)
      return SZ_ERROR_DATA; /* no progress: truncated or corrupt stream */
  }
  /* Strict like 7zDec: the folder's packed stream must be fully consumed. */
  return inSize == 0 ? SZ_OK : SZ_ERROR_DATA;
}

static int decode_lzma(const Byte *props, unsigned propsSize, ILookInStreamPtr in,
    UInt64 inSize, UInt64 outSize, Emit *e)
{
  if (propsSize != LZMA_PROPS_SIZE)
    return SZ_ERROR_UNSUPPORTED;
  CLzmaDec st;
  LzmaDec_CONSTRUCT(&st)
  RINOK(LzmaDec_AllocateProbs(&st, props, propsSize, &g_alloc))
  UInt64 window = GetUi32(props + 1);
  if (window > outSize)
    window = outSize;
  if (window < 1)
    window = 1;
  st.dicBufSize = (SizeT)window;
  st.dic = (Byte *)malloc(st.dicBufSize);
  int res;
  if (!st.dic)
    res = SZ_ERROR_MEM;
  else
  {
    LzmaDec_Init(&st);
    res = lzma_pump(&st, in, inSize, outSize, e, 0, NULL);
    free(st.dic);
  }
  st.dic = NULL;
  LzmaDec_FreeProbs(&st, &g_alloc);
  return res;
}

static int decode_lzma2(const Byte *props, unsigned propsSize, ILookInStreamPtr in,
    UInt64 inSize, UInt64 outSize, Emit *e)
{
  if (propsSize != 1 || props[0] > 40)
    return SZ_ERROR_UNSUPPORTED;
  CLzma2Dec st;
  Lzma2Dec_CONSTRUCT(&st)
  RINOK(Lzma2Dec_AllocateProbs(&st, props[0], &g_alloc))
  UInt64 window = (props[0] == 40)
      ? (UInt64)0xFFFFFFFF
      : (UInt64)(((UInt32)2 | (props[0] & 1)) << (props[0] / 2 + 11));
  if (window > outSize)
    window = outSize;
  if (window < 1)
    window = 1;
  st.decoder.dicBufSize = (SizeT)window;
  st.decoder.dic = (Byte *)malloc(st.decoder.dicBufSize);
  int res;
  if (!st.decoder.dic)
    res = SZ_ERROR_MEM;
  else
  {
    Lzma2Dec_Init(&st);
    res = lzma_pump(&st.decoder, in, inSize, outSize, e, 1, &st);
    free(st.decoder.dic);
  }
  st.decoder.dic = NULL;
  Lzma2Dec_FreeProbs(&st, &g_alloc);
  return res;
}

/* ---- folder driver ---- */

static int decode_folder(Ex7z *p, UInt32 folderIndex, const UInt32 *wf, const UInt32 *ws,
    UInt32 nw)
{
  const CSzAr *ar = &p->db.db;
  CSzFolder folder;
  CSzData sd;
  const Byte *data = ar->CodersData + ar->FoCodersOffsets[folderIndex];
  sd.Data = data;
  sd.Size = ar->FoCodersOffsets[folderIndex + 1] - ar->FoCodersOffsets[folderIndex];
  RINOK(SzGetNextFolderItem(&folder, &sd))
  if (sd.Size != 0 || folder.UnpackStream != ar->FoToMainUnpackSizeIndex[folderIndex])
    return SZ_ERROR_FAIL;

  /* v1 shapes: single main coder, or main + Delta (module doc). */
  const CSzCoderInfo *main_coder = &folder.Coders[0];
  int has_delta = 0;
  unsigned delta = 0;
  if (main_coder->NumStreams != 1)
    return SZ_ERROR_UNSUPPORTED;
  if (folder.NumCoders == 1)
  {
    if (folder.NumPackStreams != 1 || folder.PackStreams[0] != 0 || folder.NumBonds != 0)
      return SZ_ERROR_UNSUPPORTED;
  }
  else if (folder.NumCoders == 2)
  {
    const CSzCoderInfo *c = &folder.Coders[1];
    if (c->NumStreams != 1 || folder.NumPackStreams != 1 || folder.PackStreams[0] != 0
        || folder.NumBonds != 1 || folder.Bonds[0].InIndex != 1
        || folder.Bonds[0].OutIndex != 0)
      return SZ_ERROR_UNSUPPORTED;
    if (c->MethodID != k_Delta || c->PropsSize != 1)
      return SZ_ERROR_UNSUPPORTED;
    has_delta = 1;
    delta = (unsigned)data[c->PropsOffset] + 1;
  }
  else
    return SZ_ERROR_UNSUPPORTED;

  const UInt64 outSize = SzAr_GetFolderUnpackSize(ar, folderIndex);
  const UInt64 *packPositions = ar->PackPositions + ar->FoStartPackStreamIndex[folderIndex];
  const UInt64 inSize = packPositions[1] - packPositions[0];
  RINOK(LookInStream_SeekTo(&p->look.vt, p->db.dataPos + packPositions[0]))

  Split sp;
  memset(&sp, 0, sizeof(sp));
  sp.db = &p->db;
  sp.file = p->db.FolderToFile[folderIndex];
  sp.file_end = p->db.FolderToFile[folderIndex + 1];
  sp.wf = wf;
  sp.ws = ws;
  sp.nw = nw;
  sp.folder_crc = CRC_INIT_VAL;
  if (sp.file >= sp.file_end)
    return SZ_ERROR_DATA;
  split_open_file(&sp);

  Emit e;
  memset(&e, 0, sizeof(e));
  e.sp = &sp;
  e.has_delta = has_delta;
  e.delta = delta;
  if (has_delta)
  {
    Delta_Init(e.delta_state);
    e.scratch = (Byte *)malloc(CHUNK);
    if (!e.scratch)
      return SZ_ERROR_MEM;
  }

  int res;
  switch ((UInt32)main_coder->MethodID)
  {
    case k_Copy:
      res = decode_copy(&p->look.vt, inSize, outSize, &e);
      break;
    case k_LZMA:
      res = decode_lzma(data + main_coder->PropsOffset, main_coder->PropsSize, &p->look.vt,
          inSize, outSize, &e);
      break;
    case k_LZMA2:
      res = decode_lzma2(data + main_coder->PropsOffset, main_coder->PropsSize, &p->look.vt,
          inSize, outSize, &e);
      break;
    default:
      res = SZ_ERROR_UNSUPPORTED;
  }
  if (res == 0)
    res = split_finish(&sp, folderIndex);
  free(e.scratch);
  return res;
}

/* ---- the batch entry point (D89): requests sorted by file index, one
 * folder decoded once however many of its members are requested ---- */

int ex7z_extract(void *hv, const UInt32 *files_in, const UInt32 *slots_in, UInt32 n)
{
  Ex7z *p = (Ex7z *)hv;
  if (n == 0)
    return 0;
  UInt32 *files = (UInt32 *)malloc((size_t)n * sizeof(UInt32));
  UInt32 *slots = (UInt32 *)malloc((size_t)n * sizeof(UInt32));
  if (!files || !slots)
  {
    free(files);
    free(slots);
    return SZ_ERROR_MEM;
  }
  memcpy(files, files_in, (size_t)n * sizeof(UInt32));
  memcpy(slots, slots_in, (size_t)n * sizeof(UInt32));
  /* Insertion sort by file index (batches are small, D89 caps them). */
  for (UInt32 i = 1; i < n; i++)
  {
    const UInt32 f = files[i], s = slots[i];
    UInt32 j = i;
    for (; j > 0 && files[j - 1] > f; j--)
    {
      files[j] = files[j - 1];
      slots[j] = slots[j - 1];
    }
    files[j] = f;
    slots[j] = s;
  }

  int res = 0;
  for (UInt32 i = 0; i < n && res == 0; i++)
  {
    if (files[i] >= p->db.NumFiles || SzArEx_IsDir(&p->db, files[i])
        || (i > 0 && files[i] == files[i - 1]))
      res = EX7Z_ERR_RANGE;
  }

  UInt32 i = 0;
  while (i < n && res == 0)
  {
    const UInt32 fi = files[i];
    const UInt32 folder = p->db.FileToFolder[fi];
    const UInt64 size = p->db.UnpackPositions[fi + 1] - p->db.UnpackPositions[fi];
    if (folder == (UInt32)-1 || size == 0)
    {
      i++; /* empty member: its sink correctly receives nothing */
      continue;
    }
    /* Gather every request inside this folder's file range (includes
     * requested empty members interleaved in the range — the splitter
     * walks them too). */
    UInt32 j = i + 1;
    while (j < n && files[j] < p->db.FolderToFile[folder + 1])
      j++;
    res = decode_folder(p, folder, files + i, slots + i, j - i);
    i = j;
  }
  free(files);
  free(slots);
  return res;
}
