//! The Python/uv tool adapter: it reads the `uv.lock` graph and per-file upload times itself
//! (falling back to `PyPI` / PEP 700), computes verdicts in the core, and drives `uv` only to
//! re-resolve/apply a chosen window. `[tool.uv]` `exclude-newer`/`exclude-newer-package` is read as
//! a native config layer.

mod ambient;
mod artifact;
mod build_requires;
mod ceiling;
pub mod lock;
mod manifest;
mod native;
pub mod pypi;
mod requirement;
pub mod tool;
pub mod uvcmd;
pub mod version;

pub use pypi::PyPi;
pub use tool::{PREVIEW_PRUNED_DIRS, UV_ID, UvTool};
