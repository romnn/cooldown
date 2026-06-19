//! The JavaScript/TypeScript tool adapters. npm, pnpm, yarn, and bun all resolve from the npm
//! registry and share one `SemVer` version model; they differ only in their lockfile format and the
//! CLI that re-resolves it. A single generic adapter ([`NpmTool`]) is specialised over a
//! [`NodeLock`](lock::NodeLock) per manager, so each is exposed as its own [`ToolId`] while reusing
//! the registry client, release classification, and apply machinery.
//!
//! [`ToolId`]: cooldown_core::ToolId

pub mod deno;
pub mod jsr;
pub mod lock;
mod manifest;
pub mod nodecmd;
pub mod registry;
pub mod tool;
pub mod version;

pub use deno::{DENO_ID, DenoTool};
pub use jsr::{JSR, JsrRegistry};
pub use lock::{Bun, Npm, Pnpm, Yarn};
pub use registry::{NPM, NpmRegistry};
pub use tool::NpmTool;

/// The Bun adapter (`bun.lock`).
pub type BunTool = NpmTool<Bun>;
/// The npm adapter (`package-lock.json`).
pub type NpmCliTool = NpmTool<Npm>;
/// The pnpm adapter (`pnpm-lock.yaml`).
pub type PnpmTool = NpmTool<Pnpm>;
/// The Yarn (classic) adapter (`yarn.lock`).
pub type YarnTool = NpmTool<Yarn>;
