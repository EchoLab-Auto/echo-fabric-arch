//! 热替换 conformance（Phase 5）：[`PluginHandle::upgrade`] 的
//! 「新实例 Ready → 切流量 → 旧实例 Drain → 退出（失败保老，可回滚）」语义。
//!
//! 覆盖：
//! a. stdio 升级成功：新 config 生效（`echo` 的 `show_config` 回显
//!    `Hello.config`）、`hot_replaces()==1`、旧实例已排空退出
//!    （按唯一 tag 经 /proc 环境扫描计数）、后续调用与最终 drain 正常；
//! b. 非 Ready（Stopped）拒绝：`Err(Protocol)` 且消息含 "not ready"，不排队；
//! c. 崩溃重启窗口（Pending）拒绝且不排队；重启照常完成、服务不受影响；
//! d. 握手失败保老（inproc）：新实例启动即退出 → `upgrade` 返回 Err，
//!    旧实例继续服务、贡献与计数不变（可重试 / 回滚）；
//! e. 连续两次升级：计数累积、实例逐代更替。
//! stdio 用例经 `env!("CARGO_BIN_EXE_echo-plugin-test-peer")` 使用测试对端
//! （`src/bin/echo-plugin-test-peer.rs`）。

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use echo_plugin_host::api::*;
use echo_plugin_host::inproc::{InprocIo, InprocPlugin, InprocTransport};
use echo_plugin_host::stdio::StdioTransport;
use echo_plugin_host::supervisor::{PluginHandle, PluginState, PluginSupervisor, RestartPolicy};
use echo_plugin_host::transport::{PluginSpec, Transport, TransportError};
use serde_json::{json, Value};

/// 测试对端二进制路径（cargo 注入；[[bin]] 见 Cargo.toml）。
const PEER: &str = env!("CARGO_BIN_EXE_echo-plugin-test-peer");

// ── 辅助 ────────────────────────────────────────────────────────────────────

fn spec(config: Value) -> PluginSpec {
    PluginSpec {
        plugin_id: "upgrade-test".to_string(),
        config,
    }
}

/// 测试用调用上下文（5s 上限，防止用例卡死）。
fn ctx(deadline_ms: u64) -> InvokeContext {
    InvokeContext {
        deadline_ms: Some(deadline_ms),
        ..InvokeContext::default()
    }
}

/// 启动插件并断言握手成功。
async fn start_plugin(transport: Box<dyn Transport>, config: Value) -> PluginHandle {
    PluginSupervisor::new()
        .start(spec(config), transport)
        .await
        .expect("handshake should succeed")
}

/// 以 10ms 步进轮询条件，最多 `timeout`。
async fn wait_for(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if cond() {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// 经 `echo` 的 `show_config` 通道取当前实例握手时的 config。
async fn invoke_config(handle: &PluginHandle) -> Value {
    let out = handle
        .invoke("echo", ctx(5_000), json!({"show_config": true}))
        .await;
    match out {
        InvokeOutcome::Ok { text, .. } => serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("config echo should be JSON: {text:?} ({e})")),
        other => panic!("expected Ok, got {other:?}"),
    }
}

/// 统计环境变量含 `ECHO_PLUGIN_TEST_PEER_TAG=<tag>` 的进程数。
///
/// Linux 上读 `/proc/<pid>/environ`；`/proc` 不可用的环境返回 `None`
/// （对应的进程数断言降级为跳过）。tag 仅本用例使用，避免与其他
/// 并发用例互扰。
fn peer_processes(tag: &str) -> Option<usize> {
    let dir = std::fs::read_dir("/proc").ok()?;
    let needle = format!("ECHO_PLUGIN_TEST_PEER_TAG={tag}");
    let mut count = 0;
    for entry in dir.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(environ) = std::fs::read(format!("/proc/{pid}/environ")) else {
            continue;
        };
        if environ.split(|b| *b == 0).any(|kv| kv == needle.as_bytes()) {
            count += 1;
        }
    }
    Some(count)
}

