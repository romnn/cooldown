//! The JavaScript/TypeScript tool adapters. npm, pnpm, yarn, and bun all resolve from the npm
//! registry and share one `SemVer` version model; they differ only in their lockfile format and the
//! CLI that re-resolves it. A single generic adapter ([`NpmTool`]) is specialised over a
//! [`NodeLock`](lock::NodeLock) per manager, so each is exposed as its own [`ToolId`] while reusing
//! the registry client, release classification, and apply machinery.
//! Yarn's two lock formats share one tool, so [`YarnTool`] routes each project to the
//! specialisation its own lock's format names.
//!
//! [`ToolId`]: cooldown_core::ToolId

mod apply;
pub mod deno;
mod exclusions;
pub mod jsr;
pub mod lock;
mod manifest;
mod native;
pub mod nodecmd;
mod npmrc;
mod patches;
mod peers;
mod pins;
pub mod registry;
pub mod tool;
pub mod version;
mod yarn;

pub use deno::{DENO_ID, DenoTool};
pub use jsr::{JSR, JsrRegistry};
pub use lock::{Bun, Npm, Pnpm, Yarn};
pub use registry::{NPM, NpmRegistry};
pub use tool::NpmTool;
pub use yarn::YarnTool;

/// The Bun adapter, reading the text `bun.lock`; the binary `bun.lockb` is not read.
///
/// Each workspace member's direct dependencies are attributed through the lock's package keys, so
/// its exact manifest pins are protected like pnpm's and npm's (see [`Bun`]).
pub type BunTool = NpmTool<Bun>;
/// The npm adapter (`package-lock.json`).
pub type NpmCliTool = NpmTool<Npm>;
/// The pnpm adapter (`pnpm-lock.yaml`).
pub type PnpmTool = NpmTool<Pnpm>;
