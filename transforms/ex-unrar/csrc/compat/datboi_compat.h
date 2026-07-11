// DATBOI(D58): force-included into every unrar TU (-include). Declares the
// handful of POSIX calls wasi-libc lacks. All are on paths test-mode
// extraction never runs (stdout streaming, dir-attr restore, owner
// restore); stubs are inert, not reroutes.
#pragma once
#include <sys/types.h>
#include <sys/stat.h>

static inline int dup(int) { return -1; }
static inline mode_t umask(mode_t) { return 022; }
static inline int lchown(const char *, uid_t, gid_t) { return 0; }
