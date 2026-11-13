//! Integration tests for `echo-module-loader`.
//!
//! Fixtures live in a unique subdirectory of the system temp dir and are
//! removed on drop — no external tempfile dependency.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use echo_module_loader::{dump, load, EntryKind, LoadError, LoadedTree, ModuleEntry};
use serde_json::json;

// ---------------------------------------------------------------- helpers --

/// A temp directory removed on drop.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!(
            "echo-module-loader-{tag}-{}-{nanos}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    /// Write a fixture file; returns its path.
    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.path.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixture dir");
        }
        std::fs::write(&path, contents).expect("write fixture");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn load_ok(profile: &Path) -> LoadedTree {
    load(profile).expect("profile should load")
}

fn load_err(profile: &Path) -> LoadError {
    load(profile).expect_err("profile should fail to load")
}

fn entry<'a>(tree: &'a LoadedTree, id: &str) -> &'a ModuleEntry {
    tree.entries
        .iter()
        .find(|entry| entry.id == id)
        .unwrap_or_else(|| panic!("entry `{id}` missing"))
}

fn ids(tree: &LoadedTree) -> Vec<&str> {
    tree.entries.iter().map(|entry| entry.id.as_str()).collect()
}

/// base.toml + qq.toml bundles, local.toml patch (replaces `websearch`),
/// profile inline `extra` row — the canonical three-layer fixture.
fn three_layer_fixture(tag: &str) -> (TempDir, PathBuf) {
    let dir = TempDir::new(tag);
    dir.write(
        "base.toml",
        r#"[[module]]
id = "tools"
kind = "builtin"
name = "tools"

[[module]]
id = "websearch"
kind = "stdio"
name = "echo-module-websearch"
command = "/opt/echo/modules/websearch"
args = ["--stdio"]
"#,
    );
    dir.write(
        "qq.toml",
        r#"[[module]]
id = "qq"
kind = "stdio"
name = "echo-module-qq"
command = "/opt/echo/modules/qq"
env = { QQ_ONE_BOT = "1" }
"#,
    );
    dir.write(
        "local.toml",
        r#"[[module]]
id = "websearch"
kind = "stdio"
name = "echo-module-websearch"
command = "/usr/local/bin/echo-module-websearch"
enabled = false
config = { api_key = "new" }
"#,
    );
    let profile = dir.write(
        "modules.toml",
        r#"[profile]
name = "default"
bundles = ["base", "qq"]
patches = ["local.toml"]

[[module]]
id = "extra"
kind = "builtin"
name = "extra"

[module.config]
api_key = "inline"
"#,
    );
    (dir, profile)
}

// ------------------------------------------------------------------ tests --

#[test]
fn three_layer_composition() {
    let (_dir, profile) = three_layer_fixture("compose");
    let tree = load_ok(&profile);

    // `websearch` keeps its bundle position (replace in place); `extra` is
    // appended by the profile's inline layer.
    assert_eq!(ids(&tree), vec!["tools", "websearch", "qq", "extra"]);
    assert!(tree.warnings.is_empty(), "warnings: {:?}", tree.warnings);

    let websearch = entry(&tree, "websearch");
    assert_eq!(websearch.kind, EntryKind::Stdio);
    assert_eq!(
        websearch.command.as_deref(),
        Some("/usr/local/bin/echo-module-websearch")
    );
    assert!(!websearch.enabled);
    assert!(
        websearch.args.is_empty(),
        "patch omitted args: old args must not survive"
    );
    assert_eq!(websearch.config, json!({ "api_key": "new" }));

    assert_eq!(
        entry(&tree, "qq").env.get("QQ_ONE_BOT").map(String::as_str),
        Some("1")
    );
    assert_eq!(entry(&tree, "extra").config, json!({ "api_key": "inline" }));
    assert_eq!(entry(&tree, "tools").kind, EntryKind::Builtin);
}

#[test]
fn provenance_records_layer_and_source() {
    let (_dir, profile) = three_layer_fixture("provenance");
    let tree = load_ok(&profile);

    let expected = [
        ("tools", 0usize, "base.toml"),
        ("qq", 1, "qq.toml"),
        ("websearch", 2, "local.toml"),
        ("extra", 3, "modules.toml"),
    ];
    for (id, layer, source) in expected {
        let row = entry(&tree, id);
        assert_eq!(row.layer, layer, "layer of `{id}`");
        assert_eq!(row.layer_source, source, "layer source of `{id}`");
    }
}

