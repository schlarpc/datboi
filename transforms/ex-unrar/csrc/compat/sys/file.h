// DATBOI(D58): wasi-libc has no sys/file.h; flock is a no-op (single deterministic reader).
#pragma once
#define LOCK_SH 1
#define LOCK_EX 2
#define LOCK_NB 4
#define LOCK_UN 8
inline int flock(int, int) { return 0; }
