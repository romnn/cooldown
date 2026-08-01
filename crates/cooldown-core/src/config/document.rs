use super::layers::policy_layer_from_config;
use super::scan::{ScanConfig, scan_config_from_config};
use super::schema::{ConfigToml, SelectorToml};
use crate::error::CoreError;
use crate::policy::{Origin, PolicyLayer};

/// One parsed `cooldown.toml` document that can project into both policy and runtime/scan views.
#[derive(Debug, Clone)]
pub struct ConfigDocument {
    raw: ConfigToml,
}

fn reject_tool_only_fields(selector: &SelectorToml, ctx: &str) -> Result<(), CoreError> {
    if selector.package.is_some() {
        return Err(CoreError::Config(format!(
            "{ctx}: nested `package` tables are only supported under [tool.*]"
        )));
    }
    let misplaced_exclude = if selector.exclude_folders.is_some() {
        Some("exclude-folders")
    } else if selector.exclude_packages.is_some() {
        Some("exclude-packages")
    } else {
        None
    };
    if let Some(field) = misplaced_exclude {
        return Err(CoreError::Config(format!(
            "{ctx}: `{field}` is not valid here; exclusion lists live under [tool.*], [global], or a command table"
        )));
    }
    Ok(())
}

fn validate_structure(config: &ConfigToml, origin: &Origin) -> Result<(), CoreError> {
    if let Some(registries) = &config.registry {
        for (name, selector) in registries {
            reject_tool_only_fields(selector, &format!("{} [registry.{name:?}]", origin.token()))?;
        }
    }
    if let Some(projects) = &config.project {
        for (pattern, selector) in projects {
            reject_tool_only_fields(
                selector,
                &format!("{} [project.{pattern:?}]", origin.token()),
            )?;
        }
    }
    Ok(())
}

impl ConfigDocument {
    /// Parse a config document once, annotating any syntax or shape error with the source origin.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Config`] if `content` is not valid config TOML.
    pub fn parse(content: &str, origin: &Origin) -> Result<Self, CoreError> {
        let raw = toml::from_str(content).map_err(|error| {
            let error = error.to_string();
            let exclude_hint =
                if error.contains("exclude-folders") || error.contains("exclude-packages") {
                    "; exclusion lists live under [tool.*], [global], or a command table"
                } else {
                    ""
                };
            CoreError::Config(format!("{}: {error}{exclude_hint}", origin.token()))
        })?;
        validate_structure(&raw, origin)?;
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

    /// The cargo-only `edge-policy` key is rejected under registry/project selectors (which are
    /// policy-only and could never honor it), like the misplaced exclude lists before it.
    #[test]
    fn edge_policy_is_rejected_under_registry_and_project_selectors() {
        for src in [
            "[registry.\"npmjs.com\"]\nedge-policy = \"preserve\"\n",
            "[project.\"apps/*\"]\nedge-policy = \"preserve\"\n",
        ] {
            let doc = ConfigDocument::parse(src, &Origin::Global).expect("parse config document");
            let err = doc
                .policy_layer(Origin::Global)
                .expect_err("a misplaced edge-policy must be rejected");
            assert!(
                err.to_string().contains("[tool.cargo]"),
                "the error points at the correct placement: {err}"
            );
        }
    }
}
