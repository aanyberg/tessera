//! `cargo xtask config-docs`: renders `docs/config.md` from the
//! `tessera-config` types.
//!
//! Doc comments are not available at runtime, so descriptions come from the
//! JSON Schema that `schemars` derives for [`Config`] (behind
//! `tessera-config`'s `schema` feature), where each field's doc comment is its
//! `description`.
//!
//! Defaults come from `Config::default()` serialised to TOML. A key that is
//! not present there takes the `default` the schema records for it instead:
//! fields of an `[[array]]` entry (an entry's defaults are those of its type,
//! not of any entry `Config::default()` happens to contain) and fields of an
//! optional table that is unset by default. `schemars` derives those from the
//! containing type's `Default` impl, which is what `#[serde(default)]` fills a
//! partial table with on load. A key with neither (an `Option` that is `None`)
//! is documented as *unset*.
//!
//! Output is deterministic: keys and sections are sorted by name and nothing
//! depends on the clock or the environment.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::{fs, io};

use serde_json::{Map, Value};
use tessera_config::{APP_SUPPORT_DIR_NAME, Appearance, CONFIG_FILE_NAME, Config};

/// The generated document, relative to the repository root.
pub(crate) const DOC_PATH: &str = "docs/config.md";

/// Rendered in the *Default* column for a key that is absent by default.
const UNSET: &str = "*unset*";

/// Intra-doc links in descriptions that name a constant are replaced by the
/// constant's value, so the reference states the number rather than a Rust
/// path a user cannot look up. Links not listed here render as inline code.
fn known_constants() -> [(&'static str, String); 2] {
    [
        (
            "Appearance::MIN_FONT_SIZE",
            format!("{:?}", Appearance::MIN_FONT_SIZE),
        ),
        (
            "Appearance::MAX_FONT_SIZE",
            format!("{:?}", Appearance::MAX_FONT_SIZE),
        ),
    ]
}

/// Why `config-docs` could not run.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    /// Reading or writing the document failed.
    #[error("{}: {source}", path.display())]
    Io {
        /// The file involved.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// `Config::default()` could not be serialised to TOML.
    #[error("serialising Config::default() to TOML: {0}")]
    SerializeDefaults(#[from] toml::ser::Error),
    /// The TOML produced from `Config::default()` did not parse back.
    #[error("re-reading Config::default() as TOML: {0}")]
    ParseDefaults(#[from] toml::de::Error),
    /// The schema has a shape this generator does not know how to document.
    #[error("config key `{key}`: {message}")]
    Schema {
        /// Dotted path of the key, empty for the root.
        key: String,
        /// What is wrong.
        message: String,
    },
}

/// What a `config-docs` run did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The document was (re)written.
    Written,
    /// The document already matched; it was not touched.
    Unchanged,
    /// `--check`: the document matches.
    UpToDate,
    /// `--check`: the document differs from what would be generated.
    Stale {
        /// Unified diff from the checked-in document to the generated one.
        diff: String,
    },
}

/// Generates the document under `root` (the repository root), or with
/// `check` compares the checked-in document against it without writing.
pub(crate) fn run(root: &Path, check: bool) -> Result<Outcome, Error> {
    let path = root.join(DOC_PATH);
    let generated = render()?;
    let current = match fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(source) => return Err(Error::Io { path, source }),
    };
    if check {
        return Ok(
            match staleness_diff(current.as_deref().unwrap_or(""), &generated) {
                None => Outcome::UpToDate,
                Some(diff) => Outcome::Stale { diff },
            },
        );
    }
    if current.as_deref() == Some(generated.as_str()) {
        return Ok(Outcome::Unchanged);
    }
    fs::write(&path, generated).map_err(|source| Error::Io { path, source })?;
    Ok(Outcome::Written)
}

/// A unified diff from `current` to `generated`, or `None` when they match.
pub(crate) fn staleness_diff(current: &str, generated: &str) -> Option<String> {
    (current != generated).then(|| {
        similar::TextDiff::from_lines(current, generated)
            .unified_diff()
            .context_radius(3)
            .header(
                &format!("a/{DOC_PATH}"),
                &format!("b/{DOC_PATH} (generated)"),
            )
            .to_string()
    })
}

/// Renders the configuration reference as Markdown.
pub(crate) fn render() -> Result<String, Error> {
    let schema = schemars::schema_for!(Config);
    let defaults: toml::Table = toml::from_str(&Config::default().to_toml()?)?;
    let walker = Walker {
        defs: schema.get("$defs").and_then(Value::as_object),
        defaults: &defaults,
    };
    let sections = walker.sections(schema.as_value())?;
    Ok(render_document(&sections))
}

/// How a section's keys are written in TOML.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SectionKind {
    /// Keys at the top of the file, outside any table.
    Root,
    /// A `[table]`.
    Table {
        /// The table may be absent (an `Option` in Rust).
        optional: bool,
    },
    /// An `[[array.of.tables]]`; each entry is one block.
    ArrayOfTables,
}

