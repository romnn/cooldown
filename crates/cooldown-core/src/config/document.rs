use super::ExcludeList;
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
    if selector.edge_policy.is_some() {
        return Err(CoreError::Config(format!(
            "{ctx}: `edge-policy` is cargo-specific; move it to [tool.cargo]"
        )));
    }
    if selector.single_copy.is_some() {
        return Err(CoreError::Config(format!(
            "{ctx}: `single-copy` is pnpm-specific; move it to [tool.pnpm]"
        )));
    }
    if selector.generated_members.is_some() {
        return Err(CoreError::Config(format!(
            "{ctx}: `generated-members` is cargo-specific; move it to [tool.cargo]"
        )));
    }
    Ok(())
}

/// Rejects a `generated-members` entry that could never name one member exactly: a glob (the
/// list is matched by name, so a pattern would fail the "matches nothing" check for the wrong
/// reason), or a blank.
fn validate_generated_members(names: &[String], ctx: &str) -> Result<(), CoreError> {
    for name in names {
        if name.trim().is_empty() {
            return Err(CoreError::Config(format!(
                "{ctx}: `generated-members` lists exact member package names, and one entry is blank"
            )));
        }
        if name.contains(['*', '?', '[', '{']) {
            return Err(CoreError::Config(format!(
                "{ctx}: `generated-members` lists exact member package names, and `{name}` is a glob; a generated member is declared by name, never matched by pattern"
            )));
        }
    }
    Ok(())
}