#[test]
fn patch_replaces_whole_row_without_field_residue() {
    let dir = TempDir::new("replace");
    dir.write(
        "bundle.toml",
        r#"[[module]]
id = "tools"
kind = "builtin"
name = "tools"

[[module]]
id = "websearch"
kind = "stdio"
name = "echo-module-websearch"
command = "/old/path"
args = ["--stdio", "--verbose"]
env = { TOKEN = "old" }
requires = ["tools"]
enabled = true

[module.config]
api_key = "old"
retries = 3
"#,
    );
    dir.write(
        "patch.toml",
        r#"[[module]]
id = "websearch"
kind = "stdio"
name = "echo-module-websearch"
command = "/new/path"
enabled = false
config = { api_key = "new" }
"#,
    );
    let profile = dir.write(
        "modules.toml",
        r#"[profile]
name = "replace"
bundles = ["bundle"]
patches = ["patch"]
"#,
    );

    let tree = load_ok(&profile);
    assert_eq!(ids(&tree), vec!["tools", "websearch"]);

    let websearch = entry(&tree, "websearch");
    assert_eq!(websearch.command.as_deref(), Some("/new/path"));
    assert!(!websearch.enabled);
    assert!(websearch.args.is_empty(), "old args must not survive");
    assert!(websearch.env.is_empty(), "old env must not survive");
    assert!(
        websearch.requires.is_empty(),
        "old requires must not survive"
    );
    assert_eq!(
        websearch.config,
        json!({ "api_key": "new" }),
        "config is replaced whole, not merged"
    );
    assert_eq!(
        websearch.layer, 1,
        "provenance moves to the replacing patch"
    );
    assert_eq!(websearch.layer_source, "patch.toml");
}

#[test]
fn stdio_without_command_is_a_validation_error() {
    let dir = TempDir::new("stdio-command");
    let profile = dir.write(
        "modules.toml",
        r#"[profile]
name = "broken"

[[module]]
id = "websearch"
kind = "stdio"
name = "echo-module-websearch"
"#,
    );
    match load_err(&profile) {
        LoadError::Validation { message } => {
            assert!(
                message.contains("websearch"),
                "message names the entry: {message}"
            );
            assert!(
                message.contains("command"),
                "message explains the requirement: {message}"
            );
        }
        other => panic!("expected Validation, got {other:?}"),
    }

    let blank = dir.write(
        "blank.toml",
        r#"[profile]
name = "broken"

[[module]]
id = "blank-cmd"
kind = "stdio"
name = "echo-module-blank"
command = ""
"#,
    );
    assert!(matches!(load_err(&blank), LoadError::Validation { .. }));
}

#[test]
fn invalid_ids_are_validation_errors() {
    let dir = TempDir::new("bad-ids");
    let empty = dir.write(
        "empty-id.toml",
        r#"[profile]
name = "t"

[[module]]
id = ""
kind = "builtin"
name = "x"
"#,
    );
    match load_err(&empty) {
        LoadError::Validation { message } => assert!(message.contains("empty id"), "{message}"),
        other => panic!("expected Validation, got {other:?}"),
    }

    let spaced = dir.write(
        "spaced-id.toml",
        r#"[profile]
name = "t"

[[module]]
id = "bad id"
kind = "builtin"
name = "x"
"#,
    );
    match load_err(&spaced) {
        LoadError::Validation { message } => {
            assert!(message.contains("whitespace"), "{message}")
        }
        other => panic!("expected Validation, got {other:?}"),
    }
}

#[test]
fn missing_bundle_reports_the_file_name() {
    let dir = TempDir::new("missing");
    let missing_bundle = dir.write(
        "bundles.toml",
        r#"[profile]
name = "t"
bundles = ["ghost", "base"]
"#,
    );
    dir.write(
        "base.toml",
        "[[module]]\nid = \"base\"\nkind = \"builtin\"\nname = \"base\"\n",
    );
    match load_err(&missing_bundle) {
        LoadError::MissingBundle { name } => assert_eq!(name, "ghost"),
        other => panic!("expected MissingBundle, got {other:?}"),
    }

    let missing_patch = dir.write(
        "patches.toml",
        r#"[profile]
name = "t"
patches = ["ghost-patch.toml"]
"#,
    );
    match load_err(&missing_patch) {
        LoadError::MissingBundle { name } => assert_eq!(name, "ghost-patch.toml"),
        other => panic!("expected MissingBundle, got {other:?}"),
    }
}

