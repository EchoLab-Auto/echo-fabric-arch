//! 启动封装：把 `modules.toml` 合成出的模块树翻译成宿主启动动作
//! （解耦计划 P3/P4）。
//!
//! 职责划分：
//! - [`launch_specs`]：从 [`echo_module_loader::LoadedTree`] 中筛选
//!   `enabled == true` 且 `kind == Stdio` 的条目，转成 [`LaunchSpec`]；
//!   builtin / dylib / wasm（非本运输层）与 disabled 条目连同原因进入第二个
//!   返回值（人可读，供日志 / 诊断）。
//! - [`start_modules`]：**best-effort** 逐个启动——单个模块失败（spawn 失败 /
//!   握手超时等）不阻断其余模块，失败记入 [`StartedModules::errors`]。
//! - [`start_from_file`]：`load(path)` + [`start_modules`] 一步到位。
//! - [`StartedModules::shutdown`]：统一排空。
//!
//! 工具 / 实例的**适配**（`RemoteTool` / `RemoteInstance`）在
//! `echo-module-adapter`：本 crate 领域中立，不引用 `echo-defs`。
//!
//! 语义要点：
//! - `config` 原样透传（[`ModuleSpec::config`] → `Hello.config`）；
//! - `env` 逐条注入（stdio 运输层默认不继承宿主环境，仅透传 `PATH`）；
//! - shutdown 对每个句柄 `drain(deadline)`，单点错误只记 debug 日志。

use std::collections::BTreeMap;
use std::path::Path;

use std::time::Duration;

use serde_json::Value;

use echo_module_loader::{EntryKind, LoadedTree};

use crate::stdio::{ResourceLimits, StdioTransport};
use crate::supervisor::{ModuleHandle, ModuleSupervisor};
use crate::transport::ModuleSpec;

/// 一个可启动的 stdio 模块实例（由 `modules.toml` 的一个条目翻译而来）。
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    /// 宿主侧模块实例 id（Hello/Welcome 握手比对；同一棵树内唯一）。
    pub module_id: String,
    /// 可执行文件路径（loader 已校验非空）。
    pub command: String,
    /// 追加命令行参数。
    pub args: Vec<String>,
    /// 注入子进程的环境变量（宿主环境默认不继承，仅透传 `PATH`）。
    pub env: BTreeMap<String, String>,
    /// 模块配置（原样进 [`ModuleSpec::config`]）。
    pub config: Value,
    /// 子进程资源限额（`modules.toml` 的 `[module.limits]`；None = 不限制）。
    pub limits: Option<echo_module_loader::ModuleLimits>,
}

/// 从合成树中筛选可启动的 stdio 实例。
///
/// 返回 `(specs, skipped)`：`skipped` 是跳过原因（人可读），形如
/// `"websearch: kind builtin skipped"` / `"x: disabled"` /
/// `"y: stdio without command skipped"`。
///
/// stdio 条目缺 `command` 不应出现（loader 已校验），如遇手工构造的树
/// 则跳过并记录。
pub fn launch_specs(tree: &LoadedTree) -> (Vec<LaunchSpec>, Vec<String>) {
    let mut specs = Vec::new();
    let mut skipped = Vec::new();
    for entry in &tree.entries {
        if !entry.enabled {
            skipped.push(format!("{}: disabled", entry.id));
            continue;
        }
        if entry.kind != EntryKind::Stdio {
            skipped.push(format!(
                "{}: kind {} skipped",
                entry.id,
                entry.kind.as_str()
            ));
            continue;
        }
        let Some(command) = entry.command.as_deref().filter(|c| !c.trim().is_empty()) else {
            // 防御性分支：loader 校验过的树不会走到这里。
            skipped.push(format!("{}: stdio without command skipped", entry.id));
            continue;
        };
        specs.push(LaunchSpec {
            module_id: entry.id.clone(),
            command: command.to_string(),
            args: entry.args.clone(),
            env: entry.env.clone(),
            config: entry.config.clone(),
            limits: entry.limits,
        });
    }
    (specs, skipped)
}

/// 启动结果：成功握手的句柄 + 单点失败记录。
pub struct StartedModules {
    /// 成功完成握手的模块句柄（顺序与 `specs` 一致，失败的被跳过）。
    pub handles: Vec<ModuleHandle>,
    /// 启动失败的模块：`(module_id, 错误描述)`。
    pub errors: Vec<(String, String)>,
}

impl StartedModules {
    /// 逐个排空（`drain(deadline)`）；单点错误只记 debug 日志，不影响其余。
    pub async fn shutdown(&self, deadline: Duration) {
        for handle in &self.handles {
            if let Err(error) = handle.drain(deadline).await {
                tracing::debug!(
                    module = %handle.module_id(),
                    %error,
                    "module drain failed"
                );
            }
        }
    }
}

/// 配置侧限额（MiB / 秒的**人读单位**）→ 运行时限额（字节）。
///
/// 单位换算是刻意分开的：配置文件里人写 `memory_mb = 512`，运行时
/// `setrlimit` 要字节数——换算只在这一处发生，避免两套单位在各层漂移。
fn resource_limits(limits: echo_module_loader::ModuleLimits) -> ResourceLimits {
    const MIB: u64 = 1024 * 1024;
    ResourceLimits {
        cpu_seconds: limits.cpu_seconds,
        memory_bytes: limits.memory_mb.map(|mb| mb.saturating_mul(MIB)),
        file_size_bytes: limits.file_size_mb.map(|mb| mb.saturating_mul(MIB)),
        open_files: limits.open_files,
        processes: limits.processes,
    }
}

/// 逐个启动（best-effort）：`start` 失败记录到 [`StartedModules::errors`]
/// 并继续启动其余模块。
pub async fn start_modules(
    supervisor: &ModuleSupervisor,
    specs: Vec<LaunchSpec>,
) -> StartedModules {
    let mut handles = Vec::new();
    let mut errors = Vec::new();
    for spec in specs {
        let mut transport = StdioTransport::new(&spec.command).with_args(&spec.args);
        for (key, value) in &spec.env {
            transport = transport.with_env(key, value);
        }
        if let Some(limits) = spec.limits {
            transport = transport.with_limits(resource_limits(limits));
        }
        let module_spec = ModuleSpec {
            module_id: spec.module_id.clone(),
            config: spec.config.clone(),
        };
        match supervisor.start(module_spec, Box::new(transport)).await {
            Ok(handle) => {
                tracing::info!(module = %handle.module_id(), "module started");
                handles.push(handle);
            }
            Err(error) => {
                tracing::warn!(module = %spec.module_id, %error, "module start failed");
                errors.push((spec.module_id, error.to_string()));
            }
        }
    }
    StartedModules { handles, errors }
}

/// `load(path)` 失败原样返回 [`echo_module_loader::LoadError`]；成功则
/// best-effort [`start_modules`]。
pub async fn start_from_file(
    path: &Path,
    supervisor: &ModuleSupervisor,
) -> Result<StartedModules, echo_module_loader::LoadError> {
    let tree = echo_module_loader::load(path)?;
    let (specs, _skipped) = launch_specs(&tree);
    Ok(start_modules(supervisor, specs).await)
}
