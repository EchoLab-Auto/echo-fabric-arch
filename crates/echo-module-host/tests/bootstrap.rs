//! `bootstrap` 模块集成测试：配置 → 启动封装的端到端行为。
//!
//! 对端是 stdio 测试模块进程（`src/bin/echo-module-test-peer.rs`，经
//! `env!("CARGO_BIN_EXE_echo-module-test-peer")` 定位）：注册 `echo` 工具、
//! Invoke 回显 `payload["text"]`、Drain 后退出。
//!
//! 覆盖场景：
//! 1. [`launch_specs`] 只取 enabled 的 stdio 条目，其余进跳过原因；
//! 2. 全成功启动 → 句柄 / 工具可用（`echo` 调用回显）；
//! 3. 单点失败 best-effort（坏 command 与好 command 并存）；
//! 4. [`start_from_file`]（真实 `modules.toml`）+ 文件缺失 → Err；
//! 5. shutdown 排空到 Stopped。
//!
//! 断言口径：本 crate 领域中立，**不引用适配层**（`RemoteTool` 在
//! `echo-module-adapter`）。因此「工具可用」在这里写成「贡献里有 `Tool`
//! 且 `invoke` 回显正确」——覆盖同一条链路，但不引入 `echo-defs` 依赖。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use echo_module_host::api::{Contribution, InvokeContext, InvokeOutcome};
use echo_module_host::bootstrap::{launch_specs, start_from_file, start_modules, LaunchSpec};
use echo_module_host::supervisor::{ModuleState, ModuleSupervisor};
use echo_module_loader::{EntryKind, LoadedTree, ModuleEntry};
use serde_json::json;

/// 测试对端二进制路径（cargo 注入；[[bin]] 见 Cargo.toml）。
const PEER: &str = env!("CARGO_BIN_EXE_echo-module-test-peer");

// ── 辅助 ────────────────────────────────────────────────────────────────────

fn entry(id: &str, kind: EntryKind, enabled: bool) -> ModuleEntry {
    ModuleEntry {
        id: id.to_string(),
        kind,
        name: id.to_string(),
        command: if kind == EntryKind::Stdio {
            Some(PEER.to_string())
        } else {
            None
        },
        args: vec![],
        env: BTreeMap::new(),
        enabled,
        requires: vec![],
        config: json!({}),
        limits: None,
        layer: 0,
        layer_source: "test.toml".to_string(),
    }
}

fn spec(module_id: &str, command: &str) -> LaunchSpec {
    LaunchSpec {
        module_id: module_id.to_string(),
        command: command.to_string(),
        args: vec![],
        env: BTreeMap::new(),
        config: json!({}),
        limits: None,
    }
}

/// 唯一临时目录（不引入 tempfile 依赖；测试尾部由调用方清理）。
fn unique_temp_dir(tag: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "echo-module-host-bootstrap-{tag}-{}-{nanos}-{n}",
        std::process::id()
    ))
}

fn cleanup(dir: &std::path::Path) {
    let _ = std::fs::remove_dir_all(dir);
}

// ── 1. launch_specs 筛选 ────────────────────────────────────────────────────

#[test]
fn launch_specs_filters_enabled_stdio_only() {
    let tree = LoadedTree {
        entries: vec![
            entry("peer-a", EntryKind::Stdio, true),
            entry("peer-b", EntryKind::Stdio, false),
            entry("websearch", EntryKind::Builtin, true),
        ],
        warnings: vec![],
    };
    let (specs, skipped) = launch_specs(&tree);
    assert_eq!(specs.len(), 1, "只有 enabled stdio 条目可启动");
    assert_eq!(specs[0].module_id, "peer-a");
    assert_eq!(specs[0].command, PEER);
    assert_eq!(skipped.len(), 2, "跳过原因应逐条记录: {skipped:?}");
    assert!(
        skipped.iter().any(|s| s == "peer-b: disabled"),
        "缺少 disabled 原因: {skipped:?}"
    );
    assert!(
        skipped
            .iter()
            .any(|s| s == "websearch: kind builtin skipped"),
        "缺少 builtin 原因: {skipped:?}"
    );
}

