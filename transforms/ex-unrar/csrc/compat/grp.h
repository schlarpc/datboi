// DATBOI(D58): wasi-libc has no grp.h; see pwd.h.
#pragma once
#include <sys/types.h>
struct group { char *gr_name; gid_t gr_gid; };
inline struct group *getgrnam(const char *) { return 0; }
inline struct group *getgrgid(gid_t) { return 0; }
