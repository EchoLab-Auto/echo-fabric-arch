//! Composition engine: layered application, validation, dependency ordering.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{EntryKind, LoadError, LoadedTree, ModuleEntry, ModuleLimits};

/// 基线文件名：与 profile（`modules.toml`）**同目录**时自动作为第 0 层应用。
///
/// ## 为什么需要它
///
/// Core 编译进去的模块在此之前是**隐式集合**——`modules.toml` 不提它们，
/// 它们照样挂载（见 `resolve_mount_order` 的第 3 条）。隐式的好处是「升级
/// 不会因为配置文件过时而少挂一个模块」，坏处是**没人能一眼看出到底装了
/// 什么、按什么顺序**：要看清单得去面板里翻，想调顺序得凭猜。
///
/// `modules-base.toml` 把这份集合变成可读的文本：宿主把「我带了哪些模块、
/// 什么顺序、默认开不开」写进基线；部署方在 `modules.toml` 里只写**追加**
/// 的东西（外部仓模块、子进程模块）与**覆盖**（改某个内置模块的开关/配置）。
///
/// ## 为什么是「自动应用」而不是「在 profile 里写 bundles」
///
/// 写 `bundles = ["modules-base"]` 等于要求每个部署方手动接线，而且老部署
/// （没有这一行）会静默不生效——一个默认不启用的「基线」不是基线。自动
/// 应用则让**新旧部署行为一致**：文件在就用，不在就回到原来的隐式行为。
///
/// 它排在任何 bundle 之前，因为基线描述的是「宿主自带什么」这个前提。
pub const BASE_FILE_NAME: &str = "modules-base.toml";

/// One parsed file: an optional `[profile]` section plus `[[module]]` rows.
#[derive(Debug, Deserialize)]
struct RawDoc {
    /// Only meaningful in the entry (profile) file; bundle/patch files omit it.
    profile: Option<RawProfile>,
    /// Rows in file order.
    #[serde(default)]
    module: Vec<RawEntry>,
}

/// The `[profile]` section of the entry file.
#[derive(Debug, Default, Deserialize)]
struct RawProfile {
    /// Informational profile name; the loader records it nowhere yet.
    #[allow(dead_code)]
    #[serde(default)]
    name: String,
    /// Bundle references, applied in list order (layers `0..`).
    #[serde(default)]
    bundles: Vec<String>,
    /// Patch references, applied after all bundles.
    #[serde(default)]
    patches: Vec<String>,
}

/// One raw `[[module]]` row exactly as written in a file.
#[derive(Debug, Deserialize)]
struct RawEntry {
    id: String,
    kind: EntryKind,
    name: String,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default = "default_enabled")]
    enabled: bool,
    #[serde(default)]
    requires: Vec<String>,
    /// Free-form config as a TOML value; converted to JSON on load.
    #[serde(default)]
    config: Option<toml::Value>,
    /// 子进程资源限额（可选；见 [`ModuleLimits`]）。
    #[serde(default)]
    limits: Option<ModuleLimits>,
}

fn default_enabled() -> bool {
    true
}

impl RawEntry {
    fn into_entry(self, layer: usize, source: &str) -> ModuleEntry {
        ModuleEntry {
            id: self.id,
            kind: self.kind,
            name: self.name,
            command: self.command,
            args: self.args,
            env: self.env,
            enabled: self.enabled,
            requires: self.requires,
            config: self
                .config
                .map(toml_to_json)
                .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new())),
            layer,
            layer_source: source.to_owned(),
            // 全空（`[module.limits]` 写了空表）= 不限制，与未写等价。
            limits: self.limits.filter(|limits| !limits.is_empty()),
        }
    }
}

