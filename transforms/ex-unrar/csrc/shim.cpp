// DATBOI(D58): determinism shim for the freestanding wasm build.
//
// The component must import NOTHING (D5/D46). wasi-libc's syscall wrappers
// bottom out in wasi_snapshot_preview1 imports, so every libc symbol unrar
// can reach is defined HERE first — the wasi-libc archive members that
// would import never get pulled. Three classes:
//
//  * archive input  — open/read/lseek/... reroute onto the guest's stream
//    hooks (the WIT `file` resource, host-implemented);
//  * inert          — clock/env/fs-metadata queries answer with fixed,
//    deterministic values (epoch 0, no env, no other files);
//  * unreachable    — anything only a real filesystem-writing extraction
//    would run traps (`unreachable`), refusing the whole archive. Trap,
//    never silent success: a silently "working" write would let a code
//    path diverge from native unrar semantics unnoticed.

#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

#include <stdio.h>
#include <__verbose_abort>

extern "C" {
// Guest-side hooks (implemented in Rust over the WIT resources).
unsigned long long datboi_input_len(void);
size_t datboi_input_read_at(unsigned long long off, unsigned char *buf, size_t n);

// The one trap door (also referenced by the DATBOI patches in vendor/).
[[noreturn]] void datboi_wasm_trap(void) { __builtin_trap(); }
}

// ---- C++ runtime diagnostics: print-then-abort becomes plain trap ------
// (the printers would drag vfprintf/stderr -> wasi fd_* imports in).

_LIBCPP_BEGIN_NAMESPACE_STD
[[__noreturn__]] _LIBCPP_EXPORTED_FROM_ABI void __libcpp_verbose_abort(const char *, ...) noexcept {
  __builtin_trap();
}
_LIBCPP_END_NAMESPACE_STD

