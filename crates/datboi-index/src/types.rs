//! Column-value vocabularies. Numeric codes are part of the on-disk
//! schemas (cache.db and state.db alike); they may be extended but never
//! renumbered within a `SCHEMA_VERSION`.

use crate::IndexError;

macro_rules! db_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident = $code:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $($(#[$vmeta])* $variant = $code),+
        }

        impl $name {
            #[must_use]
            pub fn code(self) -> i64 {
                self as i64
            }

            pub fn from_code(code: i64) -> Result<Self, IndexError> {
                match code {
                    $($code => Ok(Self::$variant),)+
                    _ => Err(IndexError::Decode {
                        what: stringify!($name),
                        code,
                    }),
                }
            }
        }
    };
}

db_enum! {
    /// Store namespace (D20).
    Namespace {
        Data = 0,
        Meta = 1,
    }
}

db_enum! {
    /// Whether literal bytes exist locally.
    Residency {
        Resident = 0,
        /// Literal dropped; covered by a replayed-local recipe (D25).
        EvictedCovered = 1,
        /// Known hash, no local bytes (peer-advertised, missing, …).
        Absent = 2,
    }
}

db_enum! {
    /// Recipe operation kind (docs/70-recipes.md).
    OpKind {
        Builtin = 0,
        Wasm = 1,
    }
}

db_enum! {
    /// Seekability class (D27, docs/80-views.md).
    SeekClass {
        Affine = 0,
        ManifestSeekable = 1,
        Opaque = 2,
    }
}

db_enum! {
    /// Recipe verification state machine (D4/D25). `Failed` is terminal
    /// poison; only `ReplayedLocal` licenses dropping literals.
    VerifyState {
        Pending = 0,
        Verified = 1,
        Failed = 2,
        ReplayedLocal = 3,
    }
}

db_enum! {
    /// Where a recipe claim came from.
    RecipeSource {
        LocalIngest = 0,
        Peer = 1,
        Compaction = 2,
    }
}

db_enum! {
    /// Alias hash algorithm (D22). blake3 is never an alias — it is the key.
    /// `ChdSha1` is a separate namespace on purpose: it records what a CHD
    /// v5 *header declares* its internal sha1 to be — an attestation about
    /// decompressed content, not a hash of the blob's bytes — so it must
    /// never answer a real sha1 lookup (D44: declared evidence caps at
    /// `probable`).
    AliasAlgo {
        Crc32 = 1,
        Md5 = 2,
        Sha1 = 3,
        Sha256 = 4,
        ChdSha1 = 5,
    }
}

db_enum! {
    /// Account role (D30/D68): owners see everything; friends see
    /// exactly their granted views. Invites carry the role they mint.
    Role {
        Owner = 0,
        Friend = 1,
    }
}

db_enum! {
    /// rom_claim kind (60-dats: disk = CHD internal sha1, no size).
    ClaimKind {
        Rom = 0,
        Disk = 1,
        Sample = 2,
    }
}

db_enum! {
    /// Logiqx dump status.
    ClaimStatus {
        Good = 0,
        BadDump = 1,
        NoDump = 2,
        Verified = 3,
    }
}

db_enum! {
    /// Job ledger kind (D74, state.db `job.kind`). ONE definition for
    /// both writers: the daemon maps the wire enum here, the CLI's
    /// ledger_stamp names these directly.
    JobKind {
        Ingest = 0,
        Refine = 1,
        Gc = 2,
        Scrub = 3,
    }
}

db_enum! {
    /// Job ledger state (D74, state.db `job.state`): the wire
    /// vocabulary plus crash evidence.
    JobState {
        Running = 0,
        Done = 1,
        Failed = 2,
        /// Still `running` when a daemon started: the process died
        /// under it.
        Interrupted = 3,
    }
}

impl VerifyState {
    /// Legal transitions: Pending→{Verified, ReplayedLocal, Failed},
    /// Verified→{ReplayedLocal, Failed}, ReplayedLocal→Failed (late
    /// nondeterminism found by scrub — alarm-level, docs/70-recipes.md).
    /// Failed is terminal; downgrades and self-transitions are illegal.
    #[must_use]
    pub fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (
                Self::Pending,
                Self::Verified | Self::ReplayedLocal | Self::Failed
            ) | (Self::Verified, Self::ReplayedLocal | Self::Failed)
                | (Self::ReplayedLocal, Self::Failed)
        )
    }
}
