//! Policy types and the pure resolver.
//!
//! Two orthogonal axes decide a value: **layers** (where a value comes from, low→high authority)
//! and **selectors** (what it applies to, most→least specific). Resolution is *per field*, and
//! each field has its own combine rule:
//!
//! - `min-age` / per-kind windows — **authority-first**: the highest layer that sets the field
//!   wins; within a layer the most specific selector breaks the tie. Layer dominates selector.
//! - `floor` — **max-clamp**: the effective window clamps up to `max(floor)` over all layers.
//! - `allow` — **accumulated union** that zeroes an ordinary window, but bypasses a floor only
//!   per-floor: a floor is escaped only by an allow co-declared in that floor's own layer, or by an
//!   audited env/CLI allow. A floor in any other layer remains as a residual clamp — so a repo
//!   `allow` cannot undercut a separate org/global floor.
//! - `strict-native` — **security-monotone** OR across layers (handled on [`PolicyStack`]).

mod model;
mod resolve;

pub use model::{
    ByKind, Origin, PatternGlob, PolicyLayer, PolicyStack, Resolution, ResolveKind, ResolveQuery,
    ResolvedWindow, Rule, Selector, TraceStep, WindowSpec,
};
pub use resolve::resolve;