// ── a. stdio 升级成功 ───────────────────────────────────────────────────────

/// 升级成功：新 config 生效 + `hot_replaces` +1 + 旧实例排空退出 + 后续调用正常。
#[tokio::test]
async fn a_stdio_upgrade_switches_to_new_instance() {
    let tag = "upgrade-a";
    let handle = start_plugin(
        Box::new(StdioTransport::new(PEER).with_env("ECHO_PLUGIN_TEST_PEER_TAG", tag)),
        json!({"k": 1}),
    )
    .await;

    assert_eq!(handle.hot_replaces(), 0);
    assert_eq!(invoke_config(&handle).await, json!({"k": 1}));
    let counting = peer_processes(tag).is_some();
    if counting {
        assert_eq!(peer_processes(tag), Some(1), "启动后应恰好 1 个实例在跑");
    }

    let started = Instant::now();
    handle
        .upgrade(json!({"gen": 2}), Duration::from_secs(2))
        .await
        .expect("upgrade should succeed");
    let elapsed = started.elapsed();

    assert_eq!(handle.hot_replaces(), 1, "完成一次热替换");
    assert_eq!(handle.state(), PluginState::Ready);
    // 流量已切到新实例：echo 回显新实例握手时的 config。
    assert_eq!(invoke_config(&handle).await, json!({"gen": 2}));
    let out = handle
        .invoke("echo", ctx(5_000), json!({"text": "after-upgrade"}))
        .await;
    assert_eq!(out, InvokeOutcome::ok("after-upgrade"));

    // upgrade 返回 Ok 即表示旧实例已排空退出：只剩新实例。
    if counting {
        assert!(
            wait_for(Duration::from_secs(2), || peer_processes(tag) == Some(1)).await,
            "旧实例应已退出；当前计数 = {:?}",
            peer_processes(tag)
        );
    }
    assert!(
        elapsed < Duration::from_secs(2),
        "旧实例应由 Drain 优雅排空（未走到强杀兜底），实际耗时 {elapsed:?}"
    );

    // 新实例最终可 drain 干净退出。
    handle
        .drain(Duration::from_secs(3))
        .await
        .expect("drain ok");
    assert_eq!(handle.state(), PluginState::Stopped);
    if counting {
        assert!(
            wait_for(Duration::from_secs(2), || peer_processes(tag) == Some(0)).await,
            "drain 后所有实例应退出；当前计数 = {:?}",
            peer_processes(tag)
        );
    }
}

// ── b/c. 非 Ready 拒绝 ──────────────────────────────────────────────────────

/// 非 Ready（Stopped）拒绝：`Err(Protocol)`、消息含 "not ready"、不排队。
#[tokio::test]
async fn b_upgrade_rejected_when_stopped() {
    let handle = start_plugin(Box::new(StdioTransport::new(PEER)), json!({})).await;
    handle
        .drain(Duration::from_secs(3))
        .await
        .expect("drain ok");
    assert_eq!(handle.state(), PluginState::Stopped);

    let err = handle
        .upgrade(json!({"gen": 2}), Duration::from_secs(1))
        .await
        .expect_err("upgrade on a stopped plugin must fail");
    match &err {
        TransportError::Protocol(msg) => {
            assert!(
                msg.contains("not ready for upgrade"),
                "unexpected message: {msg}"
            );
        }
        other => panic!("expected Protocol, got {other:?}"),
    }
    assert_eq!(handle.hot_replaces(), 0);
}

