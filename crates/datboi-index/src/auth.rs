//! Auth v1 state (D30/D68): users, invites, sessions, view grants.
//!
//! All of it lives in state.db. Tokens (invite, session, bearer) are
//! never stored — only `blake3(token)`, so a stolen database file mints
//! nothing. Sessions are authoritative but truncatable and excluded
//! from CAS snapshots; everything else round-trips (D37).

use rusqlite::{OptionalExtension, params};

use crate::types::Role;
use crate::{Db, IndexError};

/// One `user` row.
#[derive(Debug, Clone)]
pub struct UserRow {
    pub user_id: i64,
    pub username: String,
    /// PHC-format argon2id hash string.
    pub argon2: String,
    pub role: Role,
    pub created_at: i64,
}

/// One `session` row, joined with its user for display.
#[derive(Debug, Clone)]
pub struct SessionRow {
    pub user_id: i64,
    pub username: String,
    pub expires_at: i64,
}

/// One `invite` row (the admin-surface projection). Only the token's
/// blake3 is ever stored, so this cannot leak a usable invite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteRow {
    pub token_hash: [u8; 32],
    pub created_by: Option<i64>,
    pub role: Role,
    pub expires_at: i64,
    pub used_by: Option<i64>,
}

/// What [`Db::accept_invite`] decided, atomically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InviteOutcome {
    /// Invite consumed, user created.
    Accepted { user_id: i64, role: Role },
    /// No such invite, already used, or expired — deliberately one
    /// answer (an attacker probing tokens learns nothing extra).
    InviteInvalid,
    /// Username exists; the invite is NOT consumed (retry with another
    /// name).
    UsernameTaken,
}

fn user_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, String, String, i64, i64)> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
    ))
}

fn decode_user(raw: (i64, String, String, i64, i64)) -> Result<UserRow, IndexError> {
    Ok(UserRow {
        user_id: raw.0,
        username: raw.1,
        argon2: raw.2,
        role: Role::from_code(raw.3)?,
        created_at: raw.4,
    })
}

impl Db {
    /// Insert a user. The UNIQUE(username) constraint is the caller's
    /// problem to anticipate ([`Db::user_by_name`] first); a violation
    /// surfaces as the sqlite error.
    pub fn create_user(
        &self,
        username: &str,
        argon2: &str,
        role: Role,
        created_at: i64,
    ) -> Result<i64, IndexError> {
        let user_id = self.state().query_row(
            "INSERT INTO user (username, argon2, role, created_at)
             VALUES (?1, ?2, ?3, ?4) RETURNING user_id",
            params![username, argon2, role.code(), created_at],
            |row| row.get(0),
        )?;
        Ok(user_id)
    }

    pub fn user_by_name(&self, username: &str) -> Result<Option<UserRow>, IndexError> {
        self.state()
            .query_row(
                "SELECT user_id, username, argon2, role, created_at
                 FROM user WHERE username = ?1",
                params![username],
                user_from_row,
            )
            .optional()?
            .map(decode_user)
            .transpose()
    }

    pub fn list_users(&self) -> Result<Vec<UserRow>, IndexError> {
        let mut stmt = self.state().prepare_cached(
            "SELECT user_id, username, argon2, role, created_at
             FROM user ORDER BY username",
        )?;
        stmt.query_map([], user_from_row)?
            .map(|raw| decode_user(raw?))
            .collect()
    }

    /// Record a CLI-minted invite (D68: local shell access = admin, so
    /// minting needs no user; `created_by` stays NULL for those).
    pub fn mint_invite(
        &self,
        token_hash: &[u8; 32],
        created_by: Option<i64>,
        role: Role,
        expires_at: i64,
    ) -> Result<(), IndexError> {
        self.state().execute(
            "INSERT INTO invite (token_hash, created_by, expires_at, used_by, role)
             VALUES (?1, ?2, ?3, NULL, ?4)",
            params![token_hash.as_slice(), created_by, expires_at, role.code()],
        )?;
        Ok(())
    }

