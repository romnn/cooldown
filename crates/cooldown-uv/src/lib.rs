//! The Python/uv ecosystem adapter: it reads the `uv.lock` graph and per-file upload times itself
//! (falling back to PyPI / PEP 700), computes verdicts in the core, and drives `uv` only to
//! re-resolve/apply a chosen window. `[tool.uv]` `exclude-newer`/`exclude-newer-package` is read as
//! a native config layer.

pub mod ecosystem;
pub mod lock;
pub mod pypi;
pub mod uvcmd;
pub mod version;

pub use ecosystem::{UV_ID, UvEcosystem};
pub use pypi::PyPi;