/// 崩溃重启窗口（Pending）拒绝且不排队；重启照常完成、服务不受影响。
#[tokio::test]
async fn c_upgrade_rejected_while_pending() {
    // 拉长 backoff 以稳定命中 Pending 窗口。
    let supervisor = PluginSupervisor::new().with_restart_policy(RestartPolicy {
        initial_backoff: Duration::from_secs(2),
        max_backoff: Duration::from_secs(2),
        max_restarts: 5,
    });
    let handle = supervisor
        .start(spec(json!({})), Box::new(StdioTransport::new(PEER)))
        .await
        .expect("handshake ok");

    let out = handle
        .invoke("echo", ctx(5_000), json!({"crash": true}))
        .await;
    assert!(
        matches!(&out, InvokeOutcome::Error { code, .. } if code == "plugin_crashed"),
        "expected plugin_crashed, got {out:?}"
    );
    assert!(
        wait_for(Duration::from_secs(2), || handle.state()
            == PluginState::Pending)
        .await,
        "崩溃后应进入 Pending（backoff 重启窗口）；state={:?}",
        handle.state()
    );

    let err = handle
        .upgrade(json!({"gen": 2}), Duration::from_secs(1))
        .await
        .expect_err("upgrade while pending must fail");
    match &err {
        TransportError::Protocol(msg) => assert!(
            msg.contains("not ready for upgrade"),
            "unexpected message: {msg}"
        ),
        other => panic!("expected Protocol, got {other:?}"),
    }
    assert_eq!(handle.hot_replaces(), 0, "被拒绝的升级不得计数");

    // 重启照常完成（backoff 2s），服务恢复。
    assert!(
        wait_for(Duration::from_secs(10), || handle.state()
            == PluginState::Ready
            && handle.restarts() == 1)
        .await,
        "plugin should restart; state={:?} restarts={}",
        handle.state(),
        handle.restarts()
    );
    let out = handle
        .invoke("echo", ctx(5_000), json!({"text": "back"}))
        .await;
    assert_eq!(out, InvokeOutcome::ok("back"));
    handle
        .drain(Duration::from_secs(3))
        .await
        .expect("drain ok");
}

// ── d. inproc 握手失败保老 ──────────────────────────────────────────────────

/// inproc 正常实例：完整握手 + `echo` 回显（text 或整个 payload）。
struct EchoInstance;

impl InprocPlugin for EchoInstance {
    fn run(self: Box<Self>, mut io: InprocIo) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            loop {
                match io.recv().await {
                    None => return,
                    Some(HostToPlugin::Hello(hello)) => {
                        let _ = io
                            .send(PluginToHost::Welcome(Welcome {
                                protocol: hello.protocol.clone(),
                                version: hello.version,
                                plugin_id: hello.plugin_id.clone(),
                                capabilities: vec![capabilities::SERVICES.to_string()],
                            }))
                            .await;
                        let _ = io
                            .send(PluginToHost::Register(Register {
                                contributions: vec![Contribution::Custom {
                                    kind: "host_defined".to_string(),
                                    name: "echo".to_string(),
                                    payload: json!({"description": "回显"}),
                                }],
                            }))
                            .await;
                        let _ = io.send(PluginToHost::Ready).await;
                    }
                    Some(HostToPlugin::Invoke(inv)) => {
                        let text = inv
                            .payload
                            .get("text")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_else(|| inv.payload.to_string());
                        let _ = io
                            .send(PluginToHost::InvokeResult(InvokeResult {
                                call_id: inv.call_id.clone(),
                                outcome: InvokeOutcome::ok(text),
                            }))
                            .await;
                    }
                    Some(HostToPlugin::Drain(_)) => return,
                    Some(_) => {}
                }
            }
        })
    }
}

/// inproc 死亡实例：启动即结束（模拟新实例启动后握手失败 / 立即退出）。
struct DeadInstance;

impl InprocPlugin for DeadInstance {
    fn run(self: Box<Self>, _io: InprocIo) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async {})
    }
}

