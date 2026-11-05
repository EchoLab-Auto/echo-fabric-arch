//! Public data model: entry kinds, composed rows, and the loaded tree.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// How a module instance is launched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    /// Inline builtin: resolved from the composition root's builtin registry.
    Builtin,
    /// Standalone process speaking the module protocol over stdio.
    Stdio,
    /// Dynamically loaded library (experimental track).
    Dylib,
    /// WebAssembly module (experimental track).
    Wasm,
}

impl EntryKind {
    /// Lowercase config spelling (`builtin` / `stdio` / `dylib` / `wasm`).
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::Builtin => "builtin",
            EntryKind::Stdio => "stdio",
            EntryKind::Dylib => "dylib",
            EntryKind::Wasm => "wasm",
        }
    }
}

/// One composed row: how to launch and configure a single module instance.
///
/// Distinct from `echo_module::ModuleManifest` (what a module *is*): a row
/// describes how the host starts it (inline builtin / standalone stdio
/// process / dylib / wasm) and carries its config.
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleEntry {
    /// Stable lookup key used by the patch semantics (`按 id 定位`).
    pub id: String,
    /// Launch mechanism.
    pub kind: EntryKind,
    /// For `builtin`: the module id to mount; for `stdio`: the executable name.
    pub name: String,
    /// Executable path; required (non-empty) when `kind == Stdio`.
    pub command: Option<String>,
    /// Extra command-line arguments.
    pub args: Vec<String>,
    /// Extra environment variables for the launched instance.
    pub env: BTreeMap<String, String>,
    /// Whether the entry participates in the default runtime set.
    pub enabled: bool,
    /// Ids of other rows that must be available first (drives the ordering).
    pub requires: Vec<String>,
    /// Free-form module config (TOML table turned into a JSON object; empty
    /// object when absent).
    pub config: serde_json::Value,
    /// 0-based index of the layer (applied file) that last wrote this row.
    pub layer: usize,
    /// File name of that layer.
    pub layer_source: String,
    /// 子进程资源限额（`[module.limits]`；`None` = 不限制）。
    ///
    /// 只对 `kind = "stdio"` 有意义（进程内实现没有独立进程可限）。
    pub limits: Option<ModuleLimits>,
}

/// 子进程资源限额（`[module.limits]`）。
///
/// 每个字段 `Option`：**缺省 = 不限制**（继承宿主）。限额是部署策略，
/// 没有普适默认值——给错的默认比不给更危险（例如 Node/NapCat 类模块
/// 需要大地址空间，随手设 256 MiB 会直接起不来）。
///
/// 只支持"四件套 + 进程数"：CPU 时间、地址空间、单文件大小、打开文件数、
/// 子进程数。每一项都是**硬限额**（软=硬），超限立即生效（SIGXCPU /
/// 分配失败 / EFBIG / EMFILE / fork 失败），不做"先警告再收紧"的两段式
/// ——模块没有义务处理信号，两段式只会让人误以为限额没生效。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
pub struct ModuleLimits {
    /// CPU 时间上限（秒）。达到后内核发 SIGXCPU（默认终止）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_seconds: Option<u64>,
    /// 地址空间上限（MiB）。超限 malloc/mmap 失败（模块通常随即崩溃）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u64>,
    /// 单文件写入上限（MiB）。超限写返回 `EFBIG`（防模块写爆磁盘）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_size_mb: Option<u64>,
    /// 打开文件描述符上限（含 socket）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_files: Option<u64>,
    /// 子进程数上限（防 fork 炸弹；模块再 fork 的子进程也受限）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processes: Option<u64>,
}

impl ModuleLimits {
    /// 是否一项都没设（等价于 `None`）。
    pub fn is_empty(&self) -> bool {
        self.cpu_seconds.is_none()
            && self.memory_mb.is_none()
            && self.file_size_mb.is_none()
            && self.open_files.is_none()
            && self.processes.is_none()
    }
}

/// A composed tree plus non-fatal diagnostics.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedTree {
    /// Rows in dependency order (stable topological sort by `requires`).
    pub entries: Vec<ModuleEntry>,
    /// Non-fatal diagnostics, e.g. a duplicate id inside one layer.
    pub warnings: Vec<String>,
}
