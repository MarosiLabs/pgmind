//! Typed pgmind errors — SQLSTATE class `PM` (RFC-004 A6).
//!
//! pgrx's `ereport!` only accepts the stock `PgSqlErrorCode` enum and blocks
//! direct `err*()` FFI, so custom SQLSTATEs are raised through a plpgsql
//! helper (`pgmind.raise_error`, created in the extension script): `RAISE …
//! USING ERRCODE` accepts arbitrary five-character codes, and the resulting
//! PostgreSQL error propagates through SPI/pgrx with code, message, and
//! DETAIL intact. Every error's DETAIL carries the offending value; agents
//! repair by re-reading, never by forcing.

use pgrx::prelude::*;

/// RFC-004 A6 error table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pm {
    /// PM001 pgmind_invalid_path
    InvalidPath,
    /// PM002 pgmind_note_not_found
    NoteNotFound,
    /// PM003 pgmind_block_not_found
    BlockNotFound,
    /// PM004 pgmind_fragment_arity
    FragmentArity,
    /// PM005 pgmind_invalid_anchor
    InvalidAnchor,
    /// PM006 pgmind_container_constraint
    ContainerConstraint,
    /// PM007 pgmind_section_not_found
    SectionNotFound,
    /// PM008 pgmind_splice_restructures
    SpliceRestructures,
    // RFC-005 D10. PM010 and PM011 mean opposite things to whoever debugs
    // them: "no such revision" is a client bug, "no longer reconstructable"
    // is data the operator retained away.
    /// PM009 pgmind_stale_head
    StaleHead,
    /// PM010 pgmind_unknown_revision
    UnknownRevision,
    /// PM011 pgmind_history_unavailable
    HistoryUnavailable,
    /// PM012 pgmind_excision_refused
    ExcisionRefused,
    /// PM013 pgmind_excision_incomplete
    ExcisionIncomplete,
    /// PM014 pgmind_note_tombstoned
    NoteTombstoned,
    /// PM015 pgmind_path_taken
    PathTaken,
    /// PM016 pgmind_stale_block
    StaleBlock,
    /// PM017 pgmind_invalid_author
    InvalidAuthor,
    /// PM018 pgmind_vault_not_found
    VaultNotFound,
    // Deliberately not PM004. That code is frozen to "fragment parses to the
    // wrong number of addressable blocks" (RFC-004 §A6); a batch whose parallel
    // arrays disagree in length has nothing to do with block counts, and an
    // agent matching on `pgmind_fragment_arity` would repair the wrong thing.
    /// PM019 pgmind_batch_arity
    BatchArity,
}

impl Pm {
    pub fn sqlstate(self) -> &'static str {
        match self {
            Pm::InvalidPath => "PM001",
            Pm::NoteNotFound => "PM002",
            Pm::BlockNotFound => "PM003",
            Pm::FragmentArity => "PM004",
            Pm::InvalidAnchor => "PM005",
            Pm::ContainerConstraint => "PM006",
            Pm::SectionNotFound => "PM007",
            Pm::SpliceRestructures => "PM008",
            Pm::StaleHead => "PM009",
            Pm::UnknownRevision => "PM010",
            Pm::HistoryUnavailable => "PM011",
            Pm::ExcisionRefused => "PM012",
            Pm::ExcisionIncomplete => "PM013",
            Pm::NoteTombstoned => "PM014",
            Pm::PathTaken => "PM015",
            Pm::StaleBlock => "PM016",
            Pm::InvalidAuthor => "PM017",
            Pm::VaultNotFound => "PM018",
            Pm::BatchArity => "PM019",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Pm::InvalidPath => "pgmind_invalid_path",
            Pm::NoteNotFound => "pgmind_note_not_found",
            Pm::BlockNotFound => "pgmind_block_not_found",
            Pm::FragmentArity => "pgmind_fragment_arity",
            Pm::InvalidAnchor => "pgmind_invalid_anchor",
            Pm::ContainerConstraint => "pgmind_container_constraint",
            Pm::SectionNotFound => "pgmind_section_not_found",
            Pm::SpliceRestructures => "pgmind_splice_restructures",
            Pm::StaleHead => "pgmind_stale_head",
            Pm::UnknownRevision => "pgmind_unknown_revision",
            Pm::HistoryUnavailable => "pgmind_history_unavailable",
            Pm::ExcisionRefused => "pgmind_excision_refused",
            Pm::ExcisionIncomplete => "pgmind_excision_incomplete",
            Pm::NoteTombstoned => "pgmind_note_tombstoned",
            Pm::PathTaken => "pgmind_path_taken",
            Pm::StaleBlock => "pgmind_stale_block",
            Pm::InvalidAuthor => "pgmind_invalid_author",
            Pm::VaultNotFound => "pgmind_vault_not_found",
            Pm::BatchArity => "pgmind_batch_arity",
        }
    }
}

/// Raise `ERROR` with the PM SQLSTATE, a message, and a DETAIL. Never returns.
pub fn pm_error(code: Pm, message: &str, detail: &str) -> ! {
    let msg = format!("pgmind: {message}");
    let det = format!("{} — {detail}", code.name());
    let _ = Spi::run_with_args(
        "SELECT pgmind.raise_error($1, $2, $3)",
        &[
            code.sqlstate().into(),
            msg.as_str().into(),
            det.as_str().into(),
        ],
    );
    // The SPI call above always errors (that is its job); if we are somehow
    // still here, fail hard rather than continue with broken invariants.
    pgrx::error!("pgmind: raise_error unexpectedly returned ({msg})");
}
