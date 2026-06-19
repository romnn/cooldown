//! The Java tool adapters: Maven (`pom.xml`) and Gradle (`gradle.lockfile` + `build.gradle`), both
//! resolving from Maven Central and sharing one Maven version model. A single generic adapter
//! ([`JavaTool`]) is specialised per build tool via a [`JavaLayout`](tool::JavaLayout), so each is
//! its own [`ToolId`](cooldown_core::ToolId) while reusing the registry client and release
//! classification.

pub mod lock;
pub mod registry;
pub mod tool;
pub mod version;

pub use registry::{MAVEN_CENTRAL, MavenCentral};
pub use tool::{Gradle, JavaTool, Maven};

/// The Maven adapter (`pom.xml`).
pub type MavenTool = JavaTool<Maven>;
/// The Gradle adapter (`gradle.lockfile`).
pub type GradleTool = JavaTool<Gradle>;