/// 握手失败保老：新实例启动即退出 → `upgrade` Err；旧实例继续服务、
/// 贡献与计数不变（可重试 / 回滚）。
#[tokio::test]
async fn d_failed_handshake_keeps_current_instance() {
    let instances = Arc::new(AtomicUsize::new(0));
    let counter = instances.clone();
    let transport = InprocTransport::new(move |spec: PluginSpec| -> Box<dyn InprocPlugin> {
        counter.fetch_add(1, Ordering::SeqCst);
        if spec.config.get("fail_handshake").and_then(Value::as_bool) == Some(true) {
            Box::new(DeadInstance)
        } else {
            Box::new(EchoInstance)
        }
    });
    let handle = start_plugin(Box::new(transport), json!({"k": 1})).await;
    let registered = handle.contributions();
    assert_eq!(registered.len(), 1, "echo 工具已注册");
    assert_eq!(instances.load(Ordering::SeqCst), 1);

    let out = handle
        .invoke("echo", ctx(5_000), json!({"text": "before"}))
        .await;
    assert_eq!(out, InvokeOutcome::ok("before"));

    let err = handle
        .upgrade(json!({"fail_handshake": true}), Duration::from_secs(1))
        .await
        .expect_err("a dying replacement must fail the upgrade");
    assert!(
        matches!(err, TransportError::Closed | TransportError::Protocol(_)),
        "expected Closed/Protocol, got {err:?}"
    );

    assert_eq!(instances.load(Ordering::SeqCst), 2, "新实例确实被启动过");
    assert_eq!(handle.hot_replaces(), 0, "失败的升级不得计数");
    assert_eq!(handle.state(), PluginState::Ready, "旧实例保持 Ready");
    assert_eq!(handle.contributions(), registered, "失败升级不得覆盖贡献");

    let out = handle
        .invoke("echo", ctx(5_000), json!({"text": "still-old"}))
        .await;
    assert_eq!(out, InvokeOutcome::ok("still-old"), "旧实例应继续服务");

    handle
        .drain(Duration::from_secs(2))
        .await
        .expect("drain ok");
    assert_eq!(handle.state(), PluginState::Stopped);
}

// ── e. 连续升级 ─────────────────────────────────────────────────────────────

/// 连续两次升级：`hot_replaces` 累积、实例逐代更替（每次均回到 Ready 后再升级）。
#[tokio::test]
async fn e_sequential_upgrades_advance_generation() {
    let handle = start_plugin(Box::new(StdioTransport::new(PEER)), json!({"gen": 0})).await;
    assert_eq!(invoke_config(&handle).await, json!({"gen": 0}));

    handle
        .upgrade(json!({"gen": 1}), Duration::from_secs(2))
        .await
        .expect("first upgrade should succeed");
    assert_eq!(handle.hot_replaces(), 1);
    assert_eq!(invoke_config(&handle).await, json!({"gen": 1}));

    handle
        .upgrade(json!({"gen": 2}), Duration::from_secs(2))
        .await
        .expect("second upgrade should succeed");
    assert_eq!(handle.hot_replaces(), 2);
    assert_eq!(invoke_config(&handle).await, json!({"gen": 2}));
    assert_eq!(handle.state(), PluginState::Ready);

    handle
        .drain(Duration::from_secs(3))
        .await
        .expect("drain ok");
    assert_eq!(handle.state(), PluginState::Stopped);
}

// ── f. 升级交接期间 HostCall 回执按来源连接路由（W1.2 回归）─────────────────

/// 回执记录：`(代, call_id, outcome)`——按插件实例代记，用于断言「回执到了
/// 哪一代的连接」。
type HandoffLog = std::sync::Mutex<Vec<(usize, String, HostCallOutcome)>>;

