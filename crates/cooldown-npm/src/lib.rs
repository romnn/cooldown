//! The JavaScript/TypeScript tool adapters. npm, pnpm, yarn, and bun all resolve from the npm
//! registry and share one `SemVer` version model; they differ only in their lockfile format and the
//! CLI that re-resolves it. A single generic adapter ([`NpmTool`]) is specialised over a
//! [`NodeLock`](lock::NodeLock) per manager, so each is exposed as its own [`ToolId`] while reusing
//! the registry client, release classification, and apply machinery.
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

pub use deno::{DENO_ID, DenoTool};
pub use jsr::{JSR, JsrRegistry};
pub use lock::{Bun, Npm, Pnpm, Yarn};
pub use registry::{NPM, NpmRegistry};
pub use tool::NpmTool;

/// The Bun adapter, reading the text `bun.lock`; the binary `bun.lockb` is not read.
///
/// Each workspace member's direct dependencies are attributed through the lock's package keys, so
/// its exact manifest pins are protected like pnpm's and npm's (see [`Bun`]).
pub type BunTool = NpmTool<Bun>;
/// The npm adapter (`package-lock.json`).
pub type NpmCliTool = NpmTool<Npm>;
/// The pnpm adapter (`pnpm-lock.yaml`).
pub type PnpmTool = NpmTool<Pnpm>;
/// The Yarn Classic (v1) adapter (`yarn.lock`); a Yarn Berry lock is refused as unreadable.
///
/// A Classic lock records no workspace member, so nothing is attributed per member and exact
/// manifest pins are judged only before the resolve.
pub type YarnTool = NpmTool<Yarn>;
