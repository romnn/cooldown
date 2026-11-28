//! The Java tool adapters: Maven (`pom.xml`) and Gradle (`gradle.lockfile` + `build.gradle`), both
//! resolving from Maven Central and sharing one Maven version model and registry client.

pub mod gradle;
pub mod lock;
pub mod maven;
pub mod registry;
pub mod version;

pub use gradle::GradleTool;
pub use maven::MavenTool;
pub use registry::{MAVEN_CENTRAL, MavenCentral};
