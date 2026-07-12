//! Recipe index (the D21 OR-graph), verify state machine (D25), and the
//! grounding fixpoint.

use std::collections::HashSet;

use datboi_core::hash::Blake3;
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

/// One input of a recipe, joined with its blob row — the explainability
/// projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeInputRow {
    pub position: u32,
    pub blob_id: i64,
    pub hash: Blake3,
    pub residency: crate::Residency,
    pub role: Option<String>,
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

    /// A recipe's inputs with each blob's hash, residency, and role —
    /// the explainability projection ("why won't this route license?").
    pub fn recipe_inputs(&self, recipe_id: i64) -> Result<Vec<RecipeInputRow>, IndexError> {
        let mut stmt = self.cache().prepare_cached(
            "SELECT ri.position, ri.blob_id, b.hash, b.residency, ri.role
             FROM recipe_input ri JOIN blob b ON b.blob_id = ri.blob_id
             WHERE ri.recipe_id = ?1
             ORDER BY ri.position",
        )?;
        let rows = stmt.query_map(params![recipe_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, [u8; 32]>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (position, blob_id, hash, residency, role) = row?;
            out.push(RecipeInputRow {
                position: u32::try_from(position).map_err(|_| IndexError::Decode {
                    what: "input position",
                    code: position,
                })?,
                blob_id,
                hash: Blake3(hash),
                residency: crate::Residency::from_code(residency)?,
                role,
            });
        }
        Ok(out)
    }

    /// The hash of the recipe *object blob* — what the executor decodes
    /// from meta/ to get op, params, and claimed inputs/outputs (the DB
    /// rows are the queryable projection, the object is the truth).
    pub fn recipe_object_hash(&self, recipe_id: i64) -> Result<Blake3, IndexError> {
        let hash: [u8; 32] = self
            .cache()
            .query_row(
                "SELECT b.hash FROM recipe r JOIN blob b ON b.blob_id = r.blob_id
                 WHERE r.recipe_id = ?1",
                params![recipe_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(IndexError::RecipeNotFound(recipe_id))?;
        Ok(Blake3(hash))
    }

    /// One recipe row by id.
    pub fn recipe_by_id(&self, recipe_id: i64) -> Result<RecipeRow, IndexError> {
        let row = self
            .cache()
            .query_row(
                "SELECT recipe_id, blob_id, op_kind, op_name, seek_class, verify, source
                 FROM recipe WHERE recipe_id = ?1",
                params![recipe_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                    ))
                },
            )
            .optional()?
            .ok_or(IndexError::RecipeNotFound(recipe_id))?;
        let (recipe_id, blob_id, op_kind, op_name, seek_class, verify, source) = row;
        Ok(RecipeRow {
            recipe_id,
            blob_id,
            op_kind: OpKind::from_code(op_kind)?,
            op_name,
            seek_class: SeekClass::from_code(seek_class)?,
            verify: VerifyState::from_code(verify)?,
            source: RecipeSource::from_code(source)?,
        })
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

    /// Every poisoned recipe — the rehabilitation candidate pool.
    pub fn list_failed_recipes(&self) -> Result<Vec<i64>, IndexError> {
        let mut stmt = self
            .cache()
            .prepare_cached("SELECT recipe_id FROM recipe WHERE verify = 2 ORDER BY recipe_id")?;
        let rows = stmt
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Rehabilitation: the ONE sanctioned exit from `Failed`, reserved
    /// for the case where a successful re-replay just proved the
    /// poisoning wrong (a host bug — e.g. the fixed pipe verdict race —
    /// or a since-repaired environment). Deliberately not a
    /// [`Db::set_verify_state`] transition: the ordinary state machine
    /// keeps `Failed` terminal so nothing exits poison casually; this
    /// method demands the caller assert a completed, verified replay.
    pub fn rehabilitate_recipe(&self, recipe_id: i64, at_unix: i64) -> Result<(), IndexError> {
        let row = self.recipe_by_id(recipe_id)?;
        if row.verify != VerifyState::Failed {
            return Err(IndexError::IllegalTransition {
                from: row.verify,
                to: VerifyState::ReplayedLocal,
            });
        }
        self.cache().execute(
            "UPDATE recipe SET verify = ?2, verified_at = ?3, fail_error = NULL, fail_peer = NULL
             WHERE recipe_id = ?1",
            params![recipe_id, VerifyState::ReplayedLocal.code(), at_unix],
        )?;
        Ok(())
    }

    /// Resident data blobs claimed as output by at least one
    /// ReplayedLocal recipe — the eviction candidate pool, biggest
    /// reclaim first. Pre-grounding: the planner still runs
    /// [`Db::is_evictable`] per pick (dropping one candidate can strand
    /// another — order matters, sets don't).
    pub fn list_eviction_candidates(&self) -> Result<Vec<crate::BlobRow>, IndexError> {
        // Ordering is POLICY, not cosmetics (D27/D72): best licensed
        // route's seek class first (affine-routed literals rebuild
        // cheap and seekable — drop those first; opaque-routed ones
        // cost a spill per read — keep those longest), then biggest
        // first within a class. This is also what steers a
        // mutually-inverse pair (container ⇄ member plaintext, the
        // preflate shape) to the RIGHT residual: the affine-routed
        // container evicts, D21 grounding then refuses the
        // opaque-routed plaintext, and what stays resident is exactly
        // D53's "the bytes dats name". Size-first ordering strands the
        // pair the other way around.
        let mut stmt = self.cache().prepare_cached(
            "SELECT b.blob_id, b.hash, b.size, b.namespace, b.residency
             FROM blob b
             JOIN recipe_output ro ON ro.blob_id = b.blob_id
             JOIN recipe r ON r.recipe_id = ro.recipe_id
             WHERE b.namespace = 0 AND b.residency = 0 AND r.verify = 3
             GROUP BY b.blob_id
             ORDER BY MIN(r.seek_class), b.size DESC, b.hash",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, [u8; 32]>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(blob_id, hash, size, ns, residency)| {
                Ok(crate::BlobRow {
                    blob_id,
                    hash: Blake3(hash),
                    size: size.map(|s| u64::try_from(s).expect("sizes stored non-negative")),
                    namespace: crate::Namespace::from_code(ns)?,
                    residency: crate::Residency::from_code(residency)?,
                })
            })
            .collect()
    }

    /// Quarantine a component hash's SEEK path (D49 rule 3): its
    /// serve-range output failed output-bao verification. Idempotent
    /// (first reason wins). Sequential replay stays trusted — the claim
    /// itself was proven by full materialization.
    pub fn quarantine_seek(
        &self,
        component: &Blake3,
        at_unix: i64,
        reason: &str,
    ) -> Result<(), IndexError> {
        self.cache().execute(
            "INSERT OR IGNORE INTO seek_quarantine (component, quarantined_at, reason)
             VALUES (?1, ?2, ?3)",
            params![component.0.as_slice(), at_unix, reason],
        )?;
        Ok(())
    }

    pub fn is_seek_quarantined(&self, component: &Blake3) -> Result<bool, IndexError> {
        let mut stmt = self
            .cache()
            .prepare_cached("SELECT 1 FROM seek_quarantine WHERE component = ?1")?;
        Ok(stmt.exists(params![component.0.as_slice()])?)
    }

    /// All quarantined components with reasons (status surface).
    pub fn list_seek_quarantined(&self) -> Result<Vec<(Blake3, i64, String)>, IndexError> {
        let mut stmt = self.cache().prepare_cached(
            "SELECT component, quarantined_at, reason FROM seek_quarantine ORDER BY component",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, [u8; 32]>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .map(|(c, at, r)| (Blake3(c), at, r))
            .collect())
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
