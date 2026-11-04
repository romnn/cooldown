//! Line-preserving edits of a Yarn rc file's text.
//!
//! An edit replaces the lines of the settings it owns and leaves every other line, comment and
//! line ending as it was, which a YAML round trip would not.
//! The edits read the document's layout from its lines alone, so the caller checks the edited
//! text against the settings it must hold before writing it ([`super::verified`]).

/// The text of an rc file as lines, with the line ending it uses.
#[derive(Debug, Clone)]
pub(super) struct RcText {
    lines: Vec<String>,
    /// `\r\n` when the file uses it anywhere, `\n` otherwise.
    newline: &'static str,
    /// Whether the text ends with a line ending, which an appended block always does.
    trailing_newline: bool,
}

/// Why a top-level setting could not be edited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum EditError {
    /// The key appears more than once at the top level, and Yarn reads only the last one.
    Duplicate,
}

impl RcText {
    pub(super) fn new(content: &str) -> Self {
        RcText {
            lines: content.lines().map(str::to_string).collect(),
            newline: if content.contains("\r\n") {
                "\r\n"
            } else {
                "\n"
            },
            trailing_newline: content.ends_with('\n'),
        }
    }

    pub(super) fn render(&self) -> String {
        let mut text = self.lines.join(self.newline);
        if self.trailing_newline && !self.lines.is_empty() {
            text.push_str(self.newline);
        }
        text
    }

    /// Replaces the top-level `key` and its block with `block`, or appends `block` at the end of
    /// the document when `key` is absent.
    ///
    /// # Errors
    ///
    /// Returns [`EditError::Duplicate`] when `key` appears more than once at the top level.
    pub(super) fn set_top_level(&mut self, key: &str, block: Vec<String>) -> Result<(), EditError> {
        let found: Vec<usize> = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| {
                !line.starts_with(char::is_whitespace)
                    && mapping_key(line).is_some_and(|(found, _)| found == key)
            })
            .map(|(index, _)| index)
            .collect();
        match found.as_slice() {
            [] => {
                self.lines.extend(block);
                self.trailing_newline = true;
            }
            &[index] => {
                let end = block_end(&self.lines, index + 1, 0);
                self.replace(index, end, block);
            }
            _ => return Err(EditError::Duplicate),
        }
        Ok(())
    }

    /// Sets `npmMinimalAgeGate` to `minutes` in the entry of `scope` under the top-level
    /// `npmScopes`, returning whether that entry could be edited.
    ///
    /// Only an entry that is a block mapping of settings inside a block-mapping `npmScopes` is
    /// edited, and neither may carry an `onConflict` key, since Yarn then reads the settings
    /// inside its `value` and would ignore a gate written beside it.
    /// An empty entry is left as it is too: Yarn reads it as the scope's defaults, hiding the
    /// scope's entries in less specific rc files, which a mapping would bring back, their
    /// registry and credentials included.
    /// Every other shape, a flow mapping, a scalar, tab indentation or an entry the file names
    /// twice, is left as it is.
    pub(super) fn set_scope_gate(&mut self, scope: &str, minutes: u64) -> bool {
        let Some(scopes) = self.single_top_level(super::SCOPES_KEY) else {
            return false;
        };
        let Some(entries) = self.children(scopes, 0) else {
            return false;
        };
        if entries.keys.iter().any(|(key, _)| key == "onConflict") {
            return false;
        }
        let named: Vec<usize> = entries
            .keys
            .iter()
            .filter(|(key, _)| key == scope)
            .map(|(_, index)| *index)
            .collect();
        let &[entry] = named.as_slice() else {
            return false;
        };
        let Some(properties) = self.children(entry, entries.indent) else {
            return false;
        };
        if properties.keys.is_empty() || properties.keys.iter().any(|(key, _)| key == "onConflict")
        {
            return false;
        }
        let line = format!(
            "{indent}{key}: {minutes}",
            indent = " ".repeat(properties.indent),
            key = super::GATE_KEY
        );
        let gates: Vec<usize> = properties
            .keys
            .iter()
            .filter(|(key, _)| key == super::GATE_KEY)
            .map(|(_, index)| *index)
            .collect();
        match gates.as_slice() {
            [] => self.lines.insert(entry + 1, line),
            &[gate] => {
                let end = block_end(&self.lines, gate + 1, properties.indent);
                self.replace(gate, end, vec![line]);
            }
            _ => return false,
        }
        true
    }

    /// Replaces the lines from `start` up to `end` with `block`.
    fn replace(&mut self, start: usize, end: usize, block: Vec<String>) {
        let tail = self.lines.split_off(end.min(self.lines.len()));
        self.lines.truncate(start);
        self.lines.extend(block);
        self.lines.extend(tail);
    }

    /// The line of the top-level `key`, when the document has exactly one.
    fn single_top_level(&self, key: &str) -> Option<usize> {
        let mut found = self.lines.iter().enumerate().filter(|(_, line)| {
            !line.starts_with(char::is_whitespace)
                && mapping_key(line).is_some_and(|(found, _)| found == key)
        });
        let (index, _) = found.next()?;
        found.next().is_none().then_some(index)
    }

    /// The keys of the block mapping that the key at line `parent`, indented by `parent_indent`,
    /// opens, or `None` when its value is anything but a block mapping or nothing.
    ///
    /// A key without nested lines is an empty mapping, whose children would be indented two
    /// columns deeper.
    fn children(&self, parent: usize, parent_indent: usize) -> Option<Children> {
        let (_, rest) = mapping_key(self.lines.get(parent)?.get(parent_indent..)?)?;
        if !is_blank_or_comment(rest) {
            return None;
        }
        let end = block_end(&self.lines, parent + 1, parent_indent);
        let mut children = Children {
            indent: parent_indent + 2,
            keys: Vec::new(),
        };
        let mut first = true;
        for index in parent + 1..end {
            let line = self.lines.get(index)?;
            if is_blank_or_comment(line) {
                continue;
            }
            let indent = space_indent(line)?;
            if first {
                children.indent = indent;
                first = false;
            }
            if indent < children.indent {
                return None;
            }
            if indent == children.indent {
                let (key, _) = mapping_key(line.get(indent..)?)?;
                children.keys.push((key, index));
            }
        }
        Some(children)
    }
}