/// Load, compose, validate, and topologically order the tree described by
/// `profile_path`.
///
/// Bundle/patch references resolve relative to the profile's directory; a
/// missing `.toml` extension is appended when the reference has none.
///
/// A sibling [`BASE_FILE_NAME`] is applied as layer 0 before anything the
/// profile names — see the constant's docs for why.
pub fn load(profile_path: &Path) -> Result<LoadedTree, LoadError> {
    let mut doc = read_doc(profile_path)?;
    let profile = doc.profile.take().unwrap_or_default();
    let base_dir = profile_path.parent().unwrap_or_else(|| Path::new("."));

    let mut composer = Composer::default();
    let mut layer = 0usize;

    // 基线层：与 profile 同目录的 `modules-base.toml`（存在才用）。
    //
    // 位置在**所有** bundle/patch 之前，因为它是「宿主自带什么」的声明，
    // 部署方的 profile 与 patch 都是对这份基线的追加/覆盖。
    //
    // 同一路径要排除：有人直接以 `modules-base.toml` 作为 profile 时，
    // 不排除会把同一份文件应用两遍（第二次把第一次的行整体替换，结果
    // 只是浪费，但 layer 编号与来源会错）。
    if profile_path.file_name().map(|n| n != BASE_FILE_NAME).unwrap_or(true) {
        let base_path = base_dir.join(BASE_FILE_NAME);
        if base_path.is_file() {
            composer.apply(read_doc(&base_path)?, layer, BASE_FILE_NAME);
            layer += 1;
        }
    }

    for name in &profile.bundles {
        apply_reference(&mut composer, base_dir, name, &mut layer)?;
    }
    for name in &profile.patches {
        apply_reference(&mut composer, base_dir, name, &mut layer)?;
    }
    if !doc.module.is_empty() {
        composer.apply(doc, layer, &file_name(profile_path));
    }

    composer.finish()
}

/// Read and parse one file; a missing file surfaces as [`LoadError::Io`] (the
/// missing-bundle check is done by the caller, which knows the reference).
fn read_doc(path: &Path) -> Result<RawDoc, LoadError> {
    let text = std::fs::read_to_string(path).map_err(|source| LoadError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&text).map_err(|error| LoadError::Parse {
        path: path.to_path_buf(),
        message: error.to_string(),
    })
}

/// Apply one bundle/patch reference as the next layer.
fn apply_reference(
    composer: &mut Composer,
    base_dir: &Path,
    name: &str,
    layer: &mut usize,
) -> Result<(), LoadError> {
    let path = resolve_reference(base_dir, name);
    if !path.exists() {
        return Err(LoadError::MissingBundle {
            name: name.to_owned(),
        });
    }
    composer.apply(read_doc(&path)?, *layer, &file_name(&path));
    *layer += 1;
    Ok(())
}

/// Resolve a bundle/patch reference: relative to `base_dir`, `.toml` appended
/// when the name carries no extension.
fn resolve_reference(base_dir: &Path, name: &str) -> PathBuf {
    let candidate = PathBuf::from(name);
    let mut path = if candidate.is_absolute() {
        candidate
    } else {
        base_dir.join(candidate)
    };
    if path.extension().is_none() {
        path.set_extension("toml");
    }
    path
}

/// File name of a resolved path (used as `layer_source`).
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Accumulates rows across layers. Replacing an existing id keeps that row's
/// position, so patches do not reshuffle unrelated entries.
#[derive(Debug, Default)]
struct Composer {
    entries: Vec<ModuleEntry>,
    warnings: Vec<String>,
}

impl Composer {
    /// Apply one file's rows as `layer`: locate by id, replace whole rows.
    fn apply(&mut self, doc: RawDoc, layer: usize, source: &str) {
        for raw in doc.module {
            let entry = raw.into_entry(layer, source);
            match self
                .entries
                .iter()
                .position(|existing| existing.id == entry.id)
            {
                Some(pos) => {
                    if self.entries[pos].layer == layer {
                        self.warnings.push(format!(
                            "duplicate module id `{}` in layer {layer} ({source}): later row replaced the earlier one",
                            entry.id
                        ));
                    }
                    self.entries[pos] = entry;
                }
                None => self.entries.push(entry),
            }
        }
    }

    /// Validate the composed tree (final state, after all layers) and return
    /// it topologically sorted.
    fn finish(mut self) -> Result<LoadedTree, LoadError> {
        validate(&self.entries)?;
        let entries = topo_sort(std::mem::take(&mut self.entries))?;
        Ok(LoadedTree {
            entries,
            warnings: self.warnings,
        })
    }
}