/// 换代测试插件。
///
/// - **第 1 代（交接中的旧实例）**：收到 `Drain` 后发一条 `HostCall` 并等待回执；
///   回执到达 → 记录（代=1）→ 退出（结束排空）。这正是热替换交接的窗口：
///   此时 `current_conn()` 已是第 2 代，回执必须按**来源连接**（第 1 代）
///   回发（W1.2 修复前会错投第 2 代）。
/// - **第 2 代（新实例）**：正常服务；若收到 `HostCallResult`（= 错投泄漏）
///   也记录（代=2），供用例断言零泄漏。
struct HandoffPlugin {
    generation: usize,
    log: Arc<HandoffLog>,
}

impl InprocPlugin for HandoffPlugin {
    fn run(self: Box<Self>, mut io: InprocIo) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            loop {
                let Some(msg) = io.recv().await else {
                    return;
                };
                match msg {
                    HostToPlugin::Hello(hello) => {
                        let _ = io
                            .send(PluginToHost::Welcome(Welcome {
                                protocol: hello.protocol.clone(),
                                version: hello.version,
                                plugin_id: hello.plugin_id.clone(),
                                capabilities: vec![capabilities::HOST_CALL.to_string()],
                            }))
                            .await;
                        let _ = io.send(PluginToHost::Ready).await;
                    }
                    HostToPlugin::HostCallResult(result) => {
                        self.log.lock().unwrap().push((
                            self.generation,
                            result.call_id.clone(),
                            result.outcome,
                        ));
                        if self.generation == 1 {
                            return; // 旧实例：收到回执即退出（排空完成）
                        }
                    }
                    HostToPlugin::Drain(_) => {
                        if self.generation != 1 {
                            return; // 新实例照常排空退出
                        }
                        // 交接窗口：排空前发 HostCall，等回执（上限交给用例超时兜底）。
                        let call = HostCall {
                            call_id: "host:g1".to_string(),
                            service: "svc".to_string(),
                            method: "ping".to_string(),
                            payload: json!({}),
                        };
                        if !io.send(PluginToHost::HostCall(call)).await {
                            return;
                        }
                    }
                    HostToPlugin::Dispose => return,
                    _ => {}
                }
            }
        })
    }
}

/// 服务：记录调用次数并回 `{"pong": true}`。
struct PingService {
    calls: Arc<AtomicUsize>,
}

impl echo_plugin_host::host_service::HostService for PingService {
    fn call(&self, _method: &str, _payload: Value) -> Result<Value, (String, String)> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"pong": true}))
    }
}

