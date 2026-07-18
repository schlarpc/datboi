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
 * Folder coverage is the FULL 7zDec set (no de-scope; sevenz-rust2 left
 * the tree on this, per D108):
 *   - one main coder: Copy / LZMA / LZMA2 / PPMd7;
 *   - main + one filter: Delta or a branch converter (x86 BCJ with its
 *     resumable state, ARM64/ARM/ARMT/PPC/SPARC/IA64/RISCV) — filters
 *     run CHUNKED with an instruction-boundary carry tail, exactly the
 *     resume contract Bra.h documents;
 *   - the BCJ2 four-coder shape: the MAIN stream (coder2) streams
 *     through its dictionary window into the Bcj2Dec state machine;
 *     the small call/jump/rc streams buffer whole (a bomb-sized side
 *     stream hits the allocator → clean refusal under the memory cap).
 * Unsupported = folder graphs even 7zDec refuses; those error politely
 * and the archive stays a literal.
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
#include "Bcj2.h"
#include "Bra.h"
#include "CpuArch.h"
#include "Delta.h"
#include "Lzma2Dec.h"
#include "LzmaDec.h"
#include "Ppmd7.h"

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
/* Branch-filter carry margin over CHUNK (max converter lookahead is 6;
 * 64 is comfortable and keeps the buffer pointer-aligned). */
#define BRANCH_SLACK 64

/* Method ids (mirrors 7zDec.c). */
#define k_Copy 0
#define k_Delta 3
#define k_ARM64 0xa
#define k_RISCV 0xb
#define k_LZMA2 0x21
#define k_LZMA 0x30101
#define k_PPMD 0x30401
#define k_BCJ 0x3030103
#define k_PPC 0x3030205
#define k_IA64 0x3030401
#define k_ARM 0x3030501
#define k_ARMT 0x3030701
#define k_SPARC 0x3030805
#define k_BCJ2 0x303011B

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

/* ---- emit: the filter stage between main decoder and splitter ----
 * Filters must NOT run in place on the LZMA dictionary (matches
 * reference the UNFILTERED coder output), so filtered spans copy
 * through the stage's scratch buffer first. */

enum
{
  EMIT_PLAIN,
  EMIT_DELTA,
  EMIT_BRANCH,
  EMIT_BCJ2
};

typedef struct
{
  Split *sp;
  int kind;
  /* EMIT_DELTA */
  unsigned delta;
  Byte delta_state[DELTA_STATE_SIZE];
  /* EMIT_BRANCH — chunked with a carry tail at an instruction
   * boundary (the converter's documented resume contract). */
  z7_Func_BranchConv conv; /* stateless family */
  int is_x86;              /* x86 uses the resumable St variant */
  UInt32 x86_state;
  UInt32 pc;
  size_t pend_len; /* carried tail length at scratch[0..] */
  /* EMIT_BCJ2 — the state machine pulls MAIN spans as they stream. */
  CBcj2Dec bcj2;
  UInt64 bcj2_dest_total;
  UInt64 bcj2_dest_done;
  /* stage scratch (delta copies / branch pending / bcj2 dest) */
  Byte *scratch;
  size_t scratch_cap;
} Emit;

static int emit_bcj2_pump(Emit *e, const Byte *buf, size_t n)
{
  e->bcj2.bufs[BCJ2_STREAM_MAIN] = buf;
  e->bcj2.lims[BCJ2_STREAM_MAIN] = buf + n;
  for (;;)
  {
    Byte *dst = e->scratch;
    const UInt64 want = e->bcj2_dest_total - e->bcj2_dest_done;
    const size_t cap = want < CHUNK ? (size_t)want : CHUNK;
    e->bcj2.dest = dst;
    e->bcj2.destLim = dst + cap;
    RINOK(Bcj2Dec_Decode(&e->bcj2))
    const size_t out = (size_t)(e->bcj2.dest - dst);
    if (out != 0)
    {
      const int r = split_feed(e->sp, dst, out);
      if (r)
        return r;
      e->bcj2_dest_done += out;
    }
    const int main_left =
        e->bcj2.bufs[BCJ2_STREAM_MAIN] != e->bcj2.lims[BCJ2_STREAM_MAIN];
    if (!main_left && (out == 0 || cap == 0))
      break; /* span consumed; the next main span (or finish) continues */
    if (main_left && out == 0)
      return SZ_ERROR_DATA; /* stalled: a side stream starved mid-decode */
  }
  /* Never leave dangling span pointers between emit calls. */
  e->bcj2.bufs[BCJ2_STREAM_MAIN] = e->bcj2.lims[BCJ2_STREAM_MAIN] = e->scratch;
  return 0;
}