#[test]
fn unknown_requires_lists_all_missing_ids() {
    let dir = TempDir::new("unknown-requires");
    let profile = dir.write(
        "modules.toml",
        r#"[profile]
name = "t"

[[module]]
id = "gateway"
kind = "builtin"
name = "gateway"
requires = ["tools", "nope1", "nope2", "nope1"]

[[module]]
id = "tools"
kind = "builtin"
name = "tools"
"#,
    );
    match load_err(&profile) {
        LoadError::UnknownRequires { id, missing } => {
            assert_eq!(id, "gateway");
            assert_eq!(missing, vec!["nope1", "nope2"]);
        }
        other => panic!("expected UnknownRequires, got {other:?}"),
    }
}

#[test]
fn dependency_cycles_report_a_readable_chain() {
    let dir = TempDir::new("cycle");
    let profile = dir.write(
        "modules.toml",
        r#"[profile]
name = "t"

[[module]]
id = "a"
kind = "builtin"
name = "a"
requires = ["b"]

[[module]]
id = "b"
kind = "builtin"
name = "b"
requires = ["c"]

[[module]]
id = "c"
kind = "builtin"
name = "c"
requires = ["a"]
"#,
    );
    match load_err(&profile) {
        LoadError::Cycle { chain } => assert_eq!(chain, "a -> b -> c -> a"),
        other => panic!("expected Cycle, got {other:?}"),
    }

    let self_cycle = dir.write(
        "self.toml",
        r#"[profile]
name = "t"

[[module]]
id = "loop"
kind = "builtin"
name = "loop"
requires = ["loop"]
"#,
    );
    match load_err(&self_cycle) {
        LoadError::Cycle { chain } => assert_eq!(chain, "loop -> loop"),
        other => panic!("expected Cycle, got {other:?}"),
    }
}

#[test]
fn topological_sort_is_stable() {
    let dir = TempDir::new("topo");
    let dag = dir.write(
        "dag.toml",
        r#"[profile]
name = "t"

[[module]]
id = "websearch"
kind = "builtin"
name = "websearch"
requires = ["tools"]

[[module]]
id = "render"
kind = "builtin"
name = "render"
requires = ["websearch", "tools"]

[[module]]
id = "tools"
kind = "builtin"
name = "tools"

[[module]]
id = "solo"
kind = "builtin"
name = "solo"
"#,
    );
    let tree = load_ok(&dag);
    assert_eq!(ids(&tree), vec!["tools", "websearch", "render", "solo"]);

    // Without dependency relations, insertion order wins even when a chain
    // is declared after an unrelated row.
    let stable = dir.write(
        "stable.toml",
        r#"[profile]
name = "t"

[[module]]
id = "a"
kind = "builtin"
name = "a"

[[module]]
id = "b"
kind = "builtin"
name = "b"

[[module]]
id = "c"
kind = "builtin"
name = "c"
requires = ["a"]
"#,
    );
    assert_eq!(ids(&load_ok(&stable)), vec!["a", "b", "c"]);
}

#[test]
fn dump_marks_layers_and_lists_ids() {
    let (_dir, profile) = three_layer_fixture("dump");
    let tree = load_ok(&profile);
    let text = dump(&tree);

    for comment in [
        "# --- tools (layer 0: base.toml) ---",
        "# --- websearch (layer 2: local.toml) ---",
        "# --- qq (layer 1: qq.toml) ---",
        "# --- extra (layer 3: modules.toml) ---",
    ] {
        assert!(
            text.contains(comment),
            "missing provenance comment `{comment}` in:\n{text}"
        );
    }
    for id in ["tools", "websearch", "qq", "extra"] {
        assert!(
            text.contains(&format!("id = \"{id}\"")),
            "missing id `{id}` in:\n{text}"
        );
    }
    assert_eq!(text.matches("[[module]]").count(), 4);

    let reparsed: toml::Value = toml::from_str(&text).expect("dump output must be valid TOML");
    let rows = reparsed
        .get("module")
        .and_then(toml::Value::as_array)
        .expect("[[module]] rows");
    assert_eq!(rows.len(), tree.entries.len());
    let inline = rows
        .iter()
        .find(|row| row.get("id").and_then(toml::Value::as_str) == Some("extra"))
        .expect("extra row");
    assert_eq!(
        inline
            .get("config")
            .and_then(|config| config.get("api_key"))
            .and_then(toml::Value::as_str),
        Some("inline")
    );
}

#[test]
fn duplicate_id_in_one_layer_last_row_wins_with_warning() {
    let dir = TempDir::new("duplicate");
    let profile = dir.write(
        "modules.toml",
        r#"[profile]
name = "t"

[[module]]
id = "dup"
kind = "builtin"
name = "first"

[[module]]
id = "dup"
kind = "builtin"
name = "second"
"#,
    );
    let tree = load_ok(&profile);
    assert_eq!(tree.entries.len(), 1);
    assert_eq!(entry(&tree, "dup").name, "second");
    assert_eq!(tree.warnings.len(), 1, "warnings: {:?}", tree.warnings);
    assert!(tree.warnings[0].contains("duplicate"));
    assert!(tree.warnings[0].contains("dup"));
}