/// 升级交接：旧实例在排空期发出的 `HostCall` 回执必须回到旧连接——
/// 不错投新实例、不丢失（旧实例能收到并完成排空）。
#[tokio::test]
async fn f_drain_window_hostcall_reply_routes_to_source_connection() {
    let supervisor = PluginSupervisor::new();
    let calls = Arc::new(AtomicUsize::new(0));
    supervisor.host_services().register(
        "svc",
        Arc::new(PingService {
            calls: calls.clone(),
        }),
    );

    let generations = Arc::new(AtomicUsize::new(0));
    let log: Arc<HandoffLog> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let transport = {
        let generations = generations.clone();
        let log = log.clone();
        InprocTransport::new(move |_spec: PluginSpec| -> Box<dyn InprocPlugin> {
            let generation = generations.fetch_add(1, Ordering::SeqCst) + 1;
            Box::new(HandoffPlugin {
                generation,
                log: log.clone(),
            })
        })
    };
    let handle = supervisor
        .start(spec(json!({})), Box::new(transport))
        .await
        .expect("handshake ok");

    // 交接：新实例（第 2 代）握手 → 切换 → 旧实例（第 1 代）排空。
    // 排空期旧实例发 HostCall；upgrade 返回即表示旧实例已完成排空退出。
    handle
        .upgrade(json!({"gen": 2}), Duration::from_secs(3))
        .await
        .expect("upgrade should succeed");
    assert_eq!(handle.hot_replaces(), 1);

    let entries = log.lock().unwrap().clone();
    let gen1: Vec<_> = entries.iter().filter(|(g, ..)| *g == 1).collect();
    let gen2: Vec<_> = entries.iter().filter(|(g, ..)| *g == 2).collect();

    assert_eq!(
        gen1.len(),
        1,
        "旧实例（第 1 代）必须收到它自己发出的回执；实际记录 = {entries:?}"
    );
    assert_eq!(gen1[0].1, "host:g1");
    match &gen1[0].2 {
        HostCallOutcome::Ok { result } => assert_eq!(result, &json!({"pong": true})),
        other => panic!("expected Ok outcome, got {other:?}"),
    }
    assert!(
        gen2.is_empty(),
        "回执不得错投新实例（第 2 代）；实际记录 = {entries:?}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "服务应被恰好调用一次");

    handle
        .drain(Duration::from_secs(3))
        .await
        .expect("drain ok");
    assert_eq!(handle.state(), PluginState::Stopped);
}

// ── 可逆停止（Stop ↔ Start）────────────────────────────────────────────────
//
// 与 drain 的差别是**监督任务不退出**：停掉之后还能再拉起来，因此面板的
// 启停开关能真正停掉子进程而不必重启 Core。

/// stop → Stopped（任务存活）→ start → Ready 且服务恢复。
#[tokio::test]
async fn stop_then_start_round_trips_a_plugin() {
    let handle = start_plugin(Box::new(StdioTransport::new(PEER)), json!({"k": 1})).await;
    assert_eq!(handle.state(), PluginState::Ready);
    assert_eq!(invoke_config(&handle).await, json!({"k": 1}));

    handle.stop(Duration::from_secs(3)).await.expect("stop ok");
    assert_eq!(handle.state(), PluginState::Stopped);

    // 停止期间调用应立刻失败（连接已关闭）而不是挂死到超时。
    let started = Instant::now();
    let out = handle.invoke("echo", ctx(5_000), json!({"k": 1})).await;
    assert!(
        matches!(out, InvokeOutcome::Error { .. }),
        "invoke after stop must fail, got {out:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "stopped plugin must fail fast, took {:?}",
        started.elapsed()
    );

    handle.start().await.expect("start ok");
    assert_eq!(handle.state(), PluginState::Ready);
    // 配置沿用同一份 PluginSpec：重启后行为与首启一致。
    assert_eq!(invoke_config(&handle).await, json!({"k": 1}));

    handle
        .drain(Duration::from_secs(3))
        .await
        .expect("drain ok");
    assert_eq!(handle.state(), PluginState::Stopped);
}

/// 重复 stop 幂等；重复 start（已 Ready）报「不是停止态」。
#[tokio::test]
async fn stop_is_idempotent_and_start_rejects_running_plugin() {
    let handle = start_plugin(Box::new(StdioTransport::new(PEER)), json!({})).await;

    // 运行中 start → 拒绝（想换实现用 upgrade）。
    let err = handle.start().await.expect_err("start on running plugin");
    assert!(format!("{err:?}").contains("not stopped"), "{err:?}");

    handle.stop(Duration::from_secs(3)).await.expect("stop ok");
    // 二次 stop：已是停止态，不制造错误。
    handle
        .stop(Duration::from_secs(3))
        .await
        .expect("second stop is idempotent");

    handle
        .drain(Duration::from_secs(3))
        .await
        .expect("drain ok");
}

/// 停止态下 drain 立刻收尾（不会等 5s 的握手或 3s 排空）。
#[tokio::test]
async fn drain_after_stop_returns_immediately() {
    let handle = start_plugin(Box::new(StdioTransport::new(PEER)), json!({})).await;
    handle.stop(Duration::from_secs(3)).await.expect("stop ok");

    let started = Instant::now();
    handle
        .drain(Duration::from_secs(3))
        .await
        .expect("drain ok");
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "drain on a stopped plugin should not wait: {:?}",
        started.elapsed()
    );
    assert_eq!(handle.state(), PluginState::Stopped);
}

