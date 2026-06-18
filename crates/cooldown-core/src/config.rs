//! The TOML config schema and its conversion into [`PolicyLayer`]s.
//!
//! One schema is used everywhere (the global file and every `cooldown.toml`). `min-age` is either a
//! duration scalar or a per-kind table — never both in one selector. Within any single selector,
//! `latest`, `freeze`, and `min-age` are mutually exclusive (a config-validation error, exit 2),
//! the same rule the CLI enforces for `--latest`/`--freeze`/`--min-age`.

mod layers;
mod scan;
mod schema;

pub use layers::{builtin_default_layer, layer_from_fields, parse_config};
pub use scan::{ScanConfig, parse_scan_config};
pub use schema::{CommandConfig, WindowFields};