    /// Every invite row, soonest expiry first (the admin surface;
    /// callers split pending from consumed/expired).
    pub fn list_invites(&self) -> Result<Vec<InviteRow>, IndexError> {
        let mut stmt = self.state().prepare_cached(
            "SELECT token_hash, created_by, role, expires_at, used_by
             FROM invite ORDER BY expires_at, token_hash",
        )?;
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, [u8; 32]>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<i64>>(4)?,
            ))
        })?
        .map(|raw| {
            let (token_hash, created_by, role, expires_at, used_by) = raw?;
            Ok(InviteRow {
                token_hash,
                created_by,
                role: Role::from_code(role)?,
                expires_at,
                used_by,
            })
        })
        .collect()
    }

    /// Revoke an UNUSED invite; consumed invites stay — they are the
    /// account-provenance record. Returns whether a row died.
    pub fn delete_invite(&self, token_hash: &[u8; 32]) -> Result<bool, IndexError> {
        Ok(self.state().execute(
            "DELETE FROM invite WHERE token_hash = ?1 AND used_by IS NULL",
            params![token_hash.as_slice()],
        )? > 0)
    }

    /// Consume an invite and create its user, atomically: the invite is
    /// checked (exists, unused, unexpired) and marked used in the same
    /// transaction that inserts the user, so a token can never mint two
    /// accounts however racy its presenters are.
    pub fn accept_invite(
        &self,
        token_hash: &[u8; 32],
        username: &str,
        argon2: &str,
        now: i64,
    ) -> Result<InviteOutcome, IndexError> {
        // Read-then-write: must be IMMEDIATE (D93, see cache_write_tx).
        let tx = self.state_write_tx()?;
        let role: Option<i64> = tx
            .query_row(
                "SELECT role FROM invite
                 WHERE token_hash = ?1 AND used_by IS NULL AND expires_at > ?2",
                params![token_hash.as_slice(), now],
                |row| row.get(0),
            )
            .optional()?;
        let Some(role) = role else {
            return Ok(InviteOutcome::InviteInvalid);
        };
        let role = Role::from_code(role)?;
        let taken: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM user WHERE username = ?1)",
            params![username],
            |row| row.get(0),
        )?;
        if taken {
            // Roll back (drop) without consuming: the inviter's token
            // should survive someone else holding the name.
            return Ok(InviteOutcome::UsernameTaken);
        }
        let user_id: i64 = tx.query_row(
            "INSERT INTO user (username, argon2, role, created_at)
             VALUES (?1, ?2, ?3, ?4) RETURNING user_id",
            params![username, argon2, role.code(), now],
            |row| row.get(0),
        )?;
        tx.execute(
            "UPDATE invite SET used_by = ?2 WHERE token_hash = ?1",
            params![token_hash.as_slice(), user_id],
        )?;
        tx.commit()?;
        Ok(InviteOutcome::Accepted { user_id, role })
    }

    pub fn create_session(
        &self,
        token_hash: &[u8; 32],
        user_id: i64,
        expires_at: i64,
    ) -> Result<(), IndexError> {
        self.state().execute(
            "INSERT INTO session (token_hash, user_id, expires_at) VALUES (?1, ?2, ?3)",
            params![token_hash.as_slice(), user_id, expires_at],
        )?;
        Ok(())
    }

    /// Resolve a session token hash to its (live) user. Expired rows
    /// answer `None`; they are garbage until swept by
    /// [`Db::delete_expired_sessions`].
    pub fn session_user(
        &self,
        token_hash: &[u8; 32],
        now: i64,
    ) -> Result<Option<(i64, String, Role)>, IndexError> {
        self.state()
            .query_row(
                "SELECT u.user_id, u.username, u.role
                 FROM session s JOIN user u ON u.user_id = s.user_id
                 WHERE s.token_hash = ?1 AND s.expires_at > ?2",
                params![token_hash.as_slice(), now],
                |row| Ok((row.get(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)?)),
            )
            .optional()?
            .map(|(id, name, role)| Ok((id, name, Role::from_code(role)?)))
            .transpose()
    }

    pub fn delete_session(&self, token_hash: &[u8; 32]) -> Result<bool, IndexError> {
        Ok(self.state().execute(
            "DELETE FROM session WHERE token_hash = ?1",
            params![token_hash.as_slice()],
        )? > 0)
    }

    /// Revoke every session a user holds; returns how many died.
    pub fn delete_sessions_for_user(&self, user_id: i64) -> Result<usize, IndexError> {
        Ok(self
            .state()
            .execute("DELETE FROM session WHERE user_id = ?1", params![user_id])?)
    }

    /// Sweep expired sessions (called opportunistically on login).
    pub fn delete_expired_sessions(&self, now: i64) -> Result<usize, IndexError> {
        Ok(self
            .state()
            .execute("DELETE FROM session WHERE expires_at <= ?1", params![now])?)
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionRow>, IndexError> {
        let mut stmt = self.state().prepare_cached(
            "SELECT s.user_id, u.username, s.expires_at
             FROM session s JOIN user u ON u.user_id = s.user_id
             ORDER BY u.username, s.expires_at",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(SessionRow {
                    user_id: row.get(0)?,
                    username: row.get(1)?,
                    expires_at: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Grant a friend a view (idempotent).
    pub fn grant_view(&self, user_id: i64, view_name: &str) -> Result<(), IndexError> {
        self.state().execute(
            "INSERT OR IGNORE INTO view_grant (user_id, view_name) VALUES (?1, ?2)",
            params![user_id, view_name],
        )?;
        Ok(())
    }

    pub fn revoke_view(&self, user_id: i64, view_name: &str) -> Result<bool, IndexError> {
        Ok(self.state().execute(
            "DELETE FROM view_grant WHERE user_id = ?1 AND view_name = ?2",
            params![user_id, view_name],
        )? > 0)
    }

    pub fn grants_for_user(&self, user_id: i64) -> Result<Vec<String>, IndexError> {
        let mut stmt = self.state().prepare_cached(
            "SELECT view_name FROM view_grant WHERE user_id = ?1 ORDER BY view_name",
        )?;
        let rows = stmt
            .query_map(params![user_id], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn all_grants(&self) -> Result<Vec<(i64, String)>, IndexError> {
        let mut stmt = self
            .state()
            .prepare_cached("SELECT user_id, view_name FROM view_grant ORDER BY user_id")?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}