static int emit(Emit *e, const Byte *buf, size_t n)
{
  switch (e->kind)
  {
    case EMIT_PLAIN:
      return split_feed(e->sp, buf, n);

    case EMIT_DELTA:
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

    case EMIT_BRANCH:
      while (n != 0)
      {
        const size_t space = e->scratch_cap - e->pend_len;
        const size_t take = n < space ? n : space;
        memcpy(e->scratch + e->pend_len, buf, take);
        e->pend_len += take;
        buf += take;
        n -= take;
        Byte *stop = e->is_x86
            ? z7_BranchConvSt_X86_Dec(e->scratch, e->pend_len, e->pc, &e->x86_state)
            : e->conv(e->scratch, e->pend_len, e->pc);
        const size_t done = (size_t)(stop - e->scratch);
        if (done != 0)
        {
          const int r = split_feed(e->sp, e->scratch, done);
          if (r)
            return r;
          e->pc += (UInt32)done;
          memmove(e->scratch, e->scratch + done, e->pend_len - done);
          e->pend_len -= done;
        }
        else if (take == 0)
          /* Buffer full yet the converter cannot advance: impossible
           * with CHUNK-scale capacity vs ≤6-byte lookahead — refuse
           * rather than spin. */
          return SZ_ERROR_DATA;
      }
      return 0;

    default:
      return emit_bcj2_pump(e, buf, n);
  }
}

/* End of the main stream: flush carries and run final-state checks. */
static int emit_finish(Emit *e)
{
  switch (e->kind)
  {
    case EMIT_PLAIN:
    case EMIT_DELTA:
      return 0;

    case EMIT_BRANCH:
      /* The trailing sub-lookahead bytes stay unconverted — identical
       * to the whole-buffer converters' own behavior. */
      return e->pend_len == 0 ? 0 : split_feed(e->sp, e->scratch, e->pend_len);

    default:
    {
      /* Drain what the state machine can still produce from its side
       * streams, then require the exact end state (7zDec's checks). */
      for (;;)
      {
        Byte *dst = e->scratch;
        const UInt64 want = e->bcj2_dest_total - e->bcj2_dest_done;
        const size_t cap = want < CHUNK ? (size_t)want : CHUNK;
        e->bcj2.dest = dst;
        e->bcj2.destLim = dst + cap;
        RINOK(Bcj2Dec_Decode(&e->bcj2))
        const size_t out = (size_t)(e->bcj2.dest - dst);
        if (out == 0)
          break;
        const int r = split_feed(e->sp, dst, out);
        if (r)
          return r;
        e->bcj2_dest_done += out;
      }
      if (e->bcj2_dest_done != e->bcj2_dest_total)
        return SZ_ERROR_DATA;
      for (unsigned i = 1; i < BCJ2_NUM_STREAMS; i++)
        if (e->bcj2.bufs[i] != e->bcj2.lims[i])
          return SZ_ERROR_DATA;
      if (!Bcj2Dec_IsMaybeFinished(&e->bcj2))
        return SZ_ERROR_DATA;
      return 0;
    }
  }
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
  for (;;)
  {
    const void *inBuf = NULL;
    size_t look = CHUNK;
    if ((UInt64)look > inSize)
      look = (size_t)inSize;
    RINOK(ILookInStream_Look(in, &inBuf, &look))

    /* Once the output is complete the stream may still owe its
     * terminator (LZMA2's 0x00 control byte, an LZMA end mark): keep
     * calling at the fixed limit with FINISH_END until the decoder
     * reports the mark or the packed stream runs out — 7zDec's shape. */
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
      if ((UInt64)produced > remainOut)
        return SZ_ERROR_DATA; /* stream inflates past the declared folder size */
      remainOut -= produced;
    }
    RINOK(ILookInStream_Skip(in, inProcessed))
    if (st->dicPos == st->dicBufSize)
      st->dicPos = 0;

    if (status == LZMA_STATUS_FINISHED_WITH_MARK)
      /* Strict like 7zDec: exact output, packed stream fully consumed. */
      return (remainOut == 0 && inSize == 0) ? SZ_OK : SZ_ERROR_DATA;
    if (remainOut == 0 && inSize == 0)
      return SZ_OK; /* markless end at the exact declared size */
    if (inProcessed == 0 && produced == 0)
      return SZ_ERROR_DATA; /* no progress: truncated or corrupt stream */
  }
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

