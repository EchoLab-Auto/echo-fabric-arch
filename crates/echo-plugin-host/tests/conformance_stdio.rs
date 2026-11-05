//! stdio 运输层 conformance（Phase 2）：经 [`StdioTransport`] 复跑关键协议场景。
//!
//! 对端是独立测试插件进程（`src/bin/echo-plugin-test-peer.rs`，经
//! `env!("CARGO_BIN_EXE_echo-plugin-test-peer")` 定位）。
//!
//! 覆盖场景：
//! 1. 握手 + 注册（Welcome + Register + Ready，贡献 = echo 工具）
//! 2. invoke 往返（文本回显；无 `text` 键时回显整个 payload）
//! 3. 超时 → Cancel（hang 不回结果 → Error{code:"timeout"}；
//!    对端回 Emit `peer/cancel` 证明 Cancel 已送达子进程）
//! 4. 崩溃 → 自动重启（`{"crash": true}` → plugin_crashed → 重启后再次调用成功）
//! 5. Drain 排空（Drain → 对端回 Log 后退出 → Stopped）
//! 6. 停后调用报错（Error{code:"stopped"}）
//! 7. 运输层直测：Dispose → 对端退出 → recv / send 返回 Closed；shutdown 幂等
//! 8. 分段到达容忍（对端 `--split-frames` 逐 3 字节慢写）
//! 9. 最小环境 + `with_env` 注入（子进程仅见 PATH + 显式 tag）
//! 10. shutdown 超时强杀（对端 `--stay-alive` → start_kill，类比 SIGKILL）
//!
//! 说明：inproc 套件（`tests/conformance.rs`）的脚本化插件是进程内类型直连
//! （`InprocPlugin`），无法直接复用于子进程；本套件为独立最小版。
//! 「同一套用例参数化跑全部运输层」留待后续演进。

use std::time::{Duration, Instant};

use echo_plugin_host::api::*;
use echo_plugin_host::stdio::StdioTransport;
use echo_plugin_host::supervisor::{PluginHandle, PluginState, PluginSupervisor};
use echo_plugin_host::transport::{PluginConnection, PluginSpec, Transport, TransportError};
use serde_json::{json, Value};

/// 测试对端二进制路径（cargo 注入；[[bin]] 见 Cargo.toml）。
const PEER: &str = env!("CARGO_BIN_EXE_echo-plugin-test-peer");

// ── 辅助 ────────────────────────────────────────────────────────────────────

fn peer() -> StdioTransport {
    StdioTransport::new(PEER)
}

fn spec() -> PluginSpec {
    PluginSpec {
        plugin_id: "stdio-test".to_string(),
        config: json!({"k": 1}),
    }
}

/// 测试用调用上下文（5s 上限，防止用例卡死；超时语义单独用例验证）。
fn ctx(deadline_ms: u64) -> InvokeContext {
    InvokeContext {
        deadline_ms: Some(deadline_ms),
        ..InvokeContext::default()
    }
}

