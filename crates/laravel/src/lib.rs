//! Laravel intelligence: what can be derived from PHP source without running it.
//!
//! Every result here is **best-effort, never an assertion** (RISKS.md #4). Laravel is a
//! dynamic framework: routes are registered from service providers, macros, loops over
//! config, and `Route::` calls whose arguments are variables. A route this misses is a
//! missing completion, which is acceptable. A route it reports *wrongly* is a lie the
//! user navigates on, which is not. Where a value cannot be read off the syntax tree,
//! it comes back as [`Resolved::Unknown`] rather than a guess or an empty string.
//!
//! Navigation ([`reference_at`], [`resolve`]) spells the same rule with `Option` instead of
//! [`Resolved`], because the question is different. Extraction enumerates and has to report
//! the gaps it left, so an unreadable field is a value. Navigation answers one click, so an
//! unreadable name is simply no answer — and, crucially, so is a name that resolves to
//! nothing on disk. **A `None` from [`resolve`] means "not found", never "does not exist":**
//! views come from a configurable finder, components from registered namespaces, routes from
//! anything a service provider ran. Staying silent is allowed; saying it is missing is not.
//!
//! Blocking and synchronous, like the rest of the domain layer — the caller decides which
//! executor runs it (ADR-0007). No UI dependency (ADR-0004).

mod columns;
mod livewire;
mod models;
mod references;
mod resolved;
mod routes;
mod targets;

pub use columns::{Argument, ColumnContext, ColumnTarget, column_context_at, scope_context_at};
pub use livewire::{
    LivewireFacts, WireTarget, extract_livewire, livewire_class_path, wire_context_at,
};
pub use models::{ModelFacts, extract_migration_columns, extract_model};
pub use references::{Reference, ReferenceKind, reference_at};
pub use resolved::Resolved;
pub use routes::{HttpMethod, Route, RouteAction, RouteExtraction, extract_routes};
pub use targets::{Target, resolve, route_names};