/* PPMd7 (7z variant), ported from 7zDec's SzDecodePpmd but flushing
 * chunkwise instead of filling one whole-folder buffer. */

typedef struct
{
  IByteIn vt;
  const Byte *cur;
  const Byte *end;
  const Byte *begin;
  UInt64 processed;
  BoolInt extra;
  SRes res;
  ILookInStreamPtr inStream;
} CByteInToLook;

static Byte Ppmd_ReadByte(IByteInPtr pp)
{
  Z7_CONTAINER_FROM_VTBL_TO_DECL_VAR_pp_vt_p(CByteInToLook)
  if (p->cur != p->end)
    return *p->cur++;
  if (p->res == SZ_OK)
  {
    size_t size = (size_t)(p->cur - p->begin);
    p->processed += size;
    p->res = ILookInStream_Skip(p->inStream, size);
    size = CHUNK;
    p->res = ILookInStream_Look(p->inStream, (const void **)&p->begin, &size);
    p->cur = p->begin;
    p->end = p->begin + size;
    if (size != 0)
      return *p->cur++;
  }
  p->extra = True;
  return 0;
}

static int decode_ppmd(const Byte *props, unsigned propsSize, ILookInStreamPtr in,
    UInt64 inSize, UInt64 outSize, Emit *e)
{
  CPpmd7 ppmd;
  CByteInToLook s;
  int res = SZ_OK;

  s.vt.Read = Ppmd_ReadByte;
  s.inStream = in;
  s.begin = s.end = s.cur = NULL;
  s.extra = False;
  s.res = SZ_OK;
  s.processed = 0;

  if (propsSize != 5)
    return SZ_ERROR_UNSUPPORTED;
  {
    const unsigned order = props[0];
    const UInt32 memSize = GetUi32(props + 1);
    if (order < PPMD7_MIN_ORDER || order > PPMD7_MAX_ORDER
        || memSize < PPMD7_MIN_MEM_SIZE || memSize > PPMD7_MAX_MEM_SIZE)
      return SZ_ERROR_UNSUPPORTED;
    Ppmd7_Construct(&ppmd);
    if (!Ppmd7_Alloc(&ppmd, memSize, &g_alloc))
      return SZ_ERROR_MEM;
    Ppmd7_Init(&ppmd, order);
  }
  Byte *buf = (Byte *)malloc(CHUNK);
  if (!buf)
  {
    Ppmd7_Free(&ppmd, &g_alloc);
    return SZ_ERROR_MEM;
  }
  {
    ppmd.rc.dec.Stream = &s.vt;
    if (!Ppmd7z_RangeDec_Init(&ppmd.rc.dec))
      res = SZ_ERROR_DATA;
    else if (!s.extra)
    {
      UInt64 remain = outSize;
      while (remain != 0 && res == SZ_OK)
      {
        const size_t step = remain < CHUNK ? (size_t)remain : CHUNK;
        size_t got = 0;
        for (; got < step; got++)
        {
          const int sym = Ppmd7z_DecodeSymbol(&ppmd);
          if (s.extra || sym < 0)
            break;
          buf[got] = (Byte)sym;
        }
        if (got != 0)
        {
          const int r = emit(e, buf, got);
          if (r)
            res = r;
        }
        if (res == SZ_OK && got != step)
          res = SZ_ERROR_DATA;
        remain -= got;
      }
      if (res == SZ_OK && !Ppmd7z_RangeDec_IsFinishedOK(&ppmd.rc.dec))
        res = SZ_ERROR_DATA;
    }
    if (s.extra)
      res = (s.res != SZ_OK ? s.res : SZ_ERROR_DATA);
    else if (res == SZ_OK && s.processed + (size_t)(s.cur - s.begin) != inSize)
      res = SZ_ERROR_DATA;
  }
  free(buf);
  Ppmd7_Free(&ppmd, &g_alloc);
  return res;
}