/// One documented table: a heading, an introduction and a row per key.
#[derive(Debug)]
struct Section {
    /// Key path from the root; empty for [`SectionKind::Root`].
    path: Vec<String>,
    kind: SectionKind,
    /// The table's own description, cleaned for a paragraph.
    description: String,
    /// Rendered default of the table as a whole (arrays of tables only).
    default: Option<String>,
    /// Header of the nearest enclosing `[[array]]`, if any.
    within_array: Option<String>,
    rows: Vec<Row>,
}

/// One key of a table.
#[derive(Debug)]
struct Row {
    key: String,
    ty: String,
    default: String,
    description: String,
}

/// What a property's schema documents.
#[derive(Debug)]
enum Shape<'a> {
    /// A plain value; `ty` is its human-readable type.
    Leaf { ty: String },
    /// A nested table, with the resolved object schema.
    Table(&'a Value),
    /// An array of tables, with the resolved schema of one entry.
    ArrayOfTables(&'a Value),
}

/// A property's shape plus whether it may be absent (`Option`).
#[derive(Debug)]
struct Classified<'a> {
    shape: Shape<'a>,
    optional: bool,
}

/// Walks the schema of [`Config`], resolving `$ref`s against its `$defs`.
struct Walker<'a> {
    defs: Option<&'a Map<String, Value>>,
    /// `Config::default()` as a TOML table.
    defaults: &'a toml::Table,
}

/// Context threaded through the recursive walk.
#[derive(Debug, Clone)]
struct Place {
    path: Vec<String>,
    kind: SectionKind,
    /// The table sits inside an array of tables, so `Config::default()` does
    /// not describe its defaults.
    under_array: bool,
    within_array: Option<String>,
}

impl<'a> Walker<'a> {
    /// All sections, root first, then each table followed by its sub-tables.
    fn sections(&self, root: &'a Value) -> Result<Vec<Section>, Error> {
        let place = Place {
            path: Vec::new(),
            kind: SectionKind::Root,
            under_array: false,
            within_array: None,
        };
        let mut sections = self.walk(&place, root, String::new(), None)?;
        if sections.first().is_some_and(|root| root.rows.is_empty()) {
            sections.remove(0);
        }
        Ok(sections)
    }

