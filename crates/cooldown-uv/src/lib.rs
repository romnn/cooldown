//! The Python/uv tool adapter: it reads the `uv.lock` graph and per-file upload times itself
//! (falling back to `PyPI` / PEP 700), computes verdicts in the core, and drives `uv` only to
//! re-resolve/apply a chosen window. `[tool.uv]` `exclude-newer`/`exclude-newer-package` is read as
//! a native config layer.

mod artifact;
pub mod lock;
mod manifest;
mod native;
pub mod pypi;
pub mod tool;
pub mod uvcmd;
pub mod version;

pub use pypi::PyPi;
pub use tool::{UV_ID, UvTool};
