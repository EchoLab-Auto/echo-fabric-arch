//! dylib 运输层验收：对**真实的 `.so`**（`crates/echo-module-dylib-peer`）跑协议场景。
//!
//! 与被测能力对应的场景：
//!
//! 1. **产物独立 + 同进程**：宿主 `dlopen` 一个外部编译的 `.so`，模块跑在本进程
//!    的线程里，协议帧经 socketpair 交换——这是 builtin（不独立）与 stdio
//!    （不同进程）都给不出的组合。
//! 2. 握手 + 贡献注册 + 调用往返（与 stdio / inproc 同一套语义）。
//! 3. 调用超时 → `Cancel` 送达模块（帧协议不因载体变化而改变语义）。
//! 4. 模块 panic **不带走宿主**（`.so` 内 `catch_unwind` 的验收点）。
//! 5. 关停：写方向 FIN → 模块自行退出 → 线程 join 成功（不留泄漏）。
//! 6. 载入失败的三条路径：文件不存在 / 不是动态库 / 缺入口符号。
//!
//! `.so` 的定位：`cargo build --workspace` 会产出
//! `target/<profile>/libecho_module_dylib_peer.so`；若不存在（例如只跑
//! `cargo test -p echo-module-host`），本套件自己构建一次到独立 target dir
//! ——独立目录是为了不与正在跑的 cargo 抢同一把目标锁（那会死锁）。

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use echo_module_host::api::*;
use echo_module_host::dylib::DylibTransport;
use echo_module_host::supervisor::{ModuleHandle, ModuleSupervisor};
use echo_module_host::transport::{ModuleSpec, TransportError};
use serde_json::{json, Value};

/// 夹具包的 manifest（相对本 crate）。
const FIXTURE_MANIFEST: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../echo-module-dylib-peer/Cargo.toml"
);