    /// Documents the object schema `object` at `place`, returning its section
    /// followed by those of its nested tables.
    fn walk(
        &self,
        place: &Place,
        object: &'a Value,
        description: String,
        section_default: Option<String>,
    ) -> Result<Vec<Section>, Error> {
        let key = place.path.join(".");
        let properties = object
            .get("properties")
            .and_then(Value::as_object)
            .ok_or_else(|| schema_error(&key, "expected an object schema with properties"))?;
        let mut names: Vec<&String> = properties.keys().collect();
        names.sort();

        let mut rows = Vec::new();
        let mut children = Vec::new();
        for name in names {
            let property = &properties[name];
            let mut path = place.path.clone();
            path.push(name.clone());
            let dotted = path.join(".");
            let doc = property
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if doc.trim().is_empty() {
                return Err(schema_error(
                    &dotted,
                    "has no doc comment; document the field in crates/tessera-config",
                ));
            }
            let default = self.default_value(&path, property, place.under_array)?;
            let classified = self.classify(property, &dotted)?;
            let (schema, kind) = match classified.shape {
                Shape::Leaf { ty } => {
                    rows.push(Row {
                        key: name.clone(),
                        ty: if classified.optional {
                            format!("{ty}, optional")
                        } else {
                            ty
                        },
                        default: default.as_ref().map_or_else(|| UNSET.to_owned(), code),
                        description: cell(&clean_description(doc, "<br><br>")),
                    });
                    continue;
                }
                Shape::Table(schema) => (
                    schema,
                    SectionKind::Table {
                        optional: classified.optional,
                    },
                ),
                Shape::ArrayOfTables(schema) => (schema, SectionKind::ArrayOfTables),
            };
            let is_array = kind == SectionKind::ArrayOfTables;
            let child = Place {
                within_array: if is_array {
                    Some(header(&path, kind))
                } else {
                    place.within_array.clone()
                },
                under_array: place.under_array || is_array,
                path,
                kind,
            };
            let rendered_default = default.as_ref().map(code);
            if place.kind != SectionKind::Root {
                rows.push(Row {
                    key: name.clone(),
                    ty: match kind {
                        SectionKind::ArrayOfTables => "array of tables",
                        SectionKind::Table { optional: true } => "table, optional",
                        SectionKind::Table { optional: false } | SectionKind::Root => "table",
                    }
                    .to_owned(),
                    default: match (&rendered_default, is_array) {
                        (None, _) => UNSET.to_owned(),
                        (Some(value), true) => value.clone(),
                        (Some(_), false) => "*see below*".to_owned(),
                    },
                    description: cell(&format!(
                        "{} See {}.",
                        clean_description(doc, "<br><br>"),
                        section_link(&child.path, kind)
                    )),
                });
            }
            children.extend(self.walk(
                &child,
                schema,
                clean_description(doc, "\n\n"),
                rendered_default.filter(|_| is_array),
            )?);
        }

        let mut sections = vec![Section {
            path: place.path.clone(),
            kind: place.kind,
            description,
            default: section_default,
            within_array: place.within_array.clone(),
            rows,
        }];
        sections.extend(children);
        Ok(sections)
    }

    /// The default of the key at `path`: from `Config::default()` when the
    /// key is reachable there through tables alone, else the schema's
    /// `default`. `None` means the key is absent by default.
    fn default_value(
        &self,
        path: &[String],
        property: &Value,
        under_array: bool,
    ) -> Result<Option<toml::Value>, Error> {
        if !under_array && let Some(value) = lookup(self.defaults, path) {
            return Ok(Some(value.clone()));
        }
        match property.get("default") {
            None | Some(Value::Null) => Ok(None),
            Some(value) => toml::Value::try_from(value).map(Some).map_err(|error| {
                schema_error(
                    &path.join("."),
                    &format!("default {value} has no TOML form: {error}"),
                )
            }),
        }
    }

