# datboi-nfs-server

A vendored fork of [`nfsserve`](https://github.com/huggingface/nfsserve)
0.11.0 (BSD-3-Clause; see `LICENSE`, which is upstream's and is retained
unchanged along with its copyright notice). `README.upstream.md` is
upstream's README, kept for attribution.

The fork exists because of one bug that cannot be fixed from outside the
crate. Everything else here is upstream's code.

## Why we could not just implement around it

Upstream `vfs.rs`:

```rust
async fn readdir_simple(&self, dirid: fileid3, count: usize) -> Result<ReadDirSimpleResult, nfsstat3> {
    Ok(ReadDirSimpleResult::from_readdir_result(&self.readdir(dirid, 0, count).await?))
}
```

The start cookie is hardcoded `0`, and the READDIR handler in
`nfs_handlers.rs` called `readdir_simple(dirid, estimated_max_results)`
without passing `args.cookie` at all. So **every plain NFSv3 READDIR
restarts at the first entry**, whatever the client asked to resume
after. READDIRPLUS is unaffected: it routes through `readdir`, which
does receive `start_after`.

A downstream `NFSFileSystem` impl cannot correct this, because the trait
method it would override has no cookie parameter to override with. The
signature itself is the bug.

### What it looks like from the client

`ls` on a directory of ~36,000 entries never terminates. Not slowly —
never. Observed against datboi's `arcade` view:

    READDIR:     1 call, 65,384 bytes, ~1 ms RTT
    READDIRPLUS: 1 call, 65,292 bytes, ~1 ms RTT
    ls:          state R, 100% CPU, RPC counters frozen thereafter

Two RPCs, both answered in about a millisecond, then no further RPCs at
all while the process spins. Decoded off the wire:

    CALL   READDIRPLUS cookie=0    -> 451 entries,  eof=0, cookies 3..453 strictly increasing
    CALL   READDIR     cookie=129  -> 1981 entries, eof=0, first entry cookie 3

The second reply restarts at the beginning. The client fills its page
cache, reaches cookie 129, finds nothing past it, re-reads the same
cached page, and loops — issuing no new RPC, because as far as it can
tell it already holds the page it needs. `rpcdebug -m nfs -s dircache`
shows the loop directly:

    NFS: nfs_do_filldir() filling ended @ cookie 129    (x3,974)

Two non-fixes, both tested and both failures: `-o rdirplus` (this kernel
silently drops the option — it is absent from `/proc/mounts` afterwards)
and a cold server-side id table (cookies came back 3,4,5,… strictly
increasing and it spun anyway, so cookie *ordering* was never the
problem).

## The change

1. `src/vfs.rs` — `NFSFileSystem::readdir_simple` gains a
   `start_after: fileid3` parameter, and the default body forwards it to
   `readdir` instead of passing `0`.
2. `src/nfs_handlers.rs` — the READDIR handler passes `args.cookie`.

Both sites carry a comment pointing here.

One unrelated change, to keep the crate honest against this workspace's
lint bar rather than exempting it:

3. `src/vfs.rs` — the NFS generation number was a `static mut` behind a
   `Once`. It is written once and then only read, which is what
   `OnceLock` expresses without `unsafe`. Same observable behaviour, and
   the fork now contains no `unsafe` at all, so it inherits
   `[lints] workspace = true` (which warns on `unsafe_code`) with
   nothing allowed away.

## Regression coverage

`crates/datboi-server/src/nfs.rs` has
`nfs::tests::a_plain_readdir_walk_terminates`, which walks a view root
through `readdir_simple` two entries at a time and asserts every entry
is visited exactly once and the walk ends. It fails against the upstream
behaviour and passes against this fork — verified by reintroducing the
hardcoded `0` and watching it go red. The pre-existing NFS tests only
ever exercised `readdir` (the READDIRPLUS path), which is how this
survived.

## Keeping the diff legible

Everything else is upstream 0.11.0. The import landed as its own commit
with the sources semantically untouched — the READDIR bug above was
still present in it — so the behavioural diff is auditable commit by
commit:

    git log --oneline crates/datboi-nfs-server

One caveat on diffing against crates.io directly: this repo's
pre-commit hook runs `cargo fmt --all` over every workspace member and
there is no per-crate opt-out, so the import is rustfmt-normalised to
repo style. Normalise upstream the same way before comparing, or the
whitespace will bury the three real hunks.

Upstream's `demo` feature and its `intaglio` / `tracing-subscriber`
dependencies are dropped — the feature gates no source file in the
published crate, only an example we do not ship.
