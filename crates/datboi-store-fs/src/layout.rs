//! On-disk path layout (D19/D20).

use std::path::PathBuf;

use datboi_core::hash::Blake3;

/// Store namespaces (D20): opaque payloads vs datboi structured objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Namespace {
    Data,
    Meta,
}

impl Namespace {
    #[must_use]
    pub fn dir(self) -> &'static str {
        match self {
            Self::Data => "data",
            Self::Meta => "meta",
        }
    }
}

/// Relative path of a blob's data file within the store root.
///
/// Fanout is 2 levels × 256 pending the M1 benchmark (see crate docs).
#[must_use]
pub fn blob_path(ns: Namespace, hash: &Blake3) -> PathBuf {
    let hex = hash.to_hex();
    let mut p = PathBuf::from(ns.dir());
    p.push(&hex[0..2]);
    p.push(&hex[2..4]);
    p.push(format!("{hex}.data"));
    p
}

/// Relative path of a blob's bao outboard sidecar, when one exists.
///
/// Extension is `.obao4`, not `.obao`: the trailing `4` is iroh-blobs'
/// convention for a chunk-group size of 2^4 = 16 KiB (D52). Bare `.obao`
/// denotes the standard bao format's finer 1 KiB granularity — a
/// different tree — so naming our 16 KiB-group sidecar `.obao` would be
/// an active misnomer (D52 amendment).
#[must_use]
pub fn outboard_path(ns: Namespace, hash: &Blake3) -> PathBuf {
    let mut p = blob_path(ns, hash);
    p.set_extension("obao4");
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_sharded_and_namespaced() {
        let h = Blake3::compute(b"x");
        let hex = h.to_hex();
        let p = blob_path(Namespace::Data, &h);
        let expected = PathBuf::from(format!("data/{}/{}/{hex}.data", &hex[0..2], &hex[2..4]));
        assert_eq!(p, expected);
        assert_eq!(
            outboard_path(Namespace::Meta, &h).extension().unwrap(),
            "obao4"
        );
    }
}