    /// Follows `$ref`s until a schema without one.
    fn resolve(&self, mut schema: &'a Value, key: &str) -> Result<&'a Value, Error> {
        for _ in 0..32 {
            let Some(reference) = schema.get("$ref").and_then(Value::as_str) else {
                return Ok(schema);
            };
            let name = reference
                .strip_prefix("#/$defs/")
                .ok_or_else(|| schema_error(key, &format!("unsupported $ref `{reference}`")))?;
            schema = self
                .defs
                .and_then(|defs| defs.get(name))
                .ok_or_else(|| schema_error(key, &format!("unresolved $ref `{reference}`")))?;
        }
        Err(schema_error(key, "$ref chain too deep"))
    }

    /// Classifies a property schema as a value, a table or an array of tables.
    fn classify(&self, property: &'a Value, key: &str) -> Result<Classified<'a>, Error> {
        let (inner, nullable_any_of) = strip_nullable_any_of(property);
        let schema = self.resolve(inner, key)?;
        let (types, nullable_type) = schema_types(schema);
        let optional = nullable_any_of || nullable_type;
        let shape = if is_table(schema) {
            Shape::Table(schema)
        } else if types.as_slice() == ["array"] {
            let items = self.resolve(
                schema
                    .get("items")
                    .ok_or_else(|| schema_error(key, "array schema without `items`"))?,
                key,
            )?;
            if is_table(items) {
                Shape::ArrayOfTables(items)
            } else {
                Shape::Leaf {
                    ty: self.value_type(schema, key)?,
                }
            }
        } else {
            Shape::Leaf {
                ty: self.value_type(schema, key)?,
            }
        };
        Ok(Classified { shape, optional })
    }

    /// Human-readable type of a value schema (not a table), ignoring
    /// nullability.
    fn value_type(&self, schema: &'a Value, key: &str) -> Result<String, Error> {
        let (inner, _) = strip_nullable_any_of(schema);
        let schema = self.resolve(inner, key)?;
        if let Some(values) = schema.get("enum").and_then(Value::as_array) {
            let literals = values
                .iter()
                .filter(|value| !value.is_null())
                .map(|value| {
                    toml::Value::try_from(value)
                        .map(|value| code(&value))
                        .map_err(|error| schema_error(key, &format!("enum value {value}: {error}")))
                })
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(format!("one of {}", literals.join(", ")));
        }
        let (types, _) = schema_types(schema);
        match types.as_slice() {
            ["string"] => Ok("string".to_owned()),
            ["boolean"] => Ok("boolean".to_owned()),
            ["integer"] => Ok("integer".to_owned()),
            ["number"] => Ok("float".to_owned()),
            ["array"] => {
                let items = schema
                    .get("items")
                    .ok_or_else(|| schema_error(key, "array schema without `items`"))?;
                Ok(format!("array of {}", self.value_type(items, key)?))
            }
            ["object"] => match schema.get("additionalProperties") {
                Some(values @ Value::Object(_)) => {
                    Ok(format!("table of {}", self.value_type(values, key)?))
                }
                _ => Err(schema_error(
                    key,
                    "object schema with neither properties nor typed additionalProperties",
                )),
            },
            other => Err(schema_error(
                key,
                &format!("unsupported schema type {other:?}; extend xtask/src/config_docs.rs"),
            )),
        }
    }
}

/// `anyOf: [X, {"type": "null"}]` (how `schemars` writes `Option<Struct>`)
/// becomes `(X, true)`; anything else is returned unchanged with `false`.
fn strip_nullable_any_of(schema: &Value) -> (&Value, bool) {
    let null = |s: &Value| s.get("type").and_then(Value::as_str) == Some("null");
    match schema
        .get("anyOf")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
    {
        Some([a, b]) if null(b) => (a, true),
        Some([a, b]) if null(a) => (b, true),
        _ => (schema, false),
    }
}

/// The schema's `type`s other than `"null"`, and whether `"null"` was one
/// of them (how `schemars` writes `Option<String>`).
fn schema_types(schema: &Value) -> (Vec<&str>, bool) {
    let all: Vec<&str> = match schema.get("type") {
        Some(Value::String(ty)) => vec![ty.as_str()],
        Some(Value::Array(types)) => types.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    let nullable = all.contains(&"null");
    (
        all.into_iter().filter(|ty| *ty != "null").collect(),
        nullable,
    )
}

/// An object schema with named properties, documented as a TOML table.
fn is_table(schema: &Value) -> bool {
    schema.get("properties").is_some_and(Value::is_object)
}

/// The value at `path` in `table`, descending through tables only.
fn lookup<'t>(table: &'t toml::Table, path: &[String]) -> Option<&'t toml::Value> {
    let (last, parents) = path.split_last()?;
    let mut table = table;
    for key in parents {
        table = table.get(key)?.as_table()?;
    }
    table.get(last)
}

fn schema_error(key: &str, message: &str) -> Error {
    Error::Schema {
        key: key.to_owned(),
        message: message.to_owned(),
    }
}

/// Collapses doc-comment line wrapping into single spaces, joins paragraphs
/// with `paragraph_separator` and rewrites rustdoc intra-doc links.
fn clean_description(raw: &str, paragraph_separator: &str) -> String {
    let paragraphs: Vec<String> = raw
        .split("\n\n")
        .map(|paragraph| paragraph.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|paragraph| !paragraph.is_empty())
        .collect();
    rewrite_intra_doc_links(&paragraphs.join(paragraph_separator))
}

/// Rewrites rustdoc intra-doc links (``[`Path`]``), which mean nothing
/// outside rustdoc: a known constant becomes its value, anything else plain
/// inline code. Ordinary Markdown links (``[`text`](url)``) are kept.
fn rewrite_intra_doc_links(text: &str) -> String {
    let constants = known_constants();
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("[`") {
        let after_open = &rest[start + 2..];
        let Some(end) = after_open.find("`]") else {
            break;
        };
        let target = &after_open[..end];
        let after_close = &after_open[end + 2..];
        out.push_str(&rest[..start]);
        if after_close.starts_with('(') || after_close.starts_with('[') {
            out.push_str(&rest[start..start + 2 + end + 2]);
        } else if let Some((_, value)) = constants.iter().find(|(name, _)| *name == target) {
            let _ = write!(out, "`{value}`");
        } else {
            let _ = write!(out, "`{target}`");
        }
        rest = after_close;
    }
    out.push_str(rest);
    out
}

/// Escapes text for a Markdown table cell.
fn cell(text: &str) -> String {
    text.replace('|', "\\|")
}

/// A TOML value as inline code, for a table cell.
fn code(value: &toml::Value) -> String {
    let text = cell(&value.to_string());
    if text.contains('`') {
        format!("`` {text} ``")
    } else {
        format!("`{text}`")
    }
}

/// A key as written in a TOML header: bare when possible, quoted otherwise.
fn toml_key(key: &str) -> String {
    let bare = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare {
        key.to_owned()
    } else {
        toml::Value::String(key.to_owned()).to_string()
    }
}

/// The TOML header of a section: `[a.b]` or `[[a.b]]`.
fn header(path: &[String], kind: SectionKind) -> String {
    let dotted = path
        .iter()
        .map(|key| toml_key(key))
        .collect::<Vec<_>>()
        .join(".");
    match kind {
        SectionKind::ArrayOfTables => format!("[[{dotted}]]"),
        SectionKind::Table { .. } | SectionKind::Root => format!("[{dotted}]"),
    }
}

/// The Markdown heading text of a section.
fn heading(section: &Section) -> String {
    match section.kind {
        SectionKind::Root => "Top-level keys".to_owned(),
        kind => format!("`{}`", header(&section.path, kind)),
    }
}

/// GitHub's anchor for a heading: lower case, punctuation dropped, spaces
/// to hyphens.
fn anchor(heading: &str) -> String {
    heading
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c.to_ascii_lowercase()),
            _ => None,
        })
        .collect()
}

