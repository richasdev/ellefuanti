//! The distinction between "we read this" and "we could not".

/// A value that static analysis either determined or explicitly could not.
///
/// This exists so the unknown case cannot be spelled as an empty string or a default.
/// `Resolved::Unknown` forces every consumer to decide what to do about it — a completion
/// list can skip it, a route panel can render it greyed as "dynamic" — whereas `""` reads
/// as a real answer and silently becomes a wrong one. RISKS.md #4 is the whole reason:
/// the failure mode that destroys trust is confident wrongness, not absence.
///
/// [`Unknown`](Resolved::Unknown) carries the source text that defeated us, because
/// showing the user `[$controller, 'act']` explains the gap far better than "unknown" does,
/// and it is what someone debugging the extractor needs to see.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved<T> {
    /// Read directly from the syntax tree. Still only as true as the source is honest —
    /// nothing here proves the route is ever registered at runtime.
    Known(T),
    /// Not statically determinable. Holds the source text of the expression, trimmed.
    Unknown(String),
}

impl<T> Resolved<T> {
    pub fn known(&self) -> Option<&T> {
        match self {
            Resolved::Known(value) => Some(value),
            Resolved::Unknown(_) => None,
        }
    }

    pub fn is_known(&self) -> bool {
        matches!(self, Resolved::Known(_))
    }

    /// The source expression we could not resolve, if this is unknown.
    pub fn unresolved_source(&self) -> Option<&str> {
        match self {
            Resolved::Known(_) => None,
            Resolved::Unknown(source) => Some(source),
        }
    }
}