/// Row-level validation of the composed tree.
///
/// Runs on the *final* tree, so a patch may repair (or break) a bundle row.
fn validate(entries: &[ModuleEntry]) -> Result<(), LoadError> {
    for entry in entries {
        if entry.id.is_empty() {
            return Err(LoadError::Validation {
                message: format!(
                    "module entry in `{}` (layer {}) has an empty id",
                    entry.layer_source, entry.layer
                ),
            });
        }
        if entry.id.chars().any(char::is_whitespace) {
            return Err(LoadError::Validation {
                message: format!("module id `{}` must not contain whitespace", entry.id),
            });
        }
    }

    for entry in entries {
        if entry.kind == EntryKind::Stdio {
            let has_command = entry
                .command
                .as_deref()
                .is_some_and(|command| !command.trim().is_empty());
            if !has_command {
                return Err(LoadError::Validation {
                    message: format!(
                        "module `{}` (kind = \"stdio\") requires a non-empty `command`",
                        entry.id
                    ),
                });
            }
        }
    }

    // 限额：0 是**无意义值**（`setrlimit` 里 0 表示"什么都不许"，不是
    // "不限制"），写在配置里的人几乎一定是想要后者。拒绝而不是猜意图。
    for entry in entries {
        let Some(limits) = &entry.limits else {
            continue;
        };
        for (name, value) in [
            ("cpu_seconds", limits.cpu_seconds),
            ("memory_mb", limits.memory_mb),
            ("file_size_mb", limits.file_size_mb),
            ("open_files", limits.open_files),
            ("processes", limits.processes),
        ] {
            if value == Some(0) {
                return Err(LoadError::Validation {
                    message: format!(
                        "module `{}` has `limits.{name} = 0` — 0 表示\"完全禁止\"而不是\"不限制\"；要放开请删掉该行",
                        entry.id
                    ),
                });
            }
        }
    }

    let known: HashSet<&str> = entries.iter().map(|entry| entry.id.as_str()).collect();
    for entry in entries {
        let mut missing: Vec<String> = Vec::new();
        for required in &entry.requires {
            if !known.contains(required.as_str()) && !missing.contains(required) {
                missing.push(required.clone());
            }
        }
        if !missing.is_empty() {
            return Err(LoadError::UnknownRequires {
                id: entry.id.clone(),
                missing,
            });
        }
    }

    Ok(())
}

/// Stable topological sort by `requires`.
///
/// Rows whose dependencies are all satisfied become ready; among ready rows
/// the one inserted earliest wins, so rows without dependency relations keep
/// their insertion order. A leftover row set always contains a cycle (each
/// leftover row still has an unsatisfied dependency inside the set).
fn topo_sort(entries: Vec<ModuleEntry>) -> Result<Vec<ModuleEntry>, LoadError> {
    let count = entries.len();
    let index: HashMap<&str, usize> = entries
        .iter()
        .enumerate()
        .map(|(position, entry)| (entry.id.as_str(), position))
        .collect();

    // deps[i] = positions of the rows that row i requires.
    let mut deps: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); count];
    for (position, entry) in entries.iter().enumerate() {
        for required in &entry.requires {
            if let Some(&dependency) = index.get(required.as_str()) {
                deps[position].insert(dependency);
            }
        }
    }

    let mut indegree = vec![0usize; count];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); count];
    for (position, node_deps) in deps.iter().enumerate() {
        for &dependency in node_deps {
            indegree[position] += 1;
            dependents[dependency].push(position);
        }
    }

    let mut ready: BinaryHeap<Reverse<usize>> = (0..count)
        .filter(|&position| indegree[position] == 0)
        .map(Reverse)
        .collect();
    let mut order: Vec<usize> = Vec::with_capacity(count);
    while let Some(Reverse(position)) = ready.pop() {
        order.push(position);
        for &dependent in &dependents[position] {
            indegree[dependent] -= 1;
            if indegree[dependent] == 0 {
                ready.push(Reverse(dependent));
            }
        }
    }

    if order.len() != count {
        let placed: HashSet<usize> = order.iter().copied().collect();
        let remaining: Vec<usize> = (0..count)
            .filter(|position| !placed.contains(position))
            .collect();
        let cycle = find_cycle(&remaining, &deps, &entries);
        let nodes = if cycle.is_empty() { &remaining } else { &cycle };
        let chain = nodes
            .iter()
            .map(|&position| entries[position].id.as_str())
            .collect::<Vec<_>>()
            .join(" -> ");
        return Err(LoadError::Cycle { chain });
    }

    let mut slots: Vec<Option<ModuleEntry>> = entries.into_iter().map(Some).collect();
    Ok(order
        .into_iter()
        .map(|position| {
            slots[position]
                .take()
                .expect("topological order visits each position exactly once")
        })
        .collect())
}