/// The keys of one block mapping, each with its line, and their indentation.
struct Children {
    indent: usize,
    keys: Vec<(String, usize)>,
}

/// The key of a mapping entry `content` opens (indentation already removed) and the text after
/// its `:`, or `None` when the line opens no plain or simply quoted key.
fn mapping_key(content: &str) -> Option<(String, &str)> {
    for quote in ['"', '\''] {
        if let Some(quoted) = content.strip_prefix(quote) {
            let (key, rest) = quoted.split_once(quote)?;
            if key.contains(['\\', quote]) {
                return None;
            }
            let rest = rest.strip_prefix(':')?;
            return (rest.is_empty() || rest.starts_with([' ', '\t']))
                .then(|| (key.to_string(), rest));
        }
    }
    if content.starts_with([
        '-', '#', '{', '[', '?', '&', '*', '!', '|', '>', '%', '@', '`',
    ]) {
        return None;
    }
    let colon = content
        .char_indices()
        .find(|&(index, character)| {
            character == ':'
                && content
                    .get(index + 1..)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
        })
        .map(|(index, _)| index)?;
    let key = content.get(..colon)?.trim_end();
    if key.is_empty() || key.contains(" #") {
        return None;
    }
    Some((key.to_string(), content.get(colon + 1..)?))
}

fn is_blank_or_comment(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.is_empty() || trimmed.starts_with('#')
}

/// The indentation of `line` in spaces, or `None` when a tab indents it, which YAML forbids.
fn space_indent(line: &str) -> Option<usize> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    (!line.get(indent..)?.starts_with('\t')).then_some(indent)
}

