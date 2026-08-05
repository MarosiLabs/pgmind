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