fn validate_structure(config: &ConfigToml, origin: &Origin) -> Result<(), CoreError> {
    if let Some(tools) = &config.tool {
        for (name, selector) in tools {
            if name != "cargo" && selector.edge_policy.is_some() {
                return Err(CoreError::Config(format!(
                    "{}: `edge-policy` in [tool.{name}] is cargo-specific; move it to [tool.cargo]",
                    origin.token()
                )));
            }
            if name != "pnpm" && selector.single_copy.is_some() {
                return Err(CoreError::Config(format!(
                    "{}: `single-copy` in [tool.{name}] is pnpm-specific; move it to [tool.pnpm]",
                    origin.token()
                )));
            }
            if let Some(names) = &selector.generated_members {
                if name != "cargo" {
                    return Err(CoreError::Config(format!(
                        "{}: `generated-members` in [tool.{name}] is cargo-specific; move it to [tool.cargo]",
                        origin.token()
                    )));
                }
                validate_generated_members(names, &format!("{} [tool.{name}]", origin.token()))?;
            }
            // The gate matches exact names, so a glob would parse, fold, and gate nothing.
            if let Some(list) = &selector.single_copy
                && let Some(glob) = list
                    .patterns()
                    .iter()
                    .find(|entry| entry.contains(['*', '?', '[', '{']))
            {
                return Err(CoreError::Config(format!(
                    "{}: `single-copy` lists exact package names, and `{glob}` is a glob; the gate matches names, not patterns",
                    origin.token()
                )));
            }
        }
    }
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

    /// Returns the document's Cargo edge policy, if `[tool.cargo]` sets one.
    #[must_use]
    pub fn cargo_edge_policy(&self) -> Option<crate::EdgePolicy> {
        self.raw
            .tool
            .as_ref()
            .and_then(|tools| tools.get("cargo"))
            .and_then(|cargo| cargo.edge_policy)
    }

    /// Returns the document's declared generated cargo members, if `[tool.cargo]` sets the
    /// `generated-members` key (an explicit `[]` is a declaration too).
    #[must_use]
    pub fn cargo_generated_members(&self) -> Option<Vec<String>> {
        self.raw
            .tool
            .as_ref()
            .and_then(|tools| tools.get("cargo"))
            .and_then(|cargo| cargo.generated_members.clone())
    }

    /// Returns the document's pnpm single-copy list with its merge mode, if `[tool.pnpm]` sets
    /// one.
    #[must_use]
    pub fn pnpm_single_copy(&self) -> Option<ExcludeList> {
        self.raw
            .tool
            .as_ref()
            .and_then(|tools| tools.get("pnpm"))
            .and_then(|pnpm| pnpm.single_copy.clone())
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
        assert_eq!(scan.resolved("outdated").major, Some(true));
        assert_eq!(scan.exclude_folders_for(&[], "cargo"), vec!["vendor"]);
    }

    /// `single-copy` matches exact names, so a glob that would silently gate nothing is a config
    /// error, in either list form.
    #[test]
    fn single_copy_rejects_a_glob_entry() {
        for src in [
            "[tool.pnpm]\nsingle-copy = [\"react\", \"@scope/*\"]\n",
            "[tool.pnpm]\nsingle-copy = { replace = [\"solid-?s\"] }\n",
        ] {
            let err = ConfigDocument::parse(src, &Origin::Global)
                .expect_err("a glob cannot gate anything");
            assert!(
                err.to_string().contains("exact package names"),
                "the error says what the list takes: {err}"
            );
        }
    }

    /// The pnpm-only `single-copy` key is rejected everywhere but `[tool.pnpm]`, so a list under
    /// another tool or a policy-only selector is a config error instead of a silent no-op.
    #[test]
    fn single_copy_is_accepted_only_under_the_pnpm_tool_table() {
        let accepted = ConfigDocument::parse(
            "[tool.pnpm]\nsingle-copy = [\"solid-js\"]\n",
            &Origin::Global,
        )
        .expect("single-copy belongs under [tool.pnpm]");
        assert_eq!(
            accepted
                .pnpm_single_copy()
                .as_ref()
                .map(ExcludeList::patterns),
            Some(["solid-js".to_string()].as_slice())
        );
        for src in [
            "[tool.npm]\nsingle-copy = [\"react\"]\n",
            "[registry.\"npmjs.com\"]\nsingle-copy = [\"react\"]\n",
            "[project.\"apps/*\"]\nsingle-copy = [\"react\"]\n",
        ] {
            let err = ConfigDocument::parse(src, &Origin::Global)
                .expect_err("a misplaced single-copy must be rejected");
            assert!(
                err.to_string().contains("[tool.pnpm]"),
                "the error points at the correct placement: {err}"
            );
        }
    }

    /// `generated-members` is a declaration by exact member name: an explicit `[]` is a
    /// declaration too, a glob or a blank can never name a member, and the key belongs under
    /// `[tool.cargo]` alone.
    #[test]
    fn generated_members_is_a_cargo_only_list_of_exact_names() {
        let declared = ConfigDocument::parse(
            "[tool.cargo]\ngenerated-members = [\"workspace-hack\"]\n",
            &Origin::Global,
        )
        .expect("declared under [tool.cargo]");
        assert_eq!(
            declared.cargo_generated_members(),
            Some(vec!["workspace-hack".to_string()])
        );
        let empty =
            ConfigDocument::parse("[tool.cargo]\ngenerated-members = []\n", &Origin::Global)
                .expect("an explicit empty list is a declaration");
        assert_eq!(empty.cargo_generated_members(), Some(Vec::new()));
        let unset =
            ConfigDocument::parse("[tool.cargo]\nedge-policy = \"none\"\n", &Origin::Global)
                .expect("no declaration");
        assert_eq!(unset.cargo_generated_members(), None);

        for (src, expected) in [
            (
                "[tool.cargo]\ngenerated-members = [\"*-hack\"]\n",
                "is a glob",
            ),
            ("[tool.cargo]\ngenerated-members = [\" \"]\n", "is blank"),
            (
                "[tool.pnpm]\ngenerated-members = [\"workspace-hack\"]\n",
                "[tool.cargo]",
            ),
            (
                "[project.\"apps/*\"]\ngenerated-members = [\"workspace-hack\"]\n",
                "[tool.cargo]",
            ),
        ] {
            let err = ConfigDocument::parse(src, &Origin::Global)
                .expect_err("a declaration that can never name a member exactly is rejected");
            assert!(
                err.to_string().contains(expected),
                "{src}: the error says why: {err}"
            );
        }
    }

    /// The cargo-only `edge-policy` key is rejected under registry/project selectors (which are
    /// policy-only and could never honor it), like the misplaced exclude lists before it.
    #[test]
    fn edge_policy_is_rejected_under_registry_and_project_selectors() {
        for src in [
            "[registry.\"npmjs.com\"]\nedge-policy = \"preserve\"\n",
            "[project.\"apps/*\"]\nedge-policy = \"preserve\"\n",
        ] {
            let err = ConfigDocument::parse(src, &Origin::Global)
                .expect_err("a misplaced edge-policy must be rejected");
            assert!(
                err.to_string().contains("[tool.cargo]"),
                "the error points at the correct placement: {err}"
            );
        }
    }
}