/// A Markdown link to the section for `path`.
fn section_link(path: &[String], kind: SectionKind) -> String {
    let text = format!("`{}`", header(path, kind));
    let target = anchor(&text);
    format!("[{text}](#{target})")
}

fn render_document(sections: &[Section]) -> String {
    let mut out = String::new();
    // `write!` into a `String` cannot fail.
    let _ = writeln!(
        out,
        "<!-- Generated by `cargo xtask config-docs` from the types in crates/tessera-config. \
         Do not edit by hand: change the doc comments or defaults there, then run \
         `just config-docs`. CI fails when this file is out of date. -->"
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "# Configuration reference");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Tessera reads its settings from `~/Library/Application Support/{APP_SUPPORT_DIR_NAME}/{CONFIG_FILE_NAME}`, \
         or from `{CONFIG_FILE_NAME}` in the directory the `TESSERA_CONFIG_DIR` environment \
         variable names (unset by default; for tests and side-by-side installs). Starting Tessera \
         writes no configuration there: without the file the defaults apply, and the file is \
         written, with every default commented, only when the app is asked to open it for \
         editing (a file you create yourself is picked up within a few seconds and never \
         replaced). \
         Saved edits of the file, and of the theme file in use under `themes/`, apply to every \
         open window at once. Every key is optional: a missing key \
         takes the default listed here, and a missing table takes the defaults of all its keys. \
         Unknown keys produce a warning and are kept. A file that does not parse never stops \
         Tessera: the error is reported, and until the file is fixed the defaults apply at launch \
         and the last good configuration stays in effect while running."
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Defaults are written as TOML values; {UNSET} means the key is absent unless you set it."
    );
    let _ = writeln!(out);
    for section in sections {
        let indent = "  ".repeat(section.path.len().saturating_sub(1));
        let title = heading(section);
        let _ = writeln!(out, "{indent}- [{title}](#{})", anchor(&title));
    }
    for section in sections {
        render_section(&mut out, section);
    }
    out
}