/// 构建并定位夹具 `.so`。
///
/// **每次都调 `cargo build`**（已是最新时 cargo 自己会跳过，代价约 0.3s），
/// 而不是优先找预构建产物：预构建的那份可能是**上一次源码**编的，用它跑测试
/// 等于在验旧代码。2026-11 就踩过——修好 `serve_fd_blocking` 之后测试仍然
/// SIGABRT，因为用的是改之前编出来的 `.so`。
///
/// 目标目录独立一份：外层 `cargo test` 可能仍持有本仓库 target 的锁，就地
/// 再跑一次 cargo 会死锁。
fn fixture_so() -> PathBuf {
    let target_dir = std::env::temp_dir().join("echo-module-dylib-fixture-target");
    let status = Command::new(env!("CARGO"))
        .args(["build", "--manifest-path", FIXTURE_MANIFEST, "--target-dir"])
        .arg(&target_dir)
        .arg("--quiet")
        .status()
        .unwrap_or_else(|e| panic!("spawn cargo to build the fixture: {e}"));
    assert!(status.success(), "building the dylib fixture failed");

    let built = target_dir.join("debug").join(format!(
        "{}echo_module_dylib_peer{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ));
    assert!(
        built.is_file(),
        "fixture build reported success but {} is missing",
        built.display()
    );
    built
}

fn transport() -> DylibTransport {
    DylibTransport::new(fixture_so())
}

fn spec(config: Value) -> ModuleSpec {
    ModuleSpec {
        module_id: "dylib-peer".to_string(),
        config,
    }
}

async fn start(config: Value) -> ModuleHandle {
    let supervisor = ModuleSupervisor::new();
    supervisor
        .start(spec(config), Box::new(transport()))
        .await
        .expect("dylib module should register")
}

fn ctx(deadline_ms: u64) -> InvokeContext {
    InvokeContext {
        deadline_ms: Some(deadline_ms),
        ..InvokeContext::default()
    }
}

/// 场景 1+2：外部产物被插进本进程，握手与调用往返成立。
#[tokio::test]
async fn dlopen_handshake_and_invoke_roundtrip() {
    let handle = start(json!({})).await;
    assert_eq!(handle.module_id(), "dylib-peer");

    let outcome = handle
        .invoke("echo", ctx(5_000), json!({"hello": "world"}))
        .await;
    assert_eq!(
        outcome,
        InvokeOutcome::ok("echo:{\"hello\":\"world\"}"),
        "回显应带上 payload"
    );
    handle
        .drain(Duration::from_secs(2))
        .await
        .expect("模块应在 deadline 内正常排空");
}

/// 场景 2b：模块跑在**宿主进程内**——不是子进程。
///
/// 判据是模块**自报**的 pid：与宿主进程的 pid 相同，就说明它在同一个地址空间
/// 里跑。比扫 `/proc/self/task` 的线程名可靠——那套判据在测试并行时会看到
/// 别的用例的模块线程，得到"关不掉"的假阴性（2026-11 实测）。
#[tokio::test]
async fn module_runs_inside_the_host_process() {
    let handle = start(json!({})).await;
    let outcome = handle
        .invoke("echo", ctx(5_000), json!({"whoami": true}))
        .await;
    let InvokeOutcome::Ok { text, .. } = outcome else {
        panic!("whoami 应成功，实际 {outcome:?}");
    };
    let reported = text
        .strip_prefix("pid=")
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|pid| pid.parse::<u32>().ok())
        .unwrap_or_else(|| panic!("无法解析模块自报的 pid：{text:?}"));
    assert_eq!(
        reported,
        std::process::id(),
        "模块应跑在宿主进程内（pid 相同 = 同一地址空间）；不同则说明它其实是子进程"
    );
    handle
        .drain(Duration::from_secs(2))
        .await
        .expect("模块应在 deadline 内正常排空");
}

/// 场景 3：超时 → `Cancel` 送达（帧协议语义不因载体变化）。
#[tokio::test]
async fn invoke_timeout_reports_timeout() {
    let handle = start(json!({"hang": true})).await;
    let outcome = handle.invoke("echo", ctx(150), json!({})).await;
    assert!(
        matches!(&outcome, InvokeOutcome::Error { code, .. } if code == "timeout"),
        "期望 timeout，实际 {outcome:?}"
    );
    // 模块的 invoke 永远 pending → 它不会自行退出，drain 必然以 Timeout 收场。
    // 这正是 dylib 形态的已知代价（没有可杀的对象），断言它而不是假装成功。
    assert!(
        matches!(
            handle.drain(Duration::from_millis(300)).await,
            Err(TransportError::Timeout)
        ),
        "卡住的模块应让 drain 以 Timeout 收场"
    );
}

/// 场景 4：模块 panic **不带走宿主**（`.so` 内 `catch_unwind` 的验收点）。
#[tokio::test]
async fn module_panic_does_not_kill_the_host() {
    let handle = start(json!({"panic_on_invoke": true})).await;
    // panic 之后模块线程结束 → 连接关闭。宿主要么拿到错误结果，要么看到
    // Closed 并自行重启；无论哪种，**本进程必须还活着**（能执行到断言）。
    let outcome = handle.invoke("echo", ctx(2_000), json!({})).await;
    // panic 之后模块线程结束 → 连接关闭 → 调用以错误终态收尾（具体 code 由
    // 监督器决定：`module_crashed` 或 `closed`）。关键是**本进程还活着**。
    assert!(
        matches!(outcome, InvokeOutcome::Error { .. }),
        "panic 后调用应有错误终态，实际 {outcome:?}"
    );
    assert!(std::process::id() > 0, "跑到这里就说明 panic 没有穿出 .so");
    handle.stop(Duration::from_secs(2)).await.ok();
}

/// 场景 5：关停——写方向 FIN → 模块自行退出 → 线程真的消失。
///
/// 判据同样用模块自报的 tid（而不是扫线程名）：并行跑的其他用例也会创建
/// 同名线程，扫名字会永远看到"还在"。
#[tokio::test]
async fn shutdown_reaps_the_module_thread() {
    let handle = start(json!({})).await;
    let outcome = handle
        .invoke("echo", ctx(5_000), json!({"whoami": true}))
        .await;
    let InvokeOutcome::Ok { text, .. } = outcome else {
        panic!("whoami 应成功，实际 {outcome:?}");
    };
    let tid = text
        .split_whitespace()
        .find_map(|part| part.strip_prefix("tid="))
        .and_then(|tid| tid.parse::<u32>().ok())
        .unwrap_or_else(|| panic!("无法解析模块自报的 tid：{text:?}"));
    assert!(
        thread_alive(tid),
        "自报 tid {tid} 应能在 /proc/self/task 里找到（否则判据本身失效）"
    );

    // `stop`（不是 drain）：直接关连接，不等排空——这正是"模块读到 EOF 自行
    // 退出"的路径，也是线程能被回收的前提。
    handle
        .stop(Duration::from_secs(2))
        .await
        .expect("模块应在 deadline 内自行退出（EOF 后结束协议循环）");

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while thread_alive(tid) {
        assert!(
            std::time::Instant::now() < deadline,
            "关停后模块线程 {tid} 应已退出"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 某个 tid 是否仍是本进程的存活线程。
fn thread_alive(tid: u32) -> bool {
    std::path::Path::new(&format!("/proc/self/task/{tid}")).exists()
}

/// 场景 6a：文件不存在 → 启动失败并点名路径。
#[tokio::test]
async fn missing_artifact_is_reported() {
    let transport = DylibTransport::new("/nonexistent/module.so");
    let supervisor = ModuleSupervisor::new();
    let error = match supervisor.start(spec(json!({})), Box::new(transport)).await {
        Ok(_) => panic!("不存在的 .so 不应启动成功"),
        Err(error) => error,
    };
    match error {
        TransportError::Io(message) => {
            assert!(
                message.contains("/nonexistent/module.so"),
                "错误消息应点名路径，实际 {message:?}"
            );
        }
        other => panic!("期望 Io 错误，实际 {other:?}"),
    }
}

/// 场景 6b：文件存在但不是动态库 → dlopen 报错（不是"启动后握手超时"）。
#[tokio::test]
async fn non_library_file_is_reported_at_load_time() {
    let path = std::env::temp_dir().join(format!("echo-not-a-lib-{}.so", std::process::id()));
    std::fs::write(&path, b"this is definitely not an ELF shared object").expect("write");
    let supervisor = ModuleSupervisor::new();
    let error = match supervisor
        .start(spec(json!({})), Box::new(DylibTransport::new(&path)))
        .await
    {
        Ok(_) => panic!("非动态库不应启动成功"),
        Err(error) => error,
    };
    let _ = std::fs::remove_file(&path);
    match error {
        TransportError::Io(message) => assert!(
            message.contains("dlopen"),
            "错误消息应指向 dlopen，实际 {message:?}"
        ),
        other => panic!("期望 Io 错误，实际 {other:?}"),
    }
}

/// 场景 6c：是动态库但缺入口符号 → 错误里点名缺的符号。
#[tokio::test]
async fn library_without_the_entry_symbol_is_reported() {
    // 用任意系统库冒充"没有 echo_module_run 的模块"。
    let candidate = [
        "/lib/x86_64-linux-gnu/libm.so.6",
        "/usr/lib/libm.so.6",
        "/lib/libm.so.6",
    ]
    .into_iter()
    .find(|path| std::path::Path::new(path).is_file());
    let Some(library) = candidate else {
        eprintln!("跳过：本机找不到 libm.so.6");
        return;
    };
    let supervisor = ModuleSupervisor::new();
    let error = match supervisor
        .start(spec(json!({})), Box::new(DylibTransport::new(library)))
        .await
    {
        Ok(_) => panic!("没有入口符号的库不应启动成功"),
        Err(error) => error,
    };
    match error {
        TransportError::Io(message) => assert!(
            message.contains("echo_module_run"),
            "错误消息应点名缺失的入口符号，实际 {message:?}"
        ),
        other => panic!("期望 Io 错误，实际 {other:?}"),
    }
}

// ── bootstrap：按 kind 分派 ────────────────────────────────────────────────

/// `launch_specs` 必须把 `kind = "dylib"` 也当成可启动条目（此前只有 stdio）。
#[test]
fn launch_specs_accept_dylib_rows() {
    use echo_module_host::bootstrap::{launch_specs, LaunchKind};

    let dir = std::env::temp_dir().join(format!("echo-dylib-profile-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("modules.toml");
    std::fs::write(
        &path,
        r#"
[[module]]
id = "inproc"
kind = "dylib"
name = "inproc"
command = "/opt/modules/inproc.so"
enabled = true

[[module]]
id = "proc"
kind = "stdio"
name = "proc"
command = "/opt/modules/proc"
enabled = true

[[module]]
id = "builtin-row"
kind = "builtin"
name = "builtin-row"
enabled = true
"#,
    )
    .expect("write profile");

    let tree = echo_module_loader::load(&path).expect("load tree");
    let (specs, skipped) = launch_specs(&tree);
    let _ = std::fs::remove_file(&path);

    assert_eq!(specs.len(), 2, "dylib 与 stdio 各一条，实际 {specs:?}");
    assert_eq!(specs[0].module_id, "inproc");
    assert_eq!(specs[0].kind, LaunchKind::Dylib);
    assert_eq!(specs[1].module_id, "proc");
    assert_eq!(specs[1].kind, LaunchKind::Stdio);
    assert!(
        skipped.iter().any(|reason| reason.contains("builtin-row")),
        "builtin 行应进 skipped，实际 {skipped:?}"
    );
}