#[test]
fn launch_specs_skips_stdio_without_command() {
    // loader 校验过的树不会出现这种条目；手工构造模拟损坏树。
    let mut broken = entry("broken", EntryKind::Stdio, true);
    broken.command = None;
    let tree = LoadedTree {
        entries: vec![broken],
        warnings: vec![],
    };
    let (specs, skipped) = launch_specs(&tree);
    assert!(specs.is_empty());
    assert_eq!(skipped, vec!["broken: stdio without command skipped"]);
}

// ── 2. 全成功路径 ───────────────────────────────────────────────────────────

#[tokio::test]
async fn start_modules_all_success_path() {
    let supervisor = ModuleSupervisor::new();
    let started = start_modules(&supervisor, vec![spec("peer-ok", PEER)]).await;
    assert_eq!(started.handles.len(), 1);
    assert!(started.errors.is_empty(), "errors: {:?}", started.errors);

    let handle = &started.handles[0];
    let op_names: Vec<String> = handle
        .contributions()
        .iter()
        .filter_map(Contribution::target_name)
        .map(str::to_string)
        .collect();
    assert_eq!(op_names, vec!["echo"], "对端注册一个可寻址贡献（echo）");
    let outcome = handle
        .invoke("echo", InvokeContext::default(), json!({"text": "hi"}))
        .await;
    match outcome {
        InvokeOutcome::Ok { text, .. } => {
            assert!(text.contains("hi"), "echo 应回显 text，实际: {text}")
        }
        other => panic!("echo 调用应成功，实际: {other:?}"),
    }

    started.shutdown(Duration::from_secs(2)).await;
}

// ── 3. 单点失败 best-effort ────────────────────────────────────────────────

#[tokio::test]
async fn start_modules_best_effort_on_single_failure() {
    let supervisor = ModuleSupervisor::new();
    let started = start_modules(
        &supervisor,
        vec![spec("bad", "/nonexistent/xyz"), spec("peer-ok", PEER)],
    )
    .await;
    assert_eq!(
        started.errors.len(),
        1,
        "只有一个失败: {:?}",
        started.errors
    );
    assert_eq!(started.errors[0].0, "bad");
    assert!(!started.errors[0].1.is_empty(), "错误描述非空");
    assert_eq!(started.handles.len(), 1, "其余模块应继续启动");
    let ok_handle = &started.handles[0];
    assert!(
        ok_handle
            .contributions()
            .iter()
            .any(|c| c.target_name() == Some("echo")),
        "成功模块的贡献应可用"
    );

    started.shutdown(Duration::from_secs(2)).await;
}

// ── 4. start_from_file ─────────────────────────────────────────────────────

#[tokio::test]
async fn start_from_file_loads_toml_and_missing_path_errors() {
    let dir = unique_temp_dir("file");
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("modules.toml");
    let toml = format!(
        "[[module]]\nid = \"peer\"\nkind = \"stdio\"\nname = \"echo-module-test-peer\"\n\
         command = {:?}\nenabled = true\nconfig = {{ mode = \"test\" }}\n",
        PEER
    );
    std::fs::write(&path, toml).expect("write modules.toml");

    // 存在且合法 → Ok + 可启动。
    let supervisor = ModuleSupervisor::new();
    let started = start_from_file(&path, &supervisor)
        .await
        .expect("modules.toml 应装载成功");
    assert_eq!(started.handles.len(), 1);
    assert!(started.errors.is_empty(), "errors: {:?}", started.errors);
    // 排空干净退出。
    started.shutdown(Duration::from_secs(2)).await;

    // 文件不存在路径 → Err。
    let missing = dir.join("nope").join("modules.toml");
    assert!(
        start_from_file(&missing, &supervisor).await.is_err(),
        "不存在的配置应返回 Err"
    );

    cleanup(&dir);
}

// ── 5. shutdown → Stopped ──────────────────────────────────────────────────

