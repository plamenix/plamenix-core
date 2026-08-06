//! `plamenix new <id>` — scaffolds a fresh plugin directory (I7.11)
//! using the on-disk templates from `crates/plamenix-cli/templates/`
//! (I7.14).
//!
//! Layout produced:
//!
//! ```text
//! my-plugin/
//! ├── manifest.toml          (validated by the host loader)
//! ├── Cargo.toml             (cdylib targeting wasm32-wasip2)
//! ├── package.json           (UI half — minimal React + plamenix-ui peer)
//! ├── src/
//! │   ├── lib.rs             (Rust plugin entry, prints to host.log)
//! │   └── ui.tsx             (React UI module — sidebar panel stub)
//! └── README.md              (one-line summary + build instructions)
//! ```
//!
//! Templates live as real files under `crates/plamenix-cli/templates/`
//! so contributors can edit them with IDE syntax highlighting +
//! `cargo fmt`-style diffs. They are **embedded at compile time**
//! via `include_str!` so the binary stays a single executable + no
//! runtime filesystem lookup is needed. Placeholder syntax is
//! `{{NAME}}` substituted in a single `str::replace` pass — no
//! `tera` / `handlebars` dep.

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// Errors emitted by the scaffold pipeline.
#[derive(Debug, Error)]
pub enum ScaffoldError {
    /// User passed an empty / invalid plugin id (must match
    /// `[a-z0-9][a-z0-9._-]*` and stay under 128 chars).
    #[error("invalid plugin id `{0}`: {1}")]
    InvalidId(String, &'static str),
    /// The target directory already exists. Refuse to clobber.
    #[error("target directory already exists: {0}")]
    TargetExists(PathBuf),
    /// IO error while creating the scaffold tree.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Result alias for the scaffold module.
pub type ScaffoldResult<T> = Result<T, ScaffoldError>;

/// Options passed to [`scaffold_new_plugin`].
#[derive(Debug, Clone)]
pub struct NewPluginOptions {
    /// Plugin id (`org.example.fmt`). Used in `manifest.toml`, the
    /// Rust crate name, and the package.json name (after slugify).
    pub plugin_id: String,
    /// Human display name (`"Format SQL"`).
    pub display_name: String,
    /// Optional one-line description.
    pub description: Option<String>,
    /// Author string for the manifest. Free-form.
    pub author: Option<String>,
    /// Initial version (defaults to `"0.1.0"` when omitted).
    pub version: Option<String>,
}

impl NewPluginOptions {
    /// Construct with sensible defaults derived from the plugin id.
    /// Tests use this; the binary call site sets all fields
    /// explicitly from CLI flags.
    #[must_use]
    pub fn from_id(plugin_id: impl Into<String>) -> Self {
        let id: String = plugin_id.into();
        let name = id
            .rsplit_once('.')
            .map(|(_, last)| last.to_string())
            .unwrap_or_else(|| id.clone());
        Self {
            plugin_id: id,
            display_name: humanize(&name),
            description: None,
            author: None,
            version: None,
        }
    }
}

/// Creates a new plugin directory at `target` populated with the
/// scaffold tree described in the module docs.
///
/// `target` MUST NOT already exist; the function refuses to clobber
/// existing trees + returns [`ScaffoldError::TargetExists`] if
/// `target.exists()` returns true. Callers wanting "overwrite" pass
/// a fresh directory.
///
/// # Errors
///
/// See [`ScaffoldError`].
pub fn scaffold_new_plugin(target: &Path, options: &NewPluginOptions) -> ScaffoldResult<()> {
    ensure_safe_plugin_id(&options.plugin_id)?;

    if target.exists() {
        return Err(ScaffoldError::TargetExists(target.to_path_buf()));
    }

    fs::create_dir_all(target)?;
    fs::create_dir_all(target.join("src"))?;

    let version = options.version.as_deref().unwrap_or("0.1.0");
    let description = options
        .description
        .as_deref()
        .unwrap_or("A Plamenix plugin scaffolded by `plamenix new`.");
    let author_line = options
        .author
        .as_deref()
        .map_or(String::new(), |a| format!("author = {}\n", quote_toml(a)));

    fs::write(
        target.join("manifest.toml"),
        manifest_template(
            &options.plugin_id,
            &options.display_name,
            version,
            description,
            &author_line,
        ),
    )?;
    fs::write(
        target.join("Cargo.toml"),
        cargo_template(&crate_name_for(&options.plugin_id), version),
    )?;
    fs::write(
        target.join("package.json"),
        package_json_template(&pkg_name_for(&options.plugin_id), version, description),
    )?;
    fs::write(target.join("src/lib.rs"), LIB_RS_TEMPLATE)?;
    fs::write(target.join("src/ui.tsx"), UI_TSX_TEMPLATE)?;
    fs::write(
        target.join("README.md"),
        readme_template(&options.display_name, &options.plugin_id),
    )?;

    Ok(())
}

/// Validates a plugin id against the scaffold's identifier grammar.
/// Same rules the loader enforces, lifted here so the CLI can fail
/// fast without spinning up a wasmtime engine.
///
/// Rules:
///   - Non-empty, ≤128 characters.
///   - Lowercase ASCII alphanumerics, plus `.`, `_`, `-`.
///   - First character must be alphanumeric.
///   - No consecutive dots (`..`).
///
/// # Errors
///
/// Returns [`ScaffoldError::InvalidId`] when any rule fails.
pub fn ensure_safe_plugin_id(id: &str) -> ScaffoldResult<()> {
    if id.is_empty() {
        return Err(ScaffoldError::InvalidId(id.to_string(), "id is empty"));
    }
    if id.len() > 128 {
        return Err(ScaffoldError::InvalidId(
            id.to_string(),
            "id exceeds 128 chars",
        ));
    }
    let mut chars = id.chars();
    let Some(first) = chars.next() else {
        return Err(ScaffoldError::InvalidId(id.to_string(), "id is empty"));
    };
    if !first.is_ascii_alphanumeric() {
        return Err(ScaffoldError::InvalidId(
            id.to_string(),
            "id must start with an ASCII alphanumeric character",
        ));
    }
    let mut prev = first;
    for ch in chars {
        if !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-')) {
            return Err(ScaffoldError::InvalidId(
                id.to_string(),
                "id contains an illegal character",
            ));
        }
        if ch == '.' && prev == '.' {
            return Err(ScaffoldError::InvalidId(
                id.to_string(),
                "id contains consecutive dots",
            ));
        }
        if ch.is_ascii_uppercase() {
            return Err(ScaffoldError::InvalidId(
                id.to_string(),
                "id must be lowercase",
            ));
        }
        prev = ch;
    }
    Ok(())
}

fn humanize(token: &str) -> String {
    if token.is_empty() {
        return "Untitled Plugin".to_string();
    }
    let mut out = String::new();
    let mut next_upper = true;
    for ch in token.chars() {
        if ch == '-' || ch == '_' {
            out.push(' ');
            next_upper = true;
        } else if next_upper {
            for u in ch.to_uppercase() {
                out.push(u);
            }
            next_upper = false;
        } else {
            out.push(ch);
        }
    }
    out
}

fn crate_name_for(plugin_id: &str) -> String {
    plugin_id.replace(['.', '-'], "_")
}

fn pkg_name_for(plugin_id: &str) -> String {
    plugin_id.replace('_', "-")
}

fn quote_toml(s: &str) -> String {
    // Simple TOML string quoting — doubles internal quotes the way
    // TOML basic strings expect.
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

// === Templates ===
//
// Loaded at compile time via `include_str!` so the binary stays a
// single executable. Edit the underlying files for IDE syntax
// highlighting + diff-friendly review.

const MANIFEST_TMPL: &str = include_str!("../templates/manifest.toml.tmpl");
const CARGO_TMPL: &str = include_str!("../templates/Cargo.toml.tmpl");
const PACKAGE_JSON_TMPL: &str = include_str!("../templates/package.json.tmpl");
const README_TMPL: &str = include_str!("../templates/README.md.tmpl");
const LIB_RS_TEMPLATE: &str = include_str!("../templates/lib.rs.tmpl");
const UI_TSX_TEMPLATE: &str = include_str!("../templates/ui.tsx.tmpl");

/// Renders a template by replacing `{{KEY}}` placeholders with the
/// supplied values. Placeholders are case-sensitive + must match
/// exactly. The function does a single linear `str::replace` pass per
/// pair; no `tera` / `handlebars` dependency.
#[must_use]
pub fn render_template(template: &str, replacements: &[(&str, &str)]) -> String {
    let mut out = template.to_string();
    for (key, value) in replacements {
        let needle = format!("{{{{{key}}}}}");
        out = out.replace(&needle, value);
    }
    out
}

fn manifest_template(
    plugin_id: &str,
    display_name: &str,
    version: &str,
    description: &str,
    author_line: &str,
) -> String {
    render_template(
        MANIFEST_TMPL,
        &[
            ("PLUGIN_ID", plugin_id),
            ("DISPLAY_NAME", display_name),
            ("VERSION", version),
            ("DESCRIPTION", description),
            ("AUTHOR_LINE", author_line),
        ],
    )
}

fn cargo_template(crate_name: &str, version: &str) -> String {
    render_template(
        CARGO_TMPL,
        &[("CRATE_NAME", crate_name), ("VERSION", version)],
    )
}

fn package_json_template(pkg_name: &str, version: &str, description: &str) -> String {
    render_template(
        PACKAGE_JSON_TMPL,
        &[
            ("PKG_NAME", pkg_name),
            ("VERSION", version),
            ("DESCRIPTION", description),
        ],
    )
}

fn readme_template(display_name: &str, plugin_id: &str) -> String {
    render_template(
        README_TMPL,
        &[("DISPLAY_NAME", display_name), ("PLUGIN_ID", plugin_id)],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn opts(id: &str) -> NewPluginOptions {
        NewPluginOptions::from_id(id)
    }

    #[test]
    fn scaffold_creates_expected_tree() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("my-plugin");
        scaffold_new_plugin(&target, &opts("org.example.fmt")).unwrap();
        for member in [
            "manifest.toml",
            "Cargo.toml",
            "package.json",
            "src/lib.rs",
            "src/ui.tsx",
            "README.md",
        ] {
            assert!(target.join(member).is_file(), "missing {member}");
        }
    }

    #[test]
    fn manifest_contains_id_and_default_version() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("p");
        scaffold_new_plugin(&target, &opts("test.scaffold")).unwrap();
        let manifest = std::fs::read_to_string(target.join("manifest.toml")).unwrap();
        assert!(manifest.contains(r#"id = "test.scaffold""#));
        assert!(manifest.contains(r#"version = "0.1.0""#));
    }

    #[test]
    fn manifest_parses_through_the_host_loader() {
        // End-to-end: a freshly-scaffolded manifest must satisfy the
        // production `Manifest::parse` validator.
        let dir = tempdir().unwrap();
        let target = dir.path().join("validate-me");
        scaffold_new_plugin(&target, &opts("integ.test")).unwrap();
        let manifest_text = std::fs::read_to_string(target.join("manifest.toml")).unwrap();
        let manifest = plamenix_plugin_host::Manifest::parse(&manifest_text).unwrap();
        assert_eq!(manifest.plugin.id, "integ.test");
    }

    #[test]
    fn refuses_to_clobber_existing_target() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("existing");
        std::fs::create_dir(&target).unwrap();
        let err = scaffold_new_plugin(&target, &opts("x.y")).unwrap_err();
        assert!(matches!(err, ScaffoldError::TargetExists(_)));
    }

    #[test]
    fn refuses_invalid_id() {
        let dir = tempdir().unwrap();
        for bad in [
            "",
            ".leading-dot",
            "double..dot",
            "UPPER",
            "has space",
            "has/slash",
        ] {
            let target = dir.path().join("_t").join(bad);
            let err = scaffold_new_plugin(&target, &opts(bad)).unwrap_err();
            assert!(
                matches!(err, ScaffoldError::InvalidId(_, _)),
                "expected InvalidId for {bad:?}",
            );
        }
    }

    #[test]
    fn cargo_template_uses_snake_case_crate_name() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("dotted");
        scaffold_new_plugin(&target, &opts("org.example.fmt")).unwrap();
        let cargo = std::fs::read_to_string(target.join("Cargo.toml")).unwrap();
        assert!(cargo.contains(r#"name = "org_example_fmt""#));
    }

    #[test]
    fn package_json_name_keeps_dashes() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("p");
        scaffold_new_plugin(&target, &opts("org.foo-bar")).unwrap();
        let pkg = std::fs::read_to_string(target.join("package.json")).unwrap();
        assert!(pkg.contains(r#""name": "org.foo-bar""#));
    }

    #[test]
    fn humanize_capitalises_and_strips_separators() {
        assert_eq!(humanize("fmt"), "Fmt");
        assert_eq!(humanize("hello-world"), "Hello World");
        assert_eq!(humanize("hello_world"), "Hello World");
    }

    #[test]
    fn render_template_substitutes_placeholders_exactly_once() {
        let out = render_template(
            "Hello {{NAME}}, version {{VERSION}}.",
            &[("NAME", "Plamenix"), ("VERSION", "1.0")],
        );
        assert_eq!(out, "Hello Plamenix, version 1.0.");
    }

    #[test]
    fn render_template_leaves_unknown_placeholders_alone() {
        let out = render_template("Has {{KNOWN}} and {{UNKNOWN}}.", &[("KNOWN", "yes")]);
        assert_eq!(out, "Has yes and {{UNKNOWN}}.");
    }

    #[test]
    fn render_template_replaces_every_occurrence() {
        let out = render_template("{{X}} + {{X}} = {{X}}{{X}}", &[("X", "1")]);
        assert_eq!(out, "1 + 1 = 11");
    }

    #[test]
    fn embedded_templates_match_on_disk_files() {
        // Regression guard for the I7.14 file-based templates: the
        // `include_str!` paths should resolve to the actual files
        // under templates/. We compare the embedded bytes to the
        // result of reading the files from CARGO_MANIFEST_DIR.
        let template_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("templates");
        for (embedded, filename) in [
            (MANIFEST_TMPL, "manifest.toml.tmpl"),
            (CARGO_TMPL, "Cargo.toml.tmpl"),
            (PACKAGE_JSON_TMPL, "package.json.tmpl"),
            (README_TMPL, "README.md.tmpl"),
            (LIB_RS_TEMPLATE, "lib.rs.tmpl"),
            (UI_TSX_TEMPLATE, "ui.tsx.tmpl"),
        ] {
            let on_disk = std::fs::read_to_string(template_dir.join(filename))
                .unwrap_or_else(|_| panic!("template file missing: {filename}"));
            assert_eq!(embedded, on_disk, "template drift in {filename}");
        }
    }
}
