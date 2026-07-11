// DATBOI(D58): thin extern "C" veneer over unrar's own dll API (dll.hpp),
// consumed by the Rust guest. The veneer exists so the RARHeaderDataEx /
// RAROpenArchiveDataEx struct ABI stays entirely on the C++ side, compiled
// against the real headers — Rust sees only the flat ExHeader POD below.
//
// Interaction model: the guest drives an open -> (read-header ->
// process)* -> close loop; member bytes flow out through the
// UCM_PROCESSDATA callback into the Rust sink hook. Extraction always runs
// as RAR_TEST: unrar decodes and CRC-checks in memory and never creates
// files (the File-class write path traps, see shim.cpp).
//
// Refusal posture: password and volume callbacks answer -1 (refuse), which
// unrar turns into deterministic error codes; ErrHandler and allocation
// failure trap (vendor patches). Nothing here retries or prompts.

#include "rar.hpp"
#include "dll.hpp"

extern "C" {
void datboi_sink_write(const unsigned char *buf, size_t n);
}

// Mirrored exactly (repr(C)) in src/lib.rs — keep the two in sync.
struct ExHeader {
  unsigned long long unp_size;
  unsigned long long pack_size;
  unsigned int flags;      // RHDF_* bits
  unsigned int file_crc;   // declared CRC32 (0 unless HashType is CRC32)
  unsigned int hash_type;  // RAR_HASH_*
  unsigned int redir_type; // 0 = plain file; links/copies are skipped
  unsigned int name_len;   // in wchar units, <= 1024
  unsigned int name[1024]; // unrar wide chars (UTF-32 on wasm)
};

static int CALLBACK ExCallback(UINT msg, LPARAM, LPARAM p1, LPARAM p2) {
  switch (msg) {
    case UCM_PROCESSDATA:
      datboi_sink_write((const unsigned char *)p1, (size_t)p2);
      return 1;
    default:
      // UCM_NEEDPASSWORD(W): refuse -> ERAR_MISSING_PASSWORD.
      // UCM_CHANGEVOLUME(W): refuse -> volume error (multi-volume cut).
      // UCM_LARGEDICT: refuse -> ERAR_LARGE_DICT (cannot fit in wasm32
      // memory anyway; the wasmtime cap is the real bound).
      return -1;
  }
}

extern "C" {

// Open the (single) archive input. Returns a handle or NULL; *err carries
// the ERAR_* open result, *arc_flags the ROADF_* archive flags.
void *ex_open(unsigned int mode, unsigned int *arc_flags, int *err) {
  RAROpenArchiveDataEx data{};
  char name[] = "/archive.rar"; // resolved by shim.cpp, nothing else exists
  data.ArcName = name;
  data.OpenMode = mode;
  data.Callback = ExCallback;
  HANDLE h = RAROpenArchiveEx(&data);
  *arc_flags = data.Flags;
  *err = (int)data.OpenResult;
  return h;
}

// 0 = header filled; 1 = end of archive; other = ERAR_* error.
int ex_read_header(void *h, ExHeader *out) {
  RARHeaderDataEx hd{};
  int r = RARReadHeaderEx(h, &hd);
  if (r == ERAR_END_ARCHIVE)
    return 1;
  if (r != ERAR_SUCCESS)
    return r == 0 ? ERAR_UNKNOWN : r;
  out->unp_size = ((unsigned long long)hd.UnpSizeHigh << 32) | hd.UnpSize;
  out->pack_size = ((unsigned long long)hd.PackSizeHigh << 32) | hd.PackSize;
  out->flags = hd.Flags;
  out->hash_type = hd.HashType;
  out->file_crc = hd.HashType == RAR_HASH_CRC32 ? hd.FileCRC : 0;
  out->redir_type = hd.RedirType;
  unsigned int n = 0;
  while (n < 1024 && hd.FileNameW[n] != 0) {
    out->name[n] = (unsigned int)hd.FileNameW[n];
    n++;
  }
  out->name_len = n;
  return 0;
}

// op: RAR_SKIP (0) advances past the current member (decoding internally
// where solidity demands it); RAR_TEST (1) decodes it through the
// PROCESSDATA callback. Returns ERAR_* (0 = success).
int ex_process(void *h, int op) {
  return RARProcessFile(h, op, nullptr, nullptr);
}

int ex_close(void *h) {
  return RARCloseArchive(h);
}

} // extern "C"