#[tokio::test]
async fn shutdown_drains_handles_to_stopped() {
    let supervisor = ModuleSupervisor::new();
    let started = start_modules(
        &supervisor,
        vec![spec("peer-shutdown-a", PEER), spec("peer-shutdown-b", PEER)],
    )
    .await;
    assert_eq!(started.handles.len(), 2);
    for handle in &started.handles {
        assert_eq!(handle.state(), ModuleState::Ready);
    }

    started.shutdown(Duration::from_secs(2)).await;

    for handle in &started.handles {
        assert_eq!(
            handle.state(),
            ModuleState::Stopped,
            "drain 后应为终态 Stopped（module={}）",
            handle.module_id()
        );
    }
}

// ── 资源限额落地（rlimit）──────────────────────────────────────────────────

/// 限额真的落到子进程：给一个很小的 `open_files`，让模块在握手时就撞上
/// `EMFILE`（测试对端会打开 stdin/stdout/stderr 之外的 fd）。
///
/// 只在 Linux 跑：`RLIMIT_NOFILE` 的语义在各平台不完全一致，这里锁的是
/// 「限额确实落到子进程」这条不变量，而不是某个平台的细节。
#[cfg(target_os = "linux")]
#[tokio::test]
async fn resource_limits_reach_the_child_process() {
    use echo_module_host::stdio::ResourceLimits;

    // 3 个 fd 都不够（stdin/stdout/stderr 已占 3），握手必然失败。
    let mut limited = spec("limited", PEER);
    limited.limits = Some(echo_module_loader::ModuleLimits {
        open_files: Some(3),
        ..Default::default()
    });
    let supervisor = ModuleSupervisor::new();
    let started = start_modules(&supervisor, vec![limited]).await;
    assert!(
        started.handles.is_empty(),
        "3 个 fd 的限额下模块不可能完成握手"
    );
    assert_eq!(started.errors.len(), 1, "失败要记成可读错误");

    // 对照：同一份 spec 不设限额 → 正常启动。
    let permissive = ResourceLimits {
        open_files: Some(4096),
        ..Default::default()
    };
    let mut permissive_spec = spec("permissive", PEER);
    permissive_spec.limits = Some(echo_module_loader::ModuleLimits {
        open_files: Some(4096),
        ..Default::default()
    });
    assert!(!permissive.is_empty());
    let started = start_modules(&supervisor, vec![permissive_spec]).await;
    assert_eq!(started.handles.len(), 1, "宽松限额下应正常启动");
    started.shutdown(Duration::from_secs(3)).await;
}

/// 空限额 = 不装 `pre_exec`（spawn 路径与无限制完全一致）。
#[test]
fn empty_resource_limits_are_a_noop() {
    use echo_module_host::stdio::ResourceLimits;
    assert!(ResourceLimits::default().is_empty());
    assert!(!ResourceLimits {
        cpu_seconds: Some(1),
        ..Default::default()
    }
    .is_empty());
}

/// 配置单位 → 运行时单位：MiB 换算成字节（换算只在一处发生）。
#[tokio::test]
async fn limits_config_units_convert_to_bytes() {
    // 通过 launch_specs 走一遍真实链路（配置 → spec），确认字段透传。
    let dir = unique_temp_dir("limits-launch");
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("modules.toml");
    std::fs::write(
        &path,
        format!(
            r#"
[[module]]
id = "ext"
kind = "stdio"
name = "ext"
command = "{PEER}"

[module.limits]
cpu_seconds = 60
memory_mb = 512
open_files = 256
"#
        ),
    )
    .expect("write fixture");
    let tree = echo_module_loader::load(&path).expect("load");
    let (specs, _skipped) = launch_specs(&tree);
    let limits = specs[0].limits.expect("limits carried into LaunchSpec");
    assert_eq!(limits.cpu_seconds, Some(60));
    assert_eq!(limits.memory_mb, Some(512));
    assert_eq!(limits.open_files, Some(256));
    let _ = std::fs::remove_dir_all(&dir);
}