/// 启动插件并断言握手成功。
async fn start_plugin(transport: StdioTransport) -> PluginHandle {
    let supervisor = PluginSupervisor::new();
    supervisor
        .start(spec(), Box::new(transport))
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

/// 收集事件直到 `event` 出现或超时；返回已收集的全部事件。
async fn wait_events(
    handle: &PluginHandle,
    event: &str,
    timeout: Duration,
) -> Vec<(String, Value)> {
    let mut collected: Vec<(String, Value)> = Vec::new();
    let found = wait_for(timeout, || {
        let batch = handle.take_events();
        let hit = batch.iter().any(|(name, _)| name == event);
        collected.extend(batch);
        hit
    })
    .await;
    assert!(
        found,
        "expected event {event:?} within {timeout:?}; got {collected:?}"
    );
    collected
}

/// 直连握手（不经 supervisor）：Hello → Welcome/Register/Ready，断言顺序与字段。
async fn handshake_direct(conn: &dyn PluginConnection) {
    conn.send(HostToPlugin::Hello(Hello {
        protocol: PROTOCOL_NAME.to_string(),
        version: PROTOCOL_VERSION,
        plugin_id: "stdio-test".to_string(),
        config: json!({}),
    }))
    .await
    .expect("send hello");

    let mut welcomed = false;
    let mut registered = false;
    loop {
        match conn.recv().await.expect("handshake frame") {
            PluginToHost::Welcome(welcome) => {
                assert_eq!(welcome.protocol, PROTOCOL_NAME);
                assert_eq!(welcome.version, PROTOCOL_VERSION);
                assert_eq!(welcome.plugin_id, "stdio-test");
                welcomed = true;
            }
            PluginToHost::Register(register) => {
                assert_eq!(register.contributions.len(), 1);
                registered = true;
            }
            PluginToHost::Ready => break,
            other => panic!("unexpected handshake message: {other:?}"),
        }
    }
    assert!(welcomed, "Welcome should precede Ready");
    assert!(registered, "Register should precede Ready");
}

// ── 场景 1–6：协议核心（经 PluginSupervisor） ──────────────────────────────

/// 1. 握手 + 注册：Ready；贡献 = echo 工具。
#[tokio::test]
async fn a_handshake_registers_echo_contribution() {
    let handle = start_plugin(peer()).await;
    assert_eq!(handle.state(), PluginState::Ready);

    let contributions = handle.contributions();
    assert_eq!(
        contributions.len(),
        1,
        "expected exactly one contribution: {contributions:?}"
    );
    // 贡献的分类是**对端（宿主）自己的词汇**，协议只透传——所以断言的是
    // "它被原样搬过来"，而不是某个具体枚举变体。
    match &contributions[0] {
        Contribution::Custom {
            kind,
            name,
            payload,
        } => {
            assert_eq!(kind, "host_defined");
            assert_eq!(name, "echo");
            assert_eq!(payload["description"], "回显 payload（stdio 测试插件）");
        }
        other => panic!("expected Custom(echo), got {other:?}"),
    }
    assert_eq!(contributions[0].target_name(), Some("echo"));
}

/// 2. invoke 往返：文本回显；payload 无 `text` 键时回显整个 payload。
#[tokio::test]
async fn b_invoke_roundtrip() {
    let handle = start_plugin(peer()).await;

    let out = handle
        .invoke("echo", ctx(5_000), json!({"text": "hello stdio"}))
        .await;
    assert_eq!(out, InvokeOutcome::ok("hello stdio"));

    let out = handle.invoke("echo", ctx(5_000), json!({"n": 7})).await;
    assert_eq!(out, InvokeOutcome::ok("{\"n\":7}"));
}

/// 3. 超时 → Cancel：hang 不回结果 → timeout；对端回 `peer/cancel` 证明取消送达。
#[tokio::test]
async fn c_timeout_sends_cancel_to_peer() {
    let handle = start_plugin(peer()).await;

    let t0 = Instant::now();
    let out = handle.invoke("echo", ctx(150), json!({"hang": true})).await;
    assert!(
        matches!(&out, InvokeOutcome::Error { code, .. } if code == "timeout"),
        "expected timeout, got {out:?}"
    );
    assert!(
        t0.elapsed() >= Duration::from_millis(140),
        "timeout returned too early: {:?}",
        t0.elapsed()
    );

    let events = wait_events(&handle, "peer/cancel", Duration::from_secs(3)).await;
    let payload = events
        .iter()
        .find(|(event, _)| event == "peer/cancel")
        .map(|(_, payload)| payload)
        .expect("peer/cancel event");
    assert_eq!(
        payload["call_id"], "stdio-test:1",
        "Cancel 应关联超时调用的 call_id"
    );
}

/// 4. 崩溃 → 自动重启：crash → plugin_crashed；重启后再次调用成功。
#[tokio::test]
async fn d_crash_restarts_plugin() {
    let handle = start_plugin(peer()).await;
    assert_eq!(handle.restarts(), 0);

    let crashed_at = Instant::now();
    let out = handle
        .invoke("echo", ctx(5_000), json!({"crash": true}))
        .await;
    assert!(
        matches!(&out, InvokeOutcome::Error { code, .. } if code == "plugin_crashed"),
        "expected plugin_crashed, got {out:?}"
    );

    assert!(
        wait_for(Duration::from_secs(10), || handle.restarts() == 1
            && handle.state() == PluginState::Ready)
        .await,
        "plugin should restart to Ready; state={:?} restarts={}",
        handle.state(),
        handle.restarts()
    );
    assert!(
        crashed_at.elapsed() >= Duration::from_millis(150),
        "restart should wait for backoff (~200ms), took {:?}",
        crashed_at.elapsed()
    );

    let out = handle
        .invoke("echo", ctx(5_000), json!({"text": "again"}))
        .await;
    assert_eq!(
        out,
        InvokeOutcome::ok("again"),
        "restarted peer should serve invokes"
    );
}

/// 5. Drain 排空：drain() → Stopped（对端收 Drain 回 Log 后退出）。
#[tokio::test]
async fn e_drain_stops_plugin() {
    let handle = start_plugin(peer()).await;
    handle
        .drain(Duration::from_secs(3))
        .await
        .expect("drain should succeed");
    assert_eq!(handle.state(), PluginState::Stopped);
}

/// 6. 停后调用报错：Stopped 状态 invoke → Error{code:"stopped"}。
#[tokio::test]
async fn f_invoke_after_stop_returns_error() {
    let handle = start_plugin(peer()).await;
    handle
        .drain(Duration::from_secs(3))
        .await
        .expect("drain ok");
    let out = handle.invoke("echo", ctx(5_000), json!({})).await;
    assert!(
        matches!(&out, InvokeOutcome::Error { code, .. } if code == "stopped"),
        "expected stopped, got {out:?}"
    );
}

// ── 场景 7–10：运输层直测与进程语义 ───────────────────────────────────────

/// 7. 运输层直测：Dispose → 对端退出 → recv / send 返回 Closed；shutdown 幂等。
#[tokio::test]
async fn g_transport_closed_after_peer_exit() {
    let transport = peer();
    let conn = transport.start(spec()).await.expect("start ok");
    handshake_direct(conn.as_ref()).await;

    conn.send(HostToPlugin::Dispose)
        .await
        .expect("send dispose");
    // 对端退出：先 recv 到 EOF（Closed），随后 send 也必为 Closed。
    assert!(
        matches!(conn.recv().await, Err(TransportError::Closed)),
        "recv after peer exit must be Closed"
    );
    assert!(
        matches!(
            conn.send(HostToPlugin::Dispose).await,
            Err(TransportError::Closed)
        ),
        "send after peer exit must be Closed"
    );

    // 子进程已退出：shutdown 等待 / 重复调用均安全。
    conn.shutdown(Duration::from_secs(2))
        .await
        .expect("shutdown ok");
    conn.shutdown(Duration::from_secs(2))
        .await
        .expect("shutdown idempotent");
}

/// 8. 分段到达：对端逐 3 字节慢写（--split-frames），握手 + invoke 仍正常。
#[tokio::test]
async fn h_split_frames_tolerated() {
    let transport = peer().with_args(["--split-frames"]);
    let handle = start_plugin(transport).await;

    let out = handle
        .invoke("echo", ctx(5_000), json!({"text": "segmented"}))
        .await;
    assert_eq!(out, InvokeOutcome::ok("segmented"));
    handle
        .drain(Duration::from_secs(5))
        .await
        .expect("drain ok");
}

/// 9. 最小环境 + with_env：子进程只见 PATH + 显式 tag，无宿主环境泄漏。
#[tokio::test]
async fn i_minimal_env_with_explicit_tag() {
    let transport = peer().with_env("ECHO_PLUGIN_TEST_PEER_TAG", "stdio-ok");
    let handle = start_plugin(transport).await;

    let events = wait_events(&handle, "peer/env", Duration::from_secs(3)).await;
    let payload = events
        .iter()
        .find(|(event, _)| event == "peer/env")
        .map(|(_, payload)| payload)
        .expect("peer/env event");
    assert_eq!(payload["tag"], "stdio-ok");
    assert_eq!(payload["has_path"], true, "PATH should be forwarded");
    let env_count = payload["env_count"].as_u64().expect("env_count number");
    assert!(
        env_count <= 2,
        "child env should contain only PATH + explicit tag, got {env_count} vars"
    );
}

/// 10. shutdown 超时强杀：对端无视 stdin EOF（--stay-alive）→
///     shutdown 超时 start_kill + wait（类比 SIGKILL）；重复调用幂等。
#[tokio::test]
async fn j_shutdown_timeout_kills_child() {
    let transport = peer().with_args(["--stay-alive"]);
    let conn = transport.start(spec()).await.expect("start ok");
    handshake_direct(conn.as_ref()).await;

    let t0 = Instant::now();
    let res = conn.shutdown(Duration::from_millis(250)).await;
    assert!(
        matches!(res, Err(TransportError::Timeout)),
        "expected Timeout, got {res:?}"
    );
    assert!(
        t0.elapsed() >= Duration::from_millis(240),
        "shutdown returned too early: {:?}",
        t0.elapsed()
    );

    // 强杀后对端已死：recv 观察 Closed；重复 shutdown 幂等。
    assert!(
        matches!(conn.recv().await, Err(TransportError::Closed)),
        "recv after kill must be Closed"
    );
    conn.shutdown(Duration::from_secs(1))
        .await
        .expect("second shutdown should be ok");
}
