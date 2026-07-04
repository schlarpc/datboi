# CLI surface (draft)

*Draft for M1; grows per milestone. Principles: verbs are resources,
output is human tables by default / `--json` everywhere, exit codes are
meaningful (audit: 0 complete, 1 incomplete, 2 error), 12-factor config
via `DATBOI_*` env with flags overriding.*

## M1 command tree

```
datboi serve                         # run the daemon (localhost/unix socket)

datboi ingest <path>...              # hash + claim content into the store
    --move | --copy | --reference    # what happens to source files (default: reference)
    --rescan                         # force full rescan (ignore O(changed) cache)

datboi dat import <file|url>         # manual drop (No-Intro daily pack, D16)
datboi dat fetch [<provider>]        # polite auto-fetch (Redump, MAME, libretro…)
datboi dat list                      # sources, current revisions, freshness
datboi dat diff <source> [<rev>..<rev>]   # added/removed/renamed/rehashed

datboi audit <source>...             # have(verified)/claimed/probable/peer/missing/unknown
    --missing | --unknown            # filter reports
datboi export dat <source> -o x.dat  # dir2dat (D29)

datboi recover                       # rebuild local DBs from the store (D15)
datboi scrub [--sample <pct>]        # background verification pass
datboi status                        # store stats, snapshot age, last scrub

datboi snapshot [--now]              # force a state snapshot (D15)
```

## Later milestones (sketch)

- M2: `datboi gc plan|run`, `datboi aggregate`, `datboi convert` (in-store
  representation), `datboi verify <recipe>`.
- M3: `datboi view create|eval|serve|sync|image`, `datboi select`
  (1G1R dry-runs).
- M4: `datboi user invite|list`, `datboi token`.
- M5: `datboi peer add|list`, `datboi channel publish|subscribe`,
  `datboi fetch --from-peer`.