extern "C" {

void abort(void) { __builtin_trap(); }
// libc++abi's terminate-handler diagnostic printer.
[[noreturn]] void __abort_message(const char *, ...) { __builtin_trap(); }
// Belt for -D_WASI_EMULATED_SIGNAL without its emulation library.
int raise(int) { __builtin_trap(); }

// ---- archive input: the only file that exists ---------------------------
//
// unrar's File class (FILE_USE_OPEN) drives POSIX fds. Exactly one path
// resolves — the dummy name the glue opens the container under — and every
// fd is an independent cursor over the same host-provided input resource
// (qopen re-opens the archive; volumes never happen, they are refused).

#define EX_ARCHIVE_PATH "/archive.rar"
#define EX_FD_BASE 3
#define EX_FD_MAX 16

static long long ex_pos[EX_FD_MAX];
static bool ex_used[EX_FD_MAX];

char ex_dbg_lastpath[512];
int ex_dbg_openflags;
int ex_dbg_opencalls;

int open(const char *path, int flags, ...) {
  ex_dbg_opencalls++;
  ex_dbg_openflags = flags;
  if (path != nullptr) {
    size_t i = 0;
    for (; i < 511 && path[i]; i++)
      ex_dbg_lastpath[i] = path[i];
    ex_dbg_lastpath[i] = 0;
  }
  if (path == nullptr || strcmp(path, EX_ARCHIVE_PATH) != 0) {
    errno = ENOENT;
    return -1;
  }
  // Refuse any write intent. NOTE: on wasi O_RDONLY is a NONZERO bit
  // pattern, so masking with O_RDWR (== O_RDONLY|O_WRONLY) would false-
  // positive on a plain read — test O_WRONLY / create / truncate bits only.
  if ((flags & (O_WRONLY | O_CREAT | O_TRUNC | O_APPEND)) != 0) {
    errno = EACCES; // read-only world
    return -1;
  }
  for (int fd = EX_FD_BASE; fd < EX_FD_MAX; fd++)
    if (!ex_used[fd]) {
      ex_used[fd] = true;
      ex_pos[fd] = 0;
      return fd;
    }
  errno = EMFILE;
  return -1;
}

static bool ex_valid(int fd) { return fd >= EX_FD_BASE && fd < EX_FD_MAX && ex_used[fd]; }

int close(int fd) {
  if (!ex_valid(fd)) {
    errno = EBADF;
    return -1;
  }
  ex_used[fd] = false;
  return 0;
}

ssize_t read(int fd, void *buf, size_t n) {
  if (!ex_valid(fd)) {
    errno = EBADF;
    return -1;
  }
  unsigned long long len = datboi_input_len();
  unsigned long long pos = (unsigned long long)ex_pos[fd];
  if (pos >= len || n == 0)
    return 0;
  if (n > len - pos)
    n = (size_t)(len - pos);
  size_t got = datboi_input_read_at(pos, (unsigned char *)buf, n);
  ex_pos[fd] += (long long)got;
  return (ssize_t)got;
}

off_t lseek(int fd, off_t off, int whence) {
  if (!ex_valid(fd)) {
    errno = EBADF;
    return -1;
  }
  long long base;
  switch (whence) {
    case SEEK_SET: base = 0; break;
    case SEEK_CUR: base = ex_pos[fd]; break;
    case SEEK_END: base = (long long)datboi_input_len(); break;
    default: errno = EINVAL; return -1;
  }
  long long next = base + (long long)off;
  if (next < 0) {
    errno = EINVAL;
    return -1;
  }
  ex_pos[fd] = next;
  return (off_t)next;
}

int fstat(int fd, struct stat *st) {
  if (!ex_valid(fd)) {
    errno = EBADF;
    return -1;
  }
  memset(st, 0, sizeof(*st));
  st->st_size = (off_t)datboi_input_len();
  st->st_mode = S_IFREG | 0444;
  st->st_nlink = 1;
  return 0;
}

// No other file exists, deterministically (volume probing, FindFile).
int stat(const char *, struct stat *) {
  errno = ENOENT;
  return -1;
}
int lstat(const char *, struct stat *) {
  errno = ENOENT;
  return -1;
}
int access(const char *, int) {
  errno = ENOENT;
  return -1;
}

// Write-side file ops: only a real filesystem extraction reaches these.
ssize_t write(int, const void *, size_t) { __builtin_trap(); }
int ftruncate(int, off_t) { __builtin_trap(); }
int fsync(int) { return 0; } // File::Close may flush read handles; inert
int unlink(const char *) { __builtin_trap(); }
int remove(const char *) { __builtin_trap(); }
int rename(const char *, const char *) { __builtin_trap(); }
int mkdir(const char *, mode_t) { __builtin_trap(); }
int rmdir(const char *) { __builtin_trap(); }
int chmod(const char *, mode_t) { __builtin_trap(); }
int chdir(const char *) { __builtin_trap(); }
int link(const char *, const char *) { __builtin_trap(); }
int symlink(const char *, const char *) { __builtin_trap(); }
int utime(const char *, const void *) { __builtin_trap(); }

// stdio: referenced by crypt.cpp's /dev/urandom reader (a salt-generation
// ENCODE path unrar's decoder never runs) and consio's console globals
// (SILENT compiles the actual output out, but the object still names them).
//
// Rather than redefine wasi-libc's stream globals (which duplicate-conflicts
// with libc.a in the final link), we let libc keep `stdin`/`stdout`/`stderr`
// and instead TRAP the low-level stdio BACKENDS those streams bottom out in.
// wasi-libc's `__stdio_{read,write,seek,close}` and `__stdout_write` are the
// only things that would reach the fd_* imports; overriding them here breaks
// the chain, so no import survives and nothing conflicts. None is ever
// called at runtime (no FILE opens; SILENT means no console writes).
FILE *fopen(const char *, const char *) { return nullptr; }
size_t fread(void *, size_t, size_t, FILE *) { __builtin_trap(); }
int fclose(FILE *) { __builtin_trap(); }
using off_t_stdio = long long;
size_t __stdio_read(void *, unsigned char *, size_t) { __builtin_trap(); }
size_t __stdio_write(void *, const unsigned char *, size_t) { __builtin_trap(); }
size_t __stdout_write(void *, const unsigned char *, size_t) { __builtin_trap(); }
off_t_stdio __stdio_seek(void *, off_t_stdio, int) { __builtin_trap(); }
int __stdio_close(void *) { __builtin_trap(); }

// Directory scans and fs queries: an empty, unknowable filesystem.
// (DIR is opaque; nothing ever gets a non-null one, so readdir/closedir
// can never be reached with a valid handle — trap keeps them honest.)
struct __dirstream;
struct __dirstream *opendir(const char *) { return nullptr; }
void *readdir(struct __dirstream *) { __builtin_trap(); }
int closedir(struct __dirstream *) { __builtin_trap(); }
int statvfs(const char *, void *) {
  errno = ENOENT;
  return -1;
}
char *getcwd(char *buf, size_t size) {
  if (buf != nullptr && size >= 2) {
    buf[0] = '/';
    buf[1] = 0;
    return buf;
  }
  errno = ERANGE;
  return nullptr;
}

// ---- ambient world: fixed answers ---------------------------------------

char *getenv(const char *) { return nullptr; }
pid_t getpid(void) { return 1; } // secpassword's xor key seed: fixed
uid_t getuid(void) { return 0; }
gid_t getgid(void) { return 0; }
int isatty(int) { return 0; }
long sysconf(int) {
  errno = EINVAL;
  return -1;
}
unsigned int sleep(unsigned int) { return 0; }
int usleep(unsigned int) { return 0; }
int nanosleep(const struct timespec *, struct timespec *) { return 0; }

// Epoch zero, always (metadata timestamps come from headers, never here).
time_t time(time_t *t) {
  if (t != nullptr)
    *t = 0;
  return 0;
}
int clock_gettime(clockid_t, struct timespec *ts) {
  if (ts != nullptr) {
    ts->tv_sec = 0;
    ts->tv_nsec = 0;
  }
  return 0;
}
int gettimeofday(struct timeval *tv, void *) {
  if (tv != nullptr) {
    tv->tv_sec = 0;
    tv->tv_usec = 0;
  }
  return 0;
}

// Pure UTC calendar conversion (no TZ database, no env): enough for the
// header-time formatting paths; deterministic everywhere.
static struct tm *ex_gmtime(const time_t *t, struct tm *out) {
  memset(out, 0, sizeof(*out));
  long long secs = (t != nullptr) ? (long long)*t : 0;
  long long days = secs / 86400;
  long long rem = secs % 86400;
  if (rem < 0) {
    rem += 86400;
    days -= 1;
  }
  out->tm_sec = (int)(rem % 60);
  out->tm_min = (int)((rem / 60) % 60);
  out->tm_hour = (int)(rem / 3600);
  out->tm_wday = (int)((days + 4) % 7 + ((days + 4) % 7 < 0 ? 7 : 0));
  long long year = 1970;
  auto leap = [](long long y) { return (y % 4 == 0 && y % 100 != 0) || y % 400 == 0; };
  while (true) {
    long long ylen = leap(year) ? 366 : 365;
    if (days >= ylen) {
      days -= ylen;
      year++;
    } else if (days < 0) {
      year--;
      days += leap(year) ? 366 : 365;
    } else
      break;
  }
  out->tm_year = (int)(year - 1900);
  out->tm_yday = (int)days;
  static const int mlen[12] = {31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31};
  int month = 0;
  while (true) {
    int ml = mlen[month] + (month == 1 && leap(year) ? 1 : 0);
    if (days < ml)
      break;
    days -= ml;
    month++;
  }
  out->tm_mon = month;
  out->tm_mday = (int)days + 1;
  return out;
}

struct tm *gmtime_r(const time_t *t, struct tm *out) { return ex_gmtime(t, out); }
struct tm *localtime_r(const time_t *t, struct tm *out) { return ex_gmtime(t, out); }
struct tm *gmtime(const time_t *t) {
  static struct tm shared;
  return ex_gmtime(t, &shared);
}
struct tm *localtime(const time_t *t) {
  static struct tm shared;
  return ex_gmtime(t, &shared);
}
time_t mktime(struct tm *tm) {
  // Inverse of ex_gmtime, UTC only. Used by timefn's calendar round-trips.
  auto leap = [](long long y) { return (y % 4 == 0 && y % 100 != 0) || y % 400 == 0; };
  static const int mdays[12] = {0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334};
  long long year = 1900 + tm->tm_year;
  long long days = 0;
  if (year >= 1970)
    for (long long y = 1970; y < year; y++)
      days += leap(y) ? 366 : 365;
  else
    for (long long y = year; y < 1970; y++)
      days -= leap(y) ? 366 : 365;
  days += mdays[tm->tm_mon % 12] + (tm->tm_mon > 1 && leap(year) ? 1 : 0) + tm->tm_mday - 1;
  return (time_t)(days * 86400 + tm->tm_hour * 3600 + tm->tm_min * 60 + tm->tm_sec);
}

} // extern "C"
