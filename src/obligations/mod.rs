//! Obligations-related Rust modules.
//!
//! The canonical obligations CLI is the Python tool under `tools/obligations/`.
//! This module hosts Rust-side prototypes and helpers that may eventually be
//! invoked from the same hook hot-path.
//!
//! Currently contains:
//!   - `ast_predicate`: a SPIKE exploring AST-based Bash predicate evaluation
//!     as an alternative to the current regex-based `no_pipe_pattern` kind.
//!     See module docs for rationale + design.

pub mod ast_predicate;
