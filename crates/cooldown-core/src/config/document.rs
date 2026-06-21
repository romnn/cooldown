use super::layers::policy_layer_from_config;
use super::scan::{ScanConfig, scan_config_from_config};
use super::schema::ConfigToml;
use crate::error::CoreError;
use crate::policy::{Origin, PolicyLayer};

/// One parsed `cooldown.toml` document that can project into both policy and runtime/scan views.
#[derive(Debug, Clone)]
pub struct ConfigDocument {
    raw: ConfigToml,
}

impl ConfigDocument {
    /// Parse a config document once, annotating any syntax or shape error with the source origin.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Config`] if `content` is not valid config TOML.
    pub fn parse(content: &str, origin: &Origin) -> Result<Self, CoreError> {
        let raw = toml::from_str(content)
            .map_err(|error| CoreError::Config(format!("{}: {error}", origin.token())))?;
        Ok(ConfigDocument { raw })
    }

    /// Project this parsed document into the unified policy layer model.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Config`] when selector validation, duration parsing, or other
    /// policy-layer normalization fails.
    pub fn policy_layer(&self, origin: Origin) -> Result<PolicyLayer, CoreError> {
        policy_layer_from_config(self.raw.clone(), origin)
    }

    /// Project this parsed document into the non-policy scan/runtime config view.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Config`] if a `[tool.*]` scan setting names an unknown tool.
    pub fn scan_config(&self, origin: &Origin) -> Result<ScanConfig, CoreError> {
        scan_config_from_config(self.raw.clone(), origin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    #[test]
    fn one_document_projects_to_policy_and_scan_views() {
        let src = indoc! {r#"

            min-age = "14d"

            [global]
            major = true

            [tool.cargo]
            exclude-folders = ["vendor"]
        "#};
        let doc = ConfigDocument::parse(src, &Origin::Global).expect("parse config document");

        let layer = doc.policy_layer(Origin::Global).expect("policy layer");
        let scan = doc.scan_config(&Origin::Global).expect("scan config");

        assert!(!layer.rules.is_empty(), "policy projection kept rule data");
        assert_eq!(scan.global.major, Some(true));
        assert_eq!(scan.exclude_folders_for(&[], "cargo"), vec!["vendor"]);
    }
}