fn render_section(out: &mut String, section: &Section) {
    let level = "#".repeat((section.path.len() + 1).clamp(2, 6));
    let _ = writeln!(out);
    let _ = writeln!(out, "{level} {}", heading(section));
    let _ = writeln!(out);
    if !section.description.is_empty() {
        let _ = writeln!(out, "{}", section.description);
        let _ = writeln!(out);
    }
    let mut notes = Vec::new();
    match section.kind {
        SectionKind::Root => {}
        SectionKind::Table { optional } => {
            if optional {
                notes.push(format!("Optional table; {UNSET} by default."));
            }
        }
        SectionKind::ArrayOfTables => {
            notes.push(format!(
                "An array of tables: write one `{}` block per entry.",
                header(&section.path, section.kind)
            ));
            if let Some(default) = &section.default {
                notes.push(format!("Default: {default}."));
            }
        }
    }
    if let Some(array) = &section.within_array
        && section.kind != SectionKind::ArrayOfTables
    {
        notes.push(format!("It belongs to the `{array}` entry it follows."));
    }
    if !notes.is_empty() {
        let _ = writeln!(out, "{}", notes.join(" "));
        let _ = writeln!(out);
    }
    let _ = writeln!(out, "| Key | Type | Default | Description |");
    let _ = writeln!(out, "|---|---|---|---|");
    for row in &section.rows {
        let _ = writeln!(
            out,
            "| `{}` | {} | {} | {} |",
            toml_key(&row.key),
            row.ty,
            row.default,
            row.description
        );
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tessera_config::Profile;

    use super::*;

    /// Every leaf key path in the schema of `Config`, found independently of
    /// the renderer: objects with properties and arrays of them are
    /// descended into, everything else is a leaf.
    fn leaf_paths(
        defs: &Map<String, Value>,
        schema: &Value,
        prefix: &[String],
    ) -> Vec<Vec<String>> {
        let deref = |schema: &Value| -> Value {
            let mut schema = schema.clone();
            if let Some(Value::Array(options)) = schema.get("anyOf").cloned()
                && let Some(non_null) = options.iter().find(|s| s["type"] != "null")
            {
                schema = non_null.clone();
            }
            while let Some(name) = schema
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|r| r.strip_prefix("#/$defs/"))
            {
                schema = defs[name].clone();
            }
            schema
        };
        let schema = deref(schema);
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            return properties
                .iter()
                .flat_map(|(key, property)| {
                    let mut path = prefix.to_vec();
                    path.push(key.clone());
                    leaf_paths(defs, property, &path)
                })
                .collect();
        }
        if schema["type"] == "array" {
            let items = deref(&schema["items"]);
            if items.get("properties").is_some() {
                return leaf_paths(defs, &items, prefix);
            }
        }
        vec![prefix.to_vec()]
    }

    /// Splits a Markdown table row on unescaped `|`.
    fn split_row(line: &str) -> Vec<String> {
        let inner = line.trim().trim_start_matches('|');
        let inner = inner.strip_suffix('|').unwrap_or(inner);
        let mut cells = vec![String::new()];
        let mut chars = inner.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\\' if chars.peek() == Some(&'|') => {
                    cells.last_mut().unwrap().push('|');
                    chars.next();
                }
                '|' => cells.push(String::new()),
                c => cells.last_mut().unwrap().push(c),
            }
        }
        cells.iter().map(|c| c.trim().to_owned()).collect()
    }

    /// `(section path, key) -> [type, default, description]` from the
    /// rendered document.
    fn parse_rows(doc: &str) -> BTreeMap<(String, String), Vec<String>> {
        let mut rows = BTreeMap::new();
        let mut section = None;
        for line in doc.lines() {
            if line.starts_with('#') {
                let title = line.trim_start_matches('#').trim();
                section = if title == "Top-level keys" {
                    Some(String::new())
                } else {
                    title
                        .strip_prefix('`')
                        .and_then(|t| t.strip_suffix('`'))
                        .map(|t| t.trim_matches(|c| c == '[' || c == ']').to_owned())
                };
            } else if line.starts_with("| `") {
                let cells = split_row(line);
                let key = cells[0].trim_matches('`').to_owned();
                let section = section.clone().expect("row outside a section");
                rows.insert((section, key), cells[1..].to_vec());
            }
        }
        rows
    }

    /// The value at `path` in `table`, taking the first entry of arrays.
    fn oracle_lookup(table: &toml::Table, path: &[String]) -> Option<toml::Value> {
        let mut value = toml::Value::Table(table.clone());
        for key in path {
            if let toml::Value::Array(entries) = value {
                value = entries.into_iter().next()?;
            }
            value = value.as_table()?.get(key)?.clone();
        }
        Some(value)
    }

    #[test]
    fn every_leaf_key_is_documented_with_its_default_and_description() {
        let schema = schemars::schema_for!(Config);
        let defs = schema.get("$defs").and_then(Value::as_object).unwrap();
        let leaves = leaf_paths(defs, schema.as_value(), &[]);
        assert!(leaves.len() >= 15, "suspiciously few leaf keys: {leaves:?}");

        // Defaults oracle: `Config::default()` plus one default entry for
        // every array of tables and every optional table set to its default,
        // so each leaf has a value unless it is a `None` by default.
        let oracle = Config {
            profiles: vec![Profile {
                appearance: Some(Appearance::default()),
                ..Profile::default()
            }],
            ..Config::default()
        };
        let oracle: toml::Table = toml::from_str(&oracle.to_toml().unwrap()).unwrap();

        let rows = parse_rows(&render().unwrap());
        for leaf in &leaves {
            let (key, parents) = leaf.split_last().unwrap();
            let section = parents.join(".");
            let dotted = leaf.join(".");
            let cells = rows
                .get(&(section, key.clone()))
                .unwrap_or_else(|| panic!("`{dotted}` is not documented"));
            let expected = oracle_lookup(&oracle, leaf)
                .map_or_else(|| UNSET.to_owned(), |value| format!("`{value}`"));
            assert_eq!(cells[1], expected, "default of `{dotted}`");
            assert!(!cells[0].is_empty(), "`{dotted}` has no type");
            assert!(!cells[2].is_empty(), "`{dotted}` has no description");
        }
    }

    #[test]
    fn documents_every_table_with_a_heading() {
        let doc = render().unwrap();
        for heading in [
            "## `[general]`",
            "## `[appearance]`",
            "## `[terminal]`",
            "## `[[profiles]]`",
            "### `[profiles.appearance]`",
            "## `[plugins]`",
        ] {
            assert!(
                doc.lines().any(|line| line == heading),
                "missing heading {heading}"
            );
        }
        assert!(doc.starts_with("<!-- Generated by `cargo xtask config-docs`"));
        let intra_doc_link = doc
            .match_indices("`]")
            .any(|(at, _)| !doc[at + 2..].starts_with('('));
        assert!(!intra_doc_link, "unrewritten intra-doc link in:\n{doc}");
        assert!(!doc.contains("MIN_FONT_SIZE"), "constant not resolved");
    }

    #[test]
    fn rendering_is_deterministic() {
        assert_eq!(render().unwrap(), render().unwrap());
    }

    #[test]
    fn checked_in_document_is_current() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let checked_in = fs::read_to_string(root.join(DOC_PATH)).unwrap_or_default();
        if let Some(diff) = staleness_diff(&checked_in, &render().unwrap()) {
            panic!("{DOC_PATH} is stale; run `cargo xtask config-docs`:\n{diff}");
        }
    }

    #[test]
    fn check_reports_a_unified_diff_for_an_altered_line() {
        let generated = render().unwrap();
        assert_eq!(staleness_diff(&generated, &generated), None);

        let altered = generated.replacen("| `font_family` |", "| `font_famly` |", 1);
        assert_ne!(altered, generated);
        let diff = staleness_diff(&altered, &generated).expect("drift is reported");
        assert!(diff.starts_with(&format!("--- a/{DOC_PATH}\n+++ b/{DOC_PATH} (generated)\n")));
        assert!(diff.contains("\n@@ "), "no hunk header:\n{diff}");
        assert!(diff.contains("\n-| `font_famly` |"), "{diff}");
        assert!(diff.contains("\n+| `font_family` |"), "{diff}");
    }

    #[test]
    fn run_writes_then_checks() {
        let dir = std::env::temp_dir().join(format!("xtask-config-docs-{}", std::process::id()));
        fs::create_dir_all(dir.join("docs")).unwrap();
        let path = dir.join(DOC_PATH);
        let _ = fs::remove_file(&path);

        assert!(matches!(run(&dir, true).unwrap(), Outcome::Stale { .. }));
        assert_eq!(run(&dir, false).unwrap(), Outcome::Written);
        assert_eq!(run(&dir, false).unwrap(), Outcome::Unchanged);
        assert_eq!(run(&dir, true).unwrap(), Outcome::UpToDate);

        let text = fs::read_to_string(&path).unwrap();
        fs::write(&path, text.replacen("Tessera Dark", "Tessera Light", 1)).unwrap();
        let Outcome::Stale { diff } = run(&dir, true).unwrap() else {
            panic!("an altered document must be stale");
        };
        assert!(diff.contains("Tessera Light") && diff.contains("Tessera Dark"));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn intra_doc_links_become_values_or_code() {
        assert_eq!(
            rewrite_intra_doc_links(
                "clamped to [`Appearance::MIN_FONT_SIZE`]..=[`Appearance::MAX_FONT_SIZE`]."
            ),
            "clamped to `4.0`..=`512.0`."
        );
        assert_eq!(
            rewrite_intra_doc_links("see [`ConfigStore`] and [`docs`](https://x)"),
            "see `ConfigStore` and [`docs`](https://x)"
        );
        assert_eq!(rewrite_intra_doc_links("unclosed [`x"), "unclosed [`x");
    }

    #[test]
    fn descriptions_are_unwrapped_and_escaped() {
        assert_eq!(
            clean_description("one\ntwo  three\n\nnext\n", "<br><br>"),
            "one two three<br><br>next"
        );
        assert_eq!(cell("a | b"), "a \\| b");
        assert_eq!(
            split_row("| `k` | a \\| b | `1` | d |"),
            ["`k`", "a | b", "`1`", "d"]
        );
    }

    #[test]
    fn unsupported_shapes_are_errors_not_guesses() {
        let walker = Walker {
            defs: None,
            defaults: &toml::Table::new(),
        };
        let schema = serde_json::json!({
            "properties": { "x": { "description": "d", "oneOf": [{"type": "string"}, {"type": "integer"}] } }
        });
        let place = Place {
            path: vec!["t".to_owned()],
            kind: SectionKind::Table { optional: false },
            under_array: false,
            within_array: None,
        };
        let error = walker
            .walk(&place, &schema, String::new(), None)
            .unwrap_err();
        assert!(
            matches!(&error, Error::Schema { key, .. } if key == "t.x"),
            "{error}"
        );

        let undocumented = serde_json::json!({ "properties": { "y": { "type": "string" } } });
        let error = walker
            .walk(&place, &undocumented, String::new(), None)
            .unwrap_err();
        assert!(error.to_string().contains("no doc comment"), "{error}");
    }

    #[test]
    fn keys_and_anchors() {
        assert_eq!(toml_key("font_size"), "font_size");
        assert_eq!(toml_key("a.b"), "\"a.b\"");
        assert_eq!(anchor("`[profiles.appearance]`"), "profilesappearance");
        assert_eq!(anchor("`[[profiles]]`"), "profiles");
        assert_eq!(anchor("Top-level keys"), "top-level-keys");
    }
}
