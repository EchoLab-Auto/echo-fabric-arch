//! `dump` — render a composed tree back to TOML with provenance comments.

use crate::{LoadedTree, ModuleEntry};

/// Render the composed entries as TOML text.
///
/// Each row is preceded by a provenance comment
/// `# --- <id> (layer <n>: <layer_source>) ---` naming the layer (applied
/// file) that last wrote it. The output is valid TOML and round-trips through
/// `toml::from_str`.
pub fn dump(tree: &LoadedTree) -> String {
    let mut out = String::new();
    for entry in &tree.entries {
        out.push_str(&format!(
            "# --- {} (layer {}: {}) ---\n",
            entry.id, entry.layer, entry.layer_source
        ));
        out.push_str(&render_entry(entry));
    }
    out
}

/// Render one row as a single-element `[[module]]` document.
fn render_entry(entry: &ModuleEntry) -> String {
    let mut row = toml::Table::new();
    row.insert("id".to_owned(), toml::Value::String(entry.id.clone()));
    row.insert(
        "kind".to_owned(),
        toml::Value::String(entry.kind.as_str().to_owned()),
    );
    row.insert("name".to_owned(), toml::Value::String(entry.name.clone()));
    if let Some(command) = &entry.command {
        row.insert("command".to_owned(), toml::Value::String(command.clone()));
    }
    if !entry.args.is_empty() {
        row.insert(
            "args".to_owned(),
            toml::Value::Array(
                entry
                    .args
                    .iter()
                    .cloned()
                    .map(toml::Value::String)
                    .collect(),
            ),
        );
    }
    if !entry.env.is_empty() {
        row.insert(
            "env".to_owned(),
            toml::Value::Table(
                entry
                    .env
                    .iter()
                    .map(|(key, value)| (key.clone(), toml::Value::String(value.clone())))
                    .collect(),
            ),
        );
    }
    // 资���限额（`[module.limits]`）：全空不渲染（与未写等价）。
    if let Some(limits) = &entry.limits {
        let mut table = toml::Table::new();
        for (key, value) in [
            ("cpu_seconds", limits.cpu_seconds),
            ("memory_mb", limits.memory_mb),
            ("file_size_mb", limits.file_size_mb),
            ("open_files", limits.open_files),
            ("processes", limits.processes),
        ] {
            if let Some(value) = value {
                table.insert(key.to_owned(), toml::Value::Integer(value as i64));
            }
        }
        if !table.is_empty() {
            row.insert("limits".to_owned(), toml::Value::Table(table));
        }
    }
    row.insert("enabled".to_owned(), toml::Value::Boolean(entry.enabled));
    if !entry.requires.is_empty() {
        row.insert(
            "requires".to_owned(),
            toml::Value::Array(
                entry
                    .requires
                    .iter()
                    .cloned()
                    .map(toml::Value::String)
                    .collect(),
            ),
        );
    }
    if let Some(config) = json_to_toml(&entry.config) {
        let empty_table = matches!(&config, toml::Value::Table(table) if table.is_empty());
        if !empty_table {
            row.insert("config".to_owned(), config);
        }
    }

    let mut root = toml::Table::new();
    root.insert(
        "module".to_owned(),
        toml::Value::Array(vec![toml::Value::Table(row)]),
    );
    let mut text = toml::to_string(&toml::Value::Table(root))
        .unwrap_or_else(|error| format!("# <unrenderable entry `{}`: {error}>\n", entry.id));
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// Convert a JSON value into a TOML value. `null` has no TOML counterpart and
/// maps to `None`, so object entries holding null are dropped.
fn json_to_toml(value: &serde_json::Value) -> Option<toml::Value> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::Bool(boolean) => Some(toml::Value::Boolean(*boolean)),
        serde_json::Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                Some(toml::Value::Integer(integer))
            } else {
                number.as_f64().map(toml::Value::Float)
            }
        }
        serde_json::Value::String(string) => Some(toml::Value::String(string.clone())),
        serde_json::Value::Array(items) => Some(toml::Value::Array(
            items.iter().filter_map(json_to_toml).collect(),
        )),
        serde_json::Value::Object(map) => Some(toml::Value::Table(
            map.iter()
                .filter_map(|(key, value)| {
                    json_to_toml(value).map(|converted| (key.clone(), converted))
                })
                .collect(),
        )),
    }
}
