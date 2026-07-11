//! Parameterized AnkQL construction (issue #17).
//!
//! Never splice runtime values into query text with `format!` — build the
//! query with `?` placeholders and populate them as typed AST literals via
//! these helpers. Values then cannot alter the query's structure (quotes and
//! other AnkQL metacharacters are inert), and EntityIds keep their typed
//! collation instead of comparing as text.
//!
//! `ankql` ships re-exported through the `ankurah` crate, so this adds no
//! dependency. Anything convertible to `ankql::ast::Expr` works as a
//! parameter: `&EntityId`, `&str`, `String`, `i64`, `f64`, `bool`.

use ankurah::ankql::{
    self,
    ast::{Expr, Predicate, Selection},
    error::ParseError,
};

/// Parse a selection (predicate + optional ORDER BY / LIMIT) containing `?`
/// placeholders and populate them, in order, with the given parameters.
/// Placeholder/parameter count mismatches fail closed as a parse error.
pub fn selection(src: &str, params: impl IntoIterator<Item = Expr>) -> Result<Selection, ParseError> {
    let parsed = ankql::parser::parse_selection(src)?;
    Ok(Selection { predicate: parsed.predicate.populate(params)?, order_by: parsed.order_by, limit: parsed.limit })
}

/// Like [`selection`], but returns only the predicate — for APIs that take a
/// bare `Predicate` (e.g. `ScrollManager::new`).
pub fn predicate(src: &str, params: impl IntoIterator<Item = Expr>) -> Result<Predicate, ParseError> {
    Ok(selection(src, params)?.predicate)
}
