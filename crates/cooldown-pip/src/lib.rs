//! The pip and Poetry tool adapters. Both resolve from PyPI and reuse the shared PyPI client and
//! PEP 440 version model from [`cooldown_uv`]; a single generic adapter ([`PyTool`]) is specialised
//! per tool via a [`PyLayout`](tool::PyLayout), so each is its own
//! [`ToolId`](cooldown_core::ToolId) reading its own manifest/lock format.

pub mod lock;
pub mod tool;

pub use tool::{Pip, Poetry, PyTool};

/// The pip adapter (`requirements.txt`).
pub type PipTool = PyTool<Pip>;
/// The Poetry adapter (`poetry.lock`).
pub type PoetryTool = PyTool<Poetry>;