/// 停止态下的 upgrade 被拒（不是「排队等重启完成」）。
#[tokio::test]
async fn upgrade_is_rejected_while_stopped() {
    let handle = start_plugin(Box::new(StdioTransport::new(PEER)), json!({"k": 1})).await;
    handle.stop(Duration::from_secs(3)).await.expect("stop ok");

    let err = handle
        .upgrade(json!({"k": 2}), Duration::from_secs(1))
        .await
        .expect_err("upgrade while stopped");
    // 前置检查在句柄侧（`state != Ready` 即拒），消息里带状态名——
    // 断言状态名而非具体措辞：措辞属于 CLI 文案，状态才是契约。
    assert!(format!("{err:?}").contains("stopped"), "{err:?}");
    // 状态不受影响（仍是停止态，可再 start）。
    assert_eq!(handle.state(), PluginState::Stopped);
    handle
        .drain(Duration::from_secs(3))
        .await
        .expect("drain ok");
}

/// 贡献集合随配置变化的对端：模拟新版插件增删/修改工具 schema。
struct ChangingInstance;

impl InprocPlugin for ChangingInstance {
    fn run(self: Box<Self>, mut io: InprocIo) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(async move {
            loop {
                match io.recv().await {
                    None => return,
                    Some(HostToPlugin::Hello(hello)) => {
                        let tool_name = hello
                            .config
                            .get("tool_name")
                            .and_then(Value::as_str)
                            .unwrap_or("echo")
                            .to_string();
                        let _ = io
                            .send(PluginToHost::Welcome(Welcome {
                                protocol: hello.protocol,
                                version: hello.version,
                                plugin_id: hello.plugin_id,
                                capabilities: vec![capabilities::SERVICES.to_string()],
                            }))
                            .await;
                        let _ = io
                            .send(PluginToHost::Register(Register {
                                contributions: vec![Contribution::Custom {
                                    kind: "host_defined".to_string(),
                                    name: tool_name.clone(),
                                    payload: json!({"description": "变化后的贡献"}),
                                }],
                            }))
                            .await;
                        let _ = io.send(PluginToHost::Ready).await;
                    }
                    Some(HostToPlugin::Invoke(inv)) => {
                        let _ = io
                            .send(PluginToHost::InvokeResult(InvokeResult {
                                call_id: inv.call_id,
                                outcome: InvokeOutcome::ok("old-instance"),
                            }))
                            .await;
                    }
                    Some(HostToPlugin::Drain(_)) => return,
                    Some(_) => {}
                }
            }
        })
    }
}

#[tokio::test]
async fn contribution_change_is_rejected_before_traffic_switch() {
    let transport = InprocTransport::new(|_spec: PluginSpec| -> Box<dyn InprocPlugin> {
        Box::new(ChangingInstance)
    });
    let handle = start_plugin(Box::new(transport), json!({"tool_name": "echo"})).await;
    let before = handle.contributions();

    let error = handle
        .upgrade_preserving_contributions(
            json!({"tool_name": "new_tool"}),
            Duration::from_secs(1),
        )
        .await
        .expect_err("changed contributions must be rejected");
    assert!(error.to_string().contains("contributions differ"), "{error}");
    assert_eq!(handle.contributions(), before, "旧贡献必须保留");
    assert_eq!(handle.hot_replaces(), 0, "不得发生流量切换");
    assert_eq!(handle.state(), PluginState::Ready, "旧实例必须继续服务");
    let outcome = handle.invoke("echo", ctx(5_000), json!({})).await;
    assert_eq!(outcome, InvokeOutcome::ok("old-instance"));

    handle
        .upgrade_preserving_contributions(json!({"tool_name": "echo"}), Duration::from_secs(1))
        .await
        .expect("same contributions remain hot-upgradeable");
    assert_eq!(handle.hot_replaces(), 1);
    handle.drain(Duration::from_secs(1)).await.expect("drain");
}
