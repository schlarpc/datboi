//! In-process userspace NFSv3 server.
//!
//! A vendored fork of nfsserve 0.11.0 (huggingface/nfsserve,
//! BSD-3-Clause). See FORK.md for why it is vendored and exactly what
//! differs from upstream — the short version is that plain NFSv3 READDIR
//! discarded the client's cookie, which no downstream implementation
//! could correct because the trait method took no cookie parameter.
//!
//! Upstream's `strict` feature (which set `deny(warnings)`) is dropped
//! along with `demo`; neither gated any shipped source.

mod context;
mod rpc;
mod rpcwire;
mod write_counter;
pub mod xdr;

mod mount;
mod mount_handlers;

mod portmap;
mod portmap_handlers;

pub mod nfs;
mod nfs_handlers;

#[cfg(not(target_os = "windows"))]
pub mod fs_util;

pub mod tcp;
mod transaction_tracker;
pub mod vfs;
