//! The 4-state entry vocabulary (docs/web-ui.md §7), the UI-facing
//! projection of the D39 six-state rollup. This is the ONE home for the
//! threshold rule (D96: one code path per concept). Both the SQL-side
//! aggregation/filter (`STATE_CASE_SQL`) and the Rust-side classifier
//! (`RollupState::classify`) live here and are proven equivalent by the
//! `sql_matches_rust` test, so the mapping cannot drift between the
//! daemon's query strings and any Rust caller.
//!
//! Distinct from `audit::AuditReport`, which renders the full six-state
//! contract (probable/peer split out). The four states fold probable and
//! peer-available into `missing` — they are not holdings — and treat a
//! zero-required entry as `nodump` (forcenodump: excluded from
//! completeness math). See D39 for the rollup semantics this derives from.

/// The UI's per-entry state. Discriminants ARE the wire/query codes —
/// the SQL `CASE` below emits these same integers, so ordering here is
/// load-bearing (the `sql_matches_rust` test is the fence).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RollupState {
    /// Every required claim is grounded-verified.
    Verified = 0,
    /// The remainder is covered by verified-grade claims (held, not
    /// byte-present-verified).
    Claimed = 1,
    /// Short of claimed — probable/peer fold in here; they are not
    /// holdings.
    Missing = 2,
    /// No satisfiable required claims at all (forcenodump semantics).
    Nodump = 3,
}

impl RollupState {
    /// Classify one `entry_audit` row. `required == 0` (or a NULL folded
    /// to 0 by the caller) is `Nodump`. Mirrors `STATE_CASE_SQL` exactly.
    #[must_use]
    pub fn classify(required: u64, have_verified: u64, have_claimed: u64) -> Self {
        if required == 0 {
            RollupState::Nodump
        } else if have_verified >= required {
            RollupState::Verified
        } else if have_verified + have_claimed >= required {
            RollupState::Claimed
        } else {
            RollupState::Missing
        }
    }

    #[must_use]
    pub fn code(self) -> i64 {
        self as i64
    }

    /// The query-string / filter name, e.g. `?state=verified`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            RollupState::Verified => "verified",
            RollupState::Claimed => "claimed",
            RollupState::Missing => "missing",
            RollupState::Nodump => "nodump",
        }
    }

    /// Parse a filter name back to a state (`None` = unknown token).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "verified" => Some(RollupState::Verified),
            "claimed" => Some(RollupState::Claimed),
            "missing" => Some(RollupState::Missing),
            "nodump" => Some(RollupState::Nodump),
            _ => None,
        }
    }

    /// Map a raw SQL/wire code back to a state, saturating unknowns to
    /// `Nodump` (the pre-D96 server `entry_state` fallback behaviour).
    #[must_use]
    pub fn from_code(code: i64) -> Self {
        match code {
            0 => RollupState::Verified,
            1 => RollupState::Claimed,
            2 => RollupState::Missing,
            _ => RollupState::Nodump,
        }
    }
}

/// The 4-state projection as a SQL `CASE` expression. Assumes the
/// `entry_audit` row is joined and aliased `ea` (a LEFT JOIN — a missing
/// rollup row reads as all-NULL, hence the leading NULL guard → nodump).
/// Emits the same integer codes as [`RollupState`]. Proven equivalent to
/// `RollupState::classify` by the `sql_matches_rust` test.
pub const STATE_CASE_SQL: &str = "CASE \
     WHEN ea.required IS NULL OR ea.required = 0 THEN 3 \
     WHEN ea.have_verified >= ea.required THEN 0 \
     WHEN ea.have_verified + ea.have_claimed >= ea.required THEN 1 \
     ELSE 2 END";

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// The load-bearing guard: evaluate `STATE_CASE_SQL` in sqlite over a
    /// grid of rollup rows and assert it agrees with `classify` for every
    /// one. If either definition drifts, this fails.
    #[test]
    fn sql_matches_rust() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE ea (required INTEGER, have_verified INTEGER, have_claimed INTEGER)",
            [],
        )
        .unwrap();
        let sql = format!("SELECT {STATE_CASE_SQL} FROM ea");
        for required in 0u64..4 {
            for have_verified in 0u64..4 {
                for have_claimed in 0u64..4 {
                    conn.execute("DELETE FROM ea", []).unwrap();
                    conn.execute(
                        "INSERT INTO ea VALUES (?1, ?2, ?3)",
                        rusqlite::params![required, have_verified, have_claimed],
                    )
                    .unwrap();
                    let sql_code: i64 = conn.query_row(&sql, [], |r| r.get(0)).unwrap();
                    let rust = RollupState::classify(required, have_verified, have_claimed);
                    assert_eq!(
                        sql_code,
                        rust.code(),
                        "mismatch at required={required} verified={have_verified} claimed={have_claimed}"
                    );
                }
            }
        }
    }

    /// A NULL rollup row (LEFT JOIN miss) must read as nodump.
    #[test]
    fn null_rollup_is_nodump() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE ea (required INTEGER, have_verified INTEGER, have_claimed INTEGER)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO ea VALUES (NULL, NULL, NULL)", [])
            .unwrap();
        let sql = format!("SELECT {STATE_CASE_SQL} FROM ea");
        let sql_code: i64 = conn.query_row(&sql, [], |r| r.get(0)).unwrap();
        assert_eq!(sql_code, RollupState::Nodump.code());
    }

    #[test]
    fn name_roundtrips() {
        for s in [
            RollupState::Verified,
            RollupState::Claimed,
            RollupState::Missing,
            RollupState::Nodump,
        ] {
            assert_eq!(RollupState::from_name(s.as_str()), Some(s));
            assert_eq!(RollupState::from_code(s.code()), s);
        }
        assert_eq!(RollupState::from_name("bogus"), None);
    }
}
