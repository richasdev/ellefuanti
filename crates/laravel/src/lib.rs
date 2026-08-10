//! Laravel intelligence: what can be derived from PHP source without running it.
//!
//! Every result here is **best-effort, never an assertion** (RISKS.md #4). Laravel is a
//! dynamic framework: routes are registered from service providers, macros, loops over
//! config, and `Route::` calls whose arguments are variables. A route this misses is a
//! missing completion, which is acceptable. A route it reports *wrongly* is a lie the
//! user navigates on, which is not. Where a value cannot be read off the syntax tree,
//! it comes back as [`Resolved::Unknown`] rather than a guess or an empty string.
//!
//! Blocking and synchronous, like the rest of the domain layer — the caller decides which
//! executor runs it (ADR-0007). No UI dependency (ADR-0004).

mod resolved;
mod routes;

pub use resolved::Resolved;
pub use routes::{HttpMethod, Route, RouteAction, RouteExtraction, extract_routes};
