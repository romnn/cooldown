//! The conda and pixi tool adapters. Both lockfiles pin a mix of conda-channel and PyPI packages;
//! a single generic adapter ([`CondaEnvTool`]) is specialised per tool via a
//! [`CondaLayout`](tool::CondaLayout) and routes each dependency to the anaconda.org or PyPI
//! registry it belongs to, reusing the PyPI client and PEP 440 version model from `cooldown_uv`.

pub mod lock;
pub mod registry;
pub mod tool;

pub use registry::{CONDA, Conda};
pub use tool::{CondaEnvTool, CondaLock, Pixi};

/// The conda-lock adapter (`conda-lock.yml`).
pub type CondaTool = CondaEnvTool<CondaLock>;
/// The pixi adapter (`pixi.lock`).
pub type PixiTool = CondaEnvTool<Pixi>;