static int is_main_method(UInt64 id)
{
  return id == k_Copy || id == k_LZMA || id == k_LZMA2 || id == k_PPMD;
}

/* Decode one main-method coder's packed stream (already positioned via
 * the absolute offset) through the emit stage. */
static int decode_main(Ex7z *p, const CSzCoderInfo *coder, const Byte *props_base,
    UInt64 packOffAbs, UInt64 inSize, UInt64 outSize, Emit *e)
{
  RINOK(LookInStream_SeekTo(&p->look.vt, packOffAbs))
  const Byte *props = props_base + coder->PropsOffset;
  switch ((UInt32)coder->MethodID)
  {
    case k_Copy:
      return decode_copy(&p->look.vt, inSize, outSize, e);
    case k_LZMA:
      return decode_lzma(props, coder->PropsSize, &p->look.vt, inSize, outSize, e);
    case k_LZMA2:
      return decode_lzma2(props, coder->PropsSize, &p->look.vt, inSize, outSize, e);
    case k_PPMD:
      return decode_ppmd(props, coder->PropsSize, &p->look.vt, inSize, outSize, e);
    default:
      return SZ_ERROR_UNSUPPORTED;
  }
}

/* Decode a BCJ2 side stream (call/jump — small by construction) whole
 * into memory. Copy reads raw; LZMA/LZMA2 use the one-call decoders. */
static int decode_substream(Ex7z *p, const CSzCoderInfo *coder, const Byte *props_base,
    UInt64 packOffAbs, UInt64 packSize, Byte *out, size_t outSize)
{
  RINOK(LookInStream_SeekTo(&p->look.vt, packOffAbs))
  const Byte *props = props_base + coder->PropsOffset;
  if (coder->MethodID == k_Copy)
  {
    if (packSize != outSize)
      return SZ_ERROR_DATA;
    return LookInStream_Read(&p->look.vt, out, outSize);
  }
  if (packSize > ((UInt64)1 << 31))
    return SZ_ERROR_MEM; /* absurd side stream: refuse before allocating */
  Byte *src = (Byte *)malloc(packSize != 0 ? (size_t)packSize : 1);
  if (!src)
    return SZ_ERROR_MEM;
  int res = LookInStream_Read(&p->look.vt, src, (size_t)packSize);
  if (res == SZ_OK)
  {
    SizeT destLen = outSize;
    SizeT srcLen = (SizeT)packSize;
    ELzmaStatus status;
    if (coder->MethodID == k_LZMA)
      res = LzmaDecode(out, &destLen, src, &srcLen, props, coder->PropsSize,
          LZMA_FINISH_END, &status, &g_alloc);
    else if (coder->MethodID == k_LZMA2 && coder->PropsSize == 1)
      res = Lzma2Decode(out, &destLen, src, &srcLen, props[0], LZMA_FINISH_END, &status,
          &g_alloc);
    else
      res = SZ_ERROR_UNSUPPORTED;
    if (res == SZ_OK && destLen != outSize)
      res = SZ_ERROR_DATA;
  }
  free(src);
  return res;
}

/* ---- folder driver ---- */

/* Configure the emit stage for a 2-coder folder's filter. Returns an
 * SRes error for filters even 7zDec does not know. */