#[test]
fn patch_can_repair_a_bundle_row() {
    // Validation runs on the final composed tree, so a patch may fix a
    // bundle row that would not validate on its own.
    let dir = TempDir::new("repair");
    dir.write(
        "bundle.toml",
        r#"[[module]]
id = "websearch"
kind = "stdio"
name = "echo-module-websearch"
"#,
    );
    dir.write(
        "patch.toml",
        r#"[[module]]
id = "websearch"
kind = "stdio"
name = "echo-module-websearch"
command = "/fixed"
"#,
    );
    let profile = dir.write(
        "modules.toml",
        r#"[profile]
name = "t"
bundles = ["bundle"]
patches = ["patch"]
"#,
    );

    let tree = load_ok(&profile);
    assert_eq!(entry(&tree, "websearch").command.as_deref(), Some("/fixed"));
    assert_eq!(entry(&tree, "websearch").layer, 1);
}

#[test]
fn file_without_profile_section_still_loads_its_inline_rows() {
    let dir = TempDir::new("no-profile");
    let profile = dir.write(
        "modules.toml",
        r#"[[module]]
id = "solo"
kind = "builtin"
name = "solo"
"#,
    );
    let tree = load_ok(&profile);
    assert_eq!(ids(&tree), vec!["solo"]);
    assert_eq!(tree.entries[0].layer, 0);
    assert_eq!(tree.entries[0].layer_source, "modules.toml");
}

// ── 资源限额（`[module.limits]`）──────────────────────────────────────────

/// 限额解析进 `ModuleEntry`：五项各自独立可选，未写的键为 `None`。
#[test]
fn limits_parse_per_field() {
    let dir = TempDir::new("limits-parse");
    let profile = dir.write(
        "modules.toml",
        r#"
[[module]]
id = "ext"
kind = "stdio"
name = "ext"
command = "/bin/ext"

[module.limits]
memory_mb = 512
open_files = 256
"#,
    );
    let tree = load(&profile).expect("load");
    let limits = tree.entries[0].limits.expect("limits parsed");
    assert_eq!(limits.memory_mb, Some(512));
    assert_eq!(limits.open_files, Some(256));
    // 未写的键 = 不限制（不是 0）。
    assert_eq!(limits.cpu_seconds, None);
    assert_eq!(limits.file_size_mb, None);
    assert_eq!(limits.processes, None);
}

/// 空 `[module.limits]` 表 = 不限制（与不写等价），不是"限额全 0"。
#[test]
fn empty_limits_table_means_unlimited() {
    let dir = TempDir::new("limits-empty");
    let profile = dir.write(
        "modules.toml",
        r#"
[[module]]
id = "ext"
kind = "stdio"
name = "ext"
command = "/bin/ext"

[module.limits]
"#,
    );
    let tree = load(&profile).expect("load");
    assert!(
        tree.entries[0].limits.is_none(),
        "空表应归一成 None（否则语义变成「什么都不许」）"
    );
}

/// `0` 被拒绝而不是被当成"不限制"：`setrlimit` 里 0 = 完全禁止。
#[test]
fn zero_limits_are_rejected_with_actionable_message() {
    let dir = TempDir::new("limits-zero");
    let profile = dir.write(
        "modules.toml",
        r#"
[[module]]
id = "ext"
kind = "stdio"
name = "ext"
command = "/bin/ext"

[module.limits]
memory_mb = 0
"#,
    );
    let error = load(&profile).expect_err("0 must be rejected");
    let text = error.to_string();
    assert!(text.contains("memory_mb"), "{text}");
    assert!(text.contains("不限制"), "错误文本要给出改法：{text}");
}

/// 限额经 `dump()` 原样回渲染（排查时能看到生效值）。
#[test]
fn limits_round_trip_through_dump() {
    let dir = TempDir::new("limits-dump");
    let profile = dir.write(
        "modules.toml",
        r#"
[[module]]
id = "ext"
kind = "stdio"
name = "ext"
command = "/bin/ext"

[module.limits]
cpu_seconds = 60
memory_mb = 128
"#,
    );
    let tree = load(&profile).expect("load");
    let text = dump(&tree);
    assert!(text.contains("cpu_seconds = 60"), "{text}");
    assert!(text.contains("memory_mb = 128"), "{text}");
    // 回灌：dump 的输出必须还能被解析（dump 的契约）。
    let reparsed: toml::Value = toml::from_str(&text).expect("valid toml");
    assert!(reparsed["module"][0]["limits"].is_table());
}