/// Find one cycle among `remaining` rows (each of which has an unsatisfied
/// dependency inside the set), as a closed chain rotated to start at the
/// smallest id. Returns an empty vector if the invariant is ever violated.
fn find_cycle(
    remaining: &[usize],
    deps: &[BTreeSet<usize>],
    entries: &[ModuleEntry],
) -> Vec<usize> {
    let in_remaining: HashSet<usize> = remaining.iter().copied().collect();
    let mut state = vec![0u8; entries.len()]; // 0 = unseen, 1 = on stack, 2 = done
    let mut stack: Vec<usize> = Vec::new();
    for &start in remaining {
        if state[start] == 0 {
            if let Some(cycle) = dfs_cycle(start, deps, &in_remaining, &mut state, &mut stack) {
                return rotate_to_smallest(cycle, entries);
            }
        }
    }
    Vec::new()
}

/// DFS over the remaining subgraph; on hitting a node that is still on the
/// stack, return the closed chain `[.., repeat]`.
fn dfs_cycle(
    node: usize,
    deps: &[BTreeSet<usize>],
    in_remaining: &HashSet<usize>,
    state: &mut [u8],
    stack: &mut Vec<usize>,
) -> Option<Vec<usize>> {
    state[node] = 1;
    stack.push(node);
    for &dependency in &deps[node] {
        if !in_remaining.contains(&dependency) {
            continue;
        }
        match state[dependency] {
            0 => {
                if let Some(cycle) = dfs_cycle(dependency, deps, in_remaining, state, stack) {
                    return Some(cycle);
                }
            }
            1 => {
                let start = stack
                    .iter()
                    .position(|&position| position == dependency)
                    .expect("a node in state 1 is always on the stack");
                let mut cycle: Vec<usize> = stack[start..].to_vec();
                cycle.push(dependency);
                return Some(cycle);
            }
            _ => {}
        }
    }
    stack.pop();
    state[node] = 2;
    None
}

/// Rotate a closed chain `[n1, n2, .., nk, n1]` so it starts at the smallest
/// id — deterministic cycle reports regardless of where the DFS entered.
fn rotate_to_smallest(cycle: Vec<usize>, entries: &[ModuleEntry]) -> Vec<usize> {
    if cycle.len() < 2 {
        return cycle;
    }
    let body = &cycle[..cycle.len() - 1];
    let min_pos = body
        .iter()
        .enumerate()
        .min_by_key(|(_, &position)| entries[position].id.as_str())
        .map(|(position, _)| position)
        .unwrap_or(0);
    let mut rotated = Vec::with_capacity(cycle.len());
    rotated.extend_from_slice(&body[min_pos..]);
    rotated.extend_from_slice(&body[..min_pos]);
    rotated.push(rotated[0]);
    rotated
}

/// Convert a TOML value (a `config` table) into JSON.
///
/// TOML has no JSON counterpart for datetimes, so they become RFC 3339
/// strings; TOML floats that JSON cannot represent (`nan`/`inf`) become null.
fn toml_to_json(value: toml::Value) -> serde_json::Value {
    match value {
        toml::Value::String(string) => serde_json::Value::String(string),
        toml::Value::Integer(integer) => serde_json::Value::Number(integer.into()),
        toml::Value::Float(float) => serde_json::Number::from_f64(float)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        toml::Value::Boolean(boolean) => serde_json::Value::Bool(boolean),
        toml::Value::Datetime(datetime) => serde_json::Value::String(datetime.to_string()),
        toml::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(toml_to_json).collect())
        }
        toml::Value::Table(table) => serde_json::Value::Object(
            table
                .into_iter()
                .map(|(key, value)| (key, toml_to_json(value)))
                .collect(),
        ),
    }
}
