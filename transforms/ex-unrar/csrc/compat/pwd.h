// DATBOI(D58): wasi-libc has no pwd.h; unrar only touches it on owner-restore paths we never run.
#pragma once
#include <sys/types.h>
struct passwd { char *pw_name; uid_t pw_uid; gid_t pw_gid; };
inline struct passwd *getpwnam(const char *) { return 0; }
inline struct passwd *getpwuid(uid_t) { return 0; }