// ------------------------------------------------------ modules-base.toml --

/// 基线文件：与 profile 同目录的 `modules-base.toml` 自动成为第 0 层。
///
/// 语义要点（改这组用例前先读 `BASE_FILE_NAME` 的文档）：
/// - 存在才应用——老部署没有这个文件时行为一字不变；
/// - 排在所有 bundle/patch 之前，所以 profile 能覆盖基线，反之不能；
/// - 行的来源（`layer_source`）指向基线文件名，面板「这行来自哪」靠它。
#[test]
fn base_file_is_applied_as_layer_zero() {
    let dir = TempDir::new("base-layer");
    dir.write(
        "modules-base.toml",
        r#"[[module]]
id = "tools"
kind = "builtin"
name = "内置工具集"
"#,
    );
    let profile = dir.write(
        "modules.toml",
        r#"[[module]]
id = "qq"
kind = "stdio"
name = "echo-module-qq"
command = "/opt/echo/modules/qq"
"#,
    );

    let tree = load_ok(&profile);
    assert_eq!(ids(&tree), vec!["tools", "qq"]);

    let tools = entry(&tree, "tools");
    assert_eq!(tools.layer, 0, "基线必须是第 0 层");
    assert_eq!(tools.layer_source, "modules-base.toml");
    assert_eq!(tree.entries[1].layer, 1, "profile 行排在基线之后");
}

/// 没有基线文件 = 回到原来的行为（这条是向后兼容的锚点）。
#[test]
fn missing_base_file_changes_nothing() {
    let dir = TempDir::new("no-base");
    let profile = dir.write(
        "modules.toml",
        r#"[[module]]
id = "qq"
kind = "stdio"
name = "echo-module-qq"
command = "/opt/echo/modules/qq"
"#,
    );
    let tree = load_ok(&profile);
    assert_eq!(ids(&tree), vec!["qq"]);
    assert_eq!(tree.entries[0].layer, 0);
    assert_eq!(tree.entries[0].layer_source, "modules.toml");
}

/// profile 覆盖基线：同 id 整行替换，**位置留在基线那一格**。
///
/// 位置保留是刻意的——覆盖一个模块的开关不该把它挪到列表末尾，否则挂载
/// 顺序会随无关的配置改动漂移。
#[test]
fn profile_overrides_base_entry_in_place() {
    let dir = TempDir::new("base-override");
    dir.write(
        "modules-base.toml",
        r#"[[module]]
id = "tools"
kind = "builtin"
name = "内置工具集"

[[module]]
id = "skills"
kind = "builtin"
name = "技能目录"
"#,
    );
    let profile = dir.write(
        "modules.toml",
        r#"[[module]]
id = "tools"
kind = "builtin"
name = "内置工具集"
enabled = false
"#,
    );

    let tree = load_ok(&profile);
    assert_eq!(ids(&tree), vec!["tools", "skills"], "覆盖不该换位置");
    let tools = entry(&tree, "tools");
    assert!(!tools.enabled, "profile 的 enabled 生效");
    assert_eq!(tools.layer, 1, "生效的是 profile 那一层");
    assert!(entry(&tree, "skills").enabled, "未被覆盖的基线行保持自己的开关");
}

/// 基线里的配置对 profile 不可见：整行替换不保留任何字段（同 patch 语义）。
#[test]
fn profile_override_does_not_inherit_base_config() {
    let dir = TempDir::new("base-no-inherit");
    dir.write(
        "modules-base.toml",
        r#"[[module]]
id = "prodoc"
kind = "builtin"
name = "ProDoc 文档群"
config = { roots = ["/a"] }
"#,
    );
    let profile = dir.write(
        "modules.toml",
        r#"[[module]]
id = "prodoc"
kind = "builtin"
name = "ProDoc 文档群"
"#,
    );

    let tree = load_ok(&profile);
    assert_eq!(
        entry(&tree, "prodoc").config,
        json!({}),
        "整行替换意味着 profile 必须自己写全套配置——半覆盖会留下陈旧键"
    );
}

/// 直接以 `modules-base.toml` 当 profile 时不重复应用同一份文件。
#[test]
fn base_file_as_profile_is_not_applied_twice() {
    let dir = TempDir::new("base-as-profile");
    let profile = dir.write(
        "modules-base.toml",
        r#"[[module]]
id = "tools"
kind = "builtin"
name = "内置工具集"
"#,
    );
    let tree = load_ok(&profile);
    assert_eq!(ids(&tree), vec!["tools"]);
    assert_eq!(tree.entries[0].layer, 0);
}