static int setup_filter(Emit *e, const CSzCoderInfo *c, const Byte *props_base)
{
  const Byte *props = props_base + c->PropsOffset;
  switch ((UInt32)c->MethodID)
  {
    case k_Delta:
      if (c->PropsSize != 1)
        return SZ_ERROR_UNSUPPORTED;
      e->kind = EMIT_DELTA;
      e->delta = (unsigned)props[0] + 1;
      Delta_Init(e->delta_state);
      return SZ_OK;
    case k_BCJ:
      if (c->PropsSize != 0)
        return SZ_ERROR_UNSUPPORTED;
      e->kind = EMIT_BRANCH;
      e->is_x86 = 1;
      e->x86_state = Z7_BRANCH_CONV_ST_X86_STATE_INIT_VAL;
      return SZ_OK;
    case k_ARM64:
    case k_RISCV:
      e->kind = EMIT_BRANCH;
      e->conv = (c->MethodID == k_ARM64) ? Z7_BRANCH_CONV_DEC(ARM64) : Z7_BRANCH_CONV_DEC(RISCV);
      if (c->PropsSize == 4)
      {
        const UInt32 pc = GetUi32(props);
        if (pc & ((c->MethodID == k_ARM64) ? 3 : 1))
          return SZ_ERROR_UNSUPPORTED;
        e->pc = pc;
      }
      else if (c->PropsSize != 0)
        return SZ_ERROR_UNSUPPORTED;
      return SZ_OK;
    case k_ARM:
    case k_ARMT:
    case k_PPC:
    case k_SPARC:
    case k_IA64:
      if (c->PropsSize != 0)
        return SZ_ERROR_UNSUPPORTED;
      e->kind = EMIT_BRANCH;
      switch ((UInt32)c->MethodID)
      {
        case k_ARM: e->conv = Z7_BRANCH_CONV_DEC(ARM); break;
        case k_ARMT: e->conv = Z7_BRANCH_CONV_DEC(ARMT); break;
        case k_PPC: e->conv = Z7_BRANCH_CONV_DEC(PPC); break;
        case k_SPARC: e->conv = Z7_BRANCH_CONV_DEC(SPARC); break;
        default: e->conv = Z7_BRANCH_CONV_DEC(IA64); break;
      }
      return SZ_OK;
    default:
      return SZ_ERROR_UNSUPPORTED;
  }
}

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

  const UInt64 outSize = SzAr_GetFolderUnpackSize(ar, folderIndex);
  const UInt64 *unpackSizes = ar->CoderUnpackSizes + ar->FoToCoderUnpackSizes[folderIndex];
  const UInt64 *pp = ar->PackPositions + ar->FoStartPackStreamIndex[folderIndex];
  const UInt64 base = p->db.dataPos;

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
  e.kind = EMIT_PLAIN;

  int res;
  Byte *call_buf = NULL, *jump_buf = NULL, *rc_buf = NULL;

  if (folder.NumCoders == 1)
  {
    if (!is_main_method(folder.Coders[0].MethodID) || folder.Coders[0].NumStreams != 1
        || folder.NumPackStreams != 1 || folder.PackStreams[0] != 0 || folder.NumBonds != 0)
      return SZ_ERROR_UNSUPPORTED;
    res = decode_main(p, &folder.Coders[0], data, base + pp[0], pp[1] - pp[0], outSize, &e);
  }
  else if (folder.NumCoders == 2)
  {
    const CSzCoderInfo *c = &folder.Coders[1];
    if (!is_main_method(folder.Coders[0].MethodID) || folder.Coders[0].NumStreams != 1
        || c->NumStreams != 1 || folder.NumPackStreams != 1 || folder.PackStreams[0] != 0
        || folder.NumBonds != 1 || folder.Bonds[0].InIndex != 1
        || folder.Bonds[0].OutIndex != 0)
      return SZ_ERROR_UNSUPPORTED;
    RINOK(setup_filter(&e, c, data))
    e.scratch_cap = (e.kind == EMIT_BRANCH) ? (CHUNK + BRANCH_SLACK) : CHUNK;
    e.scratch = (Byte *)malloc(e.scratch_cap);
    if (!e.scratch)
      return SZ_ERROR_MEM;
    res = decode_main(p, &folder.Coders[0], data, base + pp[0], pp[1] - pp[0], outSize, &e);
  }
  else if (folder.NumCoders == 4)
  {
    /* The BCJ2 shape, exactly as 7zDec pins it: coders 0/1/2 are
     * main-method producers for JUMP/CALL/MAIN, coder 3 is BCJ2. */
    const CSzCoderInfo *bcj2c = &folder.Coders[3];
    if (!is_main_method(folder.Coders[0].MethodID) || folder.Coders[0].NumStreams != 1
        || !is_main_method(folder.Coders[1].MethodID) || folder.Coders[1].NumStreams != 1
        || !is_main_method(folder.Coders[2].MethodID) || folder.Coders[2].NumStreams != 1
        || bcj2c->MethodID != k_BCJ2 || bcj2c->NumStreams != 4)
      return SZ_ERROR_UNSUPPORTED;
    if (folder.NumPackStreams != 4 || folder.PackStreams[0] != 2 || folder.PackStreams[1] != 6
        || folder.PackStreams[2] != 1 || folder.PackStreams[3] != 0 || folder.NumBonds != 3
        || folder.Bonds[0].InIndex != 5 || folder.Bonds[0].OutIndex != 0
        || folder.Bonds[1].InIndex != 4 || folder.Bonds[1].OutIndex != 1
        || folder.Bonds[2].InIndex != 3 || folder.Bonds[2].OutIndex != 2)
      return SZ_ERROR_UNSUPPORTED;

    const UInt64 jump_size = unpackSizes[0];
    const UInt64 call_size = unpackSizes[1];
    const UInt64 main_size = unpackSizes[2];
    const UInt64 rc_size = pp[2] - pp[1];
    if ((jump_size & 3) != 0 || (call_size & 3) != 0
        || main_size + jump_size + call_size != outSize)
      return SZ_ERROR_DATA;
    if (jump_size > ((UInt64)1 << 31) || call_size > ((UInt64)1 << 31)
        || rc_size > ((UInt64)1 << 31))
      return SZ_ERROR_MEM;

    jump_buf = (Byte *)malloc(jump_size != 0 ? (size_t)jump_size : 1);
    call_buf = (Byte *)malloc(call_size != 0 ? (size_t)call_size : 1);
    rc_buf = (Byte *)malloc(rc_size != 0 ? (size_t)rc_size : 1);
    e.scratch = (Byte *)malloc(CHUNK);
    e.scratch_cap = CHUNK;
    res = (jump_buf && call_buf && rc_buf && e.scratch) ? SZ_OK : SZ_ERROR_MEM;

    /* Side streams first (pack slots per 7zDec: coder0→pp[3], coder1→
     * pp[2]... i.e. si = {3,2,0}; rc rides pack slot 1 raw). */
    if (res == SZ_OK)
      res = decode_substream(p, &folder.Coders[0], data, base + pp[3], pp[4] - pp[3],
          jump_buf, (size_t)jump_size);
    if (res == SZ_OK)
      res = decode_substream(p, &folder.Coders[1], data, base + pp[2], pp[3] - pp[2],
          call_buf, (size_t)call_size);
    if (res == SZ_OK)
    {
      res = LookInStream_SeekTo(&p->look.vt, base + pp[1]);
      if (res == SZ_OK)
        res = LookInStream_Read(&p->look.vt, rc_buf, (size_t)rc_size);
    }
    if (res == SZ_OK)
    {
      e.kind = EMIT_BCJ2;
      Bcj2Dec_Init(&e.bcj2);
      e.bcj2.bufs[BCJ2_STREAM_MAIN] = e.bcj2.lims[BCJ2_STREAM_MAIN] = e.scratch;
      e.bcj2.bufs[BCJ2_STREAM_CALL] = call_buf;
      e.bcj2.lims[BCJ2_STREAM_CALL] = call_buf + call_size;
      e.bcj2.bufs[BCJ2_STREAM_JUMP] = jump_buf;
      e.bcj2.lims[BCJ2_STREAM_JUMP] = jump_buf + jump_size;
      e.bcj2.bufs[BCJ2_STREAM_RC] = rc_buf;
      e.bcj2.lims[BCJ2_STREAM_RC] = rc_buf + rc_size;
      e.bcj2_dest_total = outSize;
      res = decode_main(p, &folder.Coders[2], data, base + pp[0], pp[1] - pp[0], main_size,
          &e);
    }
  }
  else
    return SZ_ERROR_UNSUPPORTED;

  if (res == SZ_OK)
    res = emit_finish(&e);
  if (res == 0)
    res = split_finish(&sp, folderIndex);
  free(e.scratch);
  free(call_buf);
  free(jump_buf);
  free(rc_buf);
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
