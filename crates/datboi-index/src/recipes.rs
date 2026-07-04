//! Recipe index (the D21 OR-graph), verify state machine (D25), and the
//! grounding fixpoint.

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, params};

use crate::types::{OpKind, RecipeSource, SeekClass, VerifyState};
use crate::{Db, IndexError};

pub struct NewRecipe<'a> {
    /// The recipe object's own blob (meta/ namespace).
    pub blob_id: i64,
    pub op_kind: OpKind,
    /// Builtin `name@major`, or `<component-hex>#<export>` for wasm ops.
    pub op_name: &'a str,
    pub seek_class: SeekClass,
    pub source: RecipeSource,
    /// (position, input blob, role)
    pub inputs: &'a [(u32, i64, Option<&'a str>)],
    /// (ordinal, output blob, claimed size, name)
    pub outputs: &'a [(u32, i64, u64, Option<&'a str>)],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeRow {
    pub recipe_id: i64,
    pub blob_id: i64,
    pub op_kind: OpKind,
    pub op_name: String,
    pub seek_class: SeekClass,
    pub verify: VerifyState,
    pub source: RecipeSource,
}

impl Db {
    pub fn insert_recipe(&mut self, new: &NewRecipe<'_>) -> Result<i64, IndexError> {
        let tx = self.cache.transaction()?;
        tx.execute(
            "INSERT INTO recipe (blob_id, op_kind, op_name, seek_class, verify, source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                new.blob_id,
                new.op_kind.code(),
                new.op_name,
                new.seek_class.code(),
                VerifyState::Pending.code(),
                new.source.code()
            ],
        )?;
        let recipe_id = tx.last_insert_rowid();
        {
            let mut input = tx.prepare_cached(
                "INSERT INTO recipe_input (recipe_id, position, blob_id, role)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (position, blob_id, role) in new.inputs {
                input.execute(params![recipe_id, position, blob_id, role])?;
            }
            let mut output = tx.prepare_cached(
                "INSERT INTO recipe_output (recipe_id, ordinal, blob_id, size, name)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for (ordinal, blob_id, size, name) in new.outputs {
                output.execute(params![
                    recipe_id,
                    ordinal,
                    blob_id,
                    i64::try_from(*size).expect("size fits i64"),
                    name
                ])?;
            }
        }
        tx.commit()?;
        Ok(recipe_id)
    }

    /// All recipes claiming to produce `blob_id` — the OR-graph entry
    /// point (D21).
    pub fn recipes_for_output(&self, blob_id: i64) -> Result<Vec<RecipeRow>, IndexError> {
        let mut stmt = self.cache().prepare_cached(
            "SELECT r.recipe_id, r.blob_id, r.op_kind, r.op_name, r.seek_class, r.verify, r.source
             FROM recipe_output ro JOIN recipe r ON r.recipe_id = ro.recipe_id
             WHERE ro.blob_id = ?1
             ORDER BY r.recipe_id",
        )?;
        let rows = stmt.query_map(params![blob_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (recipe_id, blob_id, op_kind, op_name, seek_class, verify, source) = row?;
            out.push(RecipeRow {
                recipe_id,
                blob_id,
                op_kind: OpKind::from_code(op_kind)?,
                op_name,
                seek_class: SeekClass::from_code(seek_class)?,
                verify: VerifyState::from_code(verify)?,
                source: RecipeSource::from_code(source)?,
            });
        }
        Ok(out)
    }

    /// Advance the verify state machine. Illegal transitions (including
    /// anything out of the `Failed` poison state) are rejected (D25).
    /// `Failed` requires an error message; the peer that supplied a
    /// failing claim may be recorded for reputation (D8).
    pub fn set_verify_state(
        &self,
        recipe_id: i64,
        to: VerifyState,
        at_unix: i64,
        failure: Option<(&str, Option<&[u8]>)>,
    ) -> Result<(), IndexError> {
        assert_eq!(
            to == VerifyState::Failed,
            failure.is_some(),
            "failure detail iff transitioning to Failed"
        );
        let from = self
            .cache()
            .query_row(
                "SELECT verify FROM recipe WHERE recipe_id = ?1",
                params![recipe_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .ok_or(IndexError::RecipeNotFound(recipe_id))?;
        let from = VerifyState::from_code(from)?;
        if !from.can_transition_to(to) {
            return Err(IndexError::IllegalTransition { from, to });
        }
        let (fail_error, fail_peer) = failure.unzip();
        self.cache().execute(
            "UPDATE recipe SET verify = ?2, verified_at = ?3, fail_error = ?4, fail_peer = ?5
             WHERE recipe_id = ?1",
            params![
                recipe_id,
                to.code(),
                at_unix,
                fail_error,
                fail_peer.flatten()
            ],
        )?;
        Ok(())
    }

    /// The D21 grounding fixpoint: blobs reconstructible from retained
    /// literals through replayed-local recipes only. Application-driven
    /// loop of set-based rounds (∀-inputs-grounded is not a monotone
    /// recursive CTE); converges in ≤ DAG depth rounds.
    pub fn grounded_set(&self) -> Result<HashSet<i64>, IndexError> {
        grounded(self.cache(), GroundingMode::Eviction, None)
    }

    /// Grounding under a chosen recipe-trust rule. `Eviction` is the D25
    /// drop-safety rule (replayed-local only); the two audit modes serve
    /// completeness reporting, where D4's verify-on-ingest already makes
    /// locally-minted claims verified-grade knowledge without licensing
    /// any literal drop.
    pub fn grounded_set_with(&self, mode: GroundingMode) -> Result<HashSet<i64>, IndexError> {
        grounded(self.cache(), mode, None)
    }

    /// Would `blob_id` remain grounded if its literal were dropped?
    /// (M1 never evicts; the primitive lives with the schema so the D21
    /// semantics are pinned by tests from day one.) The D27 opaque/pinned
    /// rule layers on top of this when the residency planner lands.
    /// Always uses the `Eviction` rule (D25).
    pub fn is_evictable(&self, blob_id: i64) -> Result<bool, IndexError> {
        Ok(grounded(self.cache(), GroundingMode::Eviction, Some(blob_id))?.contains(&blob_id))
    }
}

/// Which recipes may carry grounding (D21) for a given question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroundingMode {
    /// Drop-safety (D25): replayed-local recipes only.
    Eviction,
    /// Audit "have(verified)": replayed-local, plus Verified recipes minted
    /// by local ingest — we hashed the real bytes ourselves (D4).
    AuditVerified,
    /// Audit "have(claimed)": any non-failed verified-grade claim,
    /// regardless of source (peer claims count; D4 language).
    AuditClaimed,
}

impl GroundingMode {
    /// SQL predicate over `recipe r` selecting recipes that ground.
    fn recipe_condition(self) -> &'static str {
        match self {
            Self::Eviction => "r.verify = 3",
            Self::AuditVerified => "(r.verify = 3 OR (r.verify = 1 AND r.source = 0))",
            Self::AuditClaimed => "r.verify IN (1, 3)",
        }
    }
}