/// The width of `line`'s leading whitespace.
fn indent_width(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The exclusive end of the block of a key indented by `parent`, scanning `lines` from `start`,
/// the line after the key.
///
/// The block holds every following line indented deeper than `parent`.
/// A run of blank and comment lines belongs to it only while deeper content follows, so a
/// comment that introduces the next key stays with that key.
fn block_end(lines: &[String], start: usize, parent: usize) -> usize {
    let mut end = start;
    while let Some(line) = lines.get(end) {
        if !is_blank_or_comment(line) {
            if indent_width(line) > parent {
                end += 1;
                continue;
            }
            break;
        }
        let mut probe = end + 1;
        let continues = loop {
            match lines.get(probe) {
                Some(next) if is_blank_or_comment(next) => probe += 1,
                Some(next) => break indent_width(next) > parent,
                None => break false,
            }
        };
        if !continues {
            break;
        }
        end = probe;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::{EditError, RcText, mapping_key};
    use indoc::indoc;

    fn gate(minutes: u64) -> Vec<String> {
        vec![format!("npmMinimalAgeGate: {minutes}")]
    }

    /// An absent key is appended after the document, whose other lines, comments and trailing
    /// newline stay as they were; a present key is replaced in place with its whole block.
    #[test]
    fn a_top_level_key_is_appended_or_replaced_in_place() -> Result<(), EditError> {
        let mut text = RcText::new(indoc! {"
            # Yarn settings
            enableTelemetry: false
        "});
        text.set_top_level("npmMinimalAgeGate", gate(10080))?;
        assert_eq!(
            text.render(),
            indoc! {"
                # Yarn settings
                enableTelemetry: false
                npmMinimalAgeGate: 10080
            "}
        );

        // The value's own block, interior comments included, goes with it; the comment that
        // introduces the next key stays.
        let mut text = RcText::new(indoc! {r#"
            npmPreapprovedPackages:
              - "a"

              # ours
              - "b"

            # the gate
            npmMinimalAgeGate: "7d"
        "#});
        text.set_top_level(
            "npmPreapprovedPackages",
            vec!["npmPreapprovedPackages: []".into()],
        )?;
        text.set_top_level("npmMinimalAgeGate", gate(10080))?;
        assert_eq!(
            text.render(),
            indoc! {"
                npmPreapprovedPackages: []

                # the gate
                npmMinimalAgeGate: 10080
            "}
        );
        Ok(())
    }

    /// The key is matched whole, quoted or plain, and only at the top level.
    #[test]
    fn only_the_whole_top_level_key_is_matched() -> Result<(), EditError> {
        let mut text = RcText::new(indoc! {r#"
            npmMinimalAgeGateX: 1
            npmScopes:
              corp:
                npmMinimalAgeGate: 5
            "npmMinimalAgeGate": 2
        "#});
        text.set_top_level("npmMinimalAgeGate", gate(60))?;
        assert_eq!(
            text.render(),
            indoc! {"
                npmMinimalAgeGateX: 1
                npmScopes:
                  corp:
                    npmMinimalAgeGate: 5
                npmMinimalAgeGate: 60
            "}
        );
        Ok(())
    }

    /// A key Yarn would read twice is refused rather than edited.
    #[test]
    fn a_duplicated_key_is_refused() {
        let mut text = RcText::new("npmMinimalAgeGate: 1\nnpmMinimalAgeGate: 2\n");
        assert_eq!(
            text.set_top_level("npmMinimalAgeGate", gate(60)),
            Err(EditError::Duplicate)
        );
    }

    /// CRLF line endings and a missing trailing newline survive an in-place edit.
    #[test]
    fn line_endings_are_preserved() -> Result<(), EditError> {
        let mut text = RcText::new("a: 1\r\nnpmMinimalAgeGate: 5\r\nb: 2");
        text.set_top_level("npmMinimalAgeGate", gate(60))?;
        assert_eq!(text.render(), "a: 1\r\nnpmMinimalAgeGate: 60\r\nb: 2");

        // An empty file gains the key and a newline.
        let mut text = RcText::new("");
        text.set_top_level("npmMinimalAgeGate", gate(60))?;
        assert_eq!(text.render(), "npmMinimalAgeGate: 60\n");
        Ok(())
    }

    /// A scope entry in block style gains the gate as its first property, or has its gate
    /// replaced, whatever its indentation, leaving its routing and authentication untouched.
    #[test]
    fn a_block_scope_entry_gains_or_replaces_its_gate() {
        let mut text = RcText::new(indoc! {r#"
            npmScopes:
                # private registry
                corp:
                    npmRegistryServer: "https://npm.corp.example"
                    npmAuthToken: "${CORP_TOKEN}"
                "other":
                    npmMinimalAgeGate: "1d"
                    npmAlwaysAuth: true
            enableTelemetry: false
        "#});
        assert!(text.set_scope_gate("corp", 10080));
        assert!(text.set_scope_gate("other", 10080));
        assert_eq!(
            text.render(),
            indoc! {r#"
                npmScopes:
                    # private registry
                    corp:
                        npmMinimalAgeGate: 10080
                        npmRegistryServer: "https://npm.corp.example"
                        npmAuthToken: "${CORP_TOKEN}"
                    "other":
                        npmMinimalAgeGate: 10080
                        npmAlwaysAuth: true
                enableTelemetry: false
            "#}
        );
        // Setting the same gate again changes nothing.
        let before = text.render();
        assert!(text.set_scope_gate("corp", 10080));
        assert_eq!(text.render(), before);
    }

    /// A scope entry Yarn would not read a gate beside is never edited: a flow mapping, a
    /// conflict wrapper on the entry or on `npmScopes`, an absent entry, and a flow `npmScopes`.
    /// Neither is an empty entry, which Yarn reads as the scope's defaults rather than a mapping
    /// that inherits the scope's settings from less specific files.
    #[test]
    fn an_entry_yarn_reads_otherwise_is_left_alone() {
        for document in [
            "npmScopes:\n  corp:\nenableTelemetry: false\n",
            "npmScopes:\n  corp:\n    # only a comment\n",
            "npmScopes:\n  corp: {npmRegistryServer: x}\n",
            "npmScopes:\n  corp:\n    onConflict: reset\n    value:\n      npmAlwaysAuth: true\n",
            "npmScopes:\n  onConflict: reset\n  corp:\n    npmAlwaysAuth: true\n",
            "npmScopes:\n  other:\n    npmAlwaysAuth: true\n",
            "npmScopes: {corp: {npmAlwaysAuth: true}}\n",
            "npmScopes:\n\tcorp:\n\t\tnpmAlwaysAuth: true\n",
            "npmScopes:\n  corp:\n    npmAlwaysAuth: true\n  corp:\n    npmAlwaysAuth: false\n",
        ] {
            let mut text = RcText::new(document);
            assert!(!text.set_scope_gate("corp", 10080), "{document}");
            assert_eq!(text.render(), document);
        }
    }

    #[test]
    fn mapping_keys_are_read_plain_or_quoted() {
        assert_eq!(
            mapping_key("npmScopes:"),
            Some(("npmScopes".to_string(), ""))
        );
        assert_eq!(
            mapping_key("\"corp\": # note"),
            Some(("corp".to_string(), " # note"))
        );
        assert_eq!(mapping_key("'corp':"), Some(("corp".to_string(), "")));
        assert_eq!(
            mapping_key("url: https://x"),
            Some(("url".to_string(), " https://x"))
        );
        for line in [
            "- item",
            "# comment",
            "key:value",
            "{a: 1}",
            "\"a\\\"b\": 1",
        ] {
            assert_eq!(mapping_key(line), None, "{line}");
        }
    }
}
