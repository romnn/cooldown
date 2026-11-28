//! The conda and pixi tool adapters. Both lockfiles pin a mix of conda-channel and PyPI packages,
//! and each adapter routes a dependency to the anaconda.org or PyPI registry it belongs to, reusing
//! the PyPI client and PEP 440 version model from `cooldown_uv`.

pub mod lock;
mod pixi;
pub mod registry;
mod tool;

pub use pixi::PixiTool;
pub use registry::{CONDA, Conda};
pub use tool::CondaTool;