fn grounded(
    conn: &Connection,
    mode: GroundingMode,
    without_literal: Option<i64>,
) -> Result<HashSet<i64>, IndexError> {
    conn.execute_batch(
        "DROP TABLE IF EXISTS temp.grounded;
         CREATE TEMP TABLE grounded (blob_id INTEGER PRIMARY KEY);",
    )?;
    conn.execute(
        "INSERT INTO temp.grounded
         SELECT blob_id FROM blob
         WHERE residency = 0 AND (?1 IS NULL OR blob_id <> ?1)",
        params![without_literal],
    )?;
    loop {
        let added = conn.execute(
            &format!(
                "INSERT OR IGNORE INTO temp.grounded
                 SELECT ro.blob_id
                 FROM recipe r JOIN recipe_output ro ON ro.recipe_id = r.recipe_id
                 WHERE {}
                   AND NOT EXISTS (
                     SELECT 1 FROM recipe_input ri
                     WHERE ri.recipe_id = r.recipe_id
                       AND ri.blob_id NOT IN (SELECT blob_id FROM temp.grounded))",
                mode.recipe_condition()
            ),
            [],
        )?;
        if added == 0 {
            break;
        }
    }
    let mut stmt = conn.prepare("SELECT blob_id FROM temp.grounded")?;
    let set = stmt
        .query_map([], |row| row.get(0))?
        .collect::<Result<HashSet<i64>, _>>()?;
    conn.execute_batch("DROP TABLE temp.grounded;")?;
    Ok(set)
}
