//! conformance 测试套件（Phase 0 验收）：对 `InprocTransport` 全量跑协议一致性用例。
//!
//! 覆盖十组核心场景 + 四组补充：
//! a. 握手成功（Welcome + Register + Ready，贡献字段原样保留）
//! b. 版本不兼容（Welcome.version=999 → start 返回 Err）
//! c. invoke 成功往返（Ok{text}）
//! d. invoke 错误结果（Error{code:"upstream"}）
//! e. cancel 在途（Cancel → Error{code:"cancelled"} 原样透传）
//! f. 超时（无响应 → Error{code:"timeout"}，且插件随后收到 Cancel）
//! g. Emit 事件收集（take_events 可见）
//! h. 崩溃重启（plugin_crashed → 自动重启 → Ready → 再次调用成功）
//! i. drain（Stopped + 插件侧观察到 Drain）
//! j. Stopped 后 invoke 返回 Error
//! k. 运输层直测：插件退出后 recv / send 返回 Closed
//! l. Register 整体替换语义
//! m. 握手超时（5s 无 Welcome → Err(Timeout)）
//! n. 契约形状：unit variant 帧只含 type（无 payload 字段）
//!
//! 说明：测试插件 [`TestPlugin`] 是可编程脚本（[`Script`] + [`Reaction`]），
//! 每次 start / 重启经工厂构造新实例；观测（收到的消息）写入共享 [`Obs`]。

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use echo_plugin_host::api::InvokeResult;
use echo_plugin_host::api::*;
use echo_plugin_host::inproc::{InprocIo, InprocPlugin, InprocTransport};
use echo_plugin_host::supervisor::{PluginHandle, PluginState, PluginSupervisor};
use echo_plugin_host::transport::{PluginSpec, Transport, TransportError};
use serde_json::{json, Value};

// ── 测试插件 ────────────────────────────────────────────────────────────────

/// 测试插件观测到的宿主消息（测试侧共享）。
#[derive(Default)]
struct Obs {
    hello: Option<Hello>,
    invokes: Vec<Invoke>,
    cancels: Vec<Cancel>,
    drains: Vec<Drain>,
    ready_sent: bool,
    /// 插件侧看到宿主关闭通道（recv → None）。
    channel_closed: bool,
}

/// 可编程插件脚本。
#[derive(Clone)]
struct Script {
    /// false = 不发 Welcome（模拟握手无响应）。
    send_welcome: bool,
    /// Welcome.version（用于不兼容用例）。
    welcome_version: u32,
    /// Welcome.plugin_id 覆盖（None = 回显 Hello.plugin_id）。
    welcome_plugin_id: Option<String>,
    /// 注册的贡献（空 = 不发 Register）。
    contributions: Vec<Contribution>,
    /// 对 Invoke 的反应。
    reaction: Reaction,
    /// Ready 后立即发送的事件。
    emit_after_ready: Option<(String, Value)>,
    /// 收到 Drain 后退出 run（默认 true）。
    exit_on_drain: bool,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            send_welcome: true,
            welcome_version: PROTOCOL_VERSION,
            welcome_plugin_id: None,
            contributions: vec![],
            reaction: Reaction::ReplyOk,
            emit_after_ready: None,
            exit_on_drain: true,
        }
    }
}

/// 插件对 Invoke / Cancel 的反应。
#[derive(Clone)]
enum Reaction {
    /// Invoke → `Ok{text}`（text 取 payload["text"]，缺省回显整个 payload）。
    ReplyOk,
    /// Invoke → `Error{code, message}`。
    ReplyError { code: String, message: String },
    /// Invoke 不响应；Cancel → `Error{code:"cancelled"}`。
    WaitCancel,
    /// Invoke / Cancel 均不响应（仅记录；用于超时用例）。
    Silent,
    /// 收到 Invoke 后直接结束 run（模拟崩溃）。
    CrashOnInvoke,
    /// 收到 Invoke：先 Register 新集合（整体替换），再回 `Ok{"refreshed"}`。
    Refresh { next: Vec<Contribution> },
}

struct TestPlugin {
    obs: Arc<Mutex<Obs>>,
    script: Script,
}

impl InprocPlugin for TestPlugin {
    fn run(self: Box<Self>, mut io: InprocIo) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let TestPlugin { obs, script } = *self;
        Box::pin(async move {
            if !script.send_welcome {
                // 沉默握手：只消费消息，等宿主关闭通道。
                while io.recv().await.is_some() {}
                obs.lock().unwrap().channel_closed = true;
                return;
            }

            // 等 Hello → 回 Welcome（版本 / id 可按脚本篡改）。
            loop {
                match io.recv().await {
                    None => {
                        obs.lock().unwrap().channel_closed = true;
                        return;
                    }
                    Some(HostToPlugin::Hello(h)) => {
                        let id = script
                            .welcome_plugin_id
                            .clone()
                            .unwrap_or_else(|| h.plugin_id.clone());
                        obs.lock().unwrap().hello = Some(h);
                        let _ = io
                            .send(PluginToHost::Welcome(Welcome {
                                protocol: PROTOCOL_NAME.to_string(),
                                version: script.welcome_version,
                                plugin_id: id,
                                capabilities: vec!["tools".to_string()],
                            }))
                            .await;
                        break;
                    }
                    Some(_) => {} // 忽略；正常不会发生
                }
            }

            if !script.contributions.is_empty() {
                let _ = io
                    .send(PluginToHost::Register(Register {
                        contributions: script.contributions.clone(),
                    }))
                    .await;
            }
            let _ = io.send(PluginToHost::Ready).await;
            obs.lock().unwrap().ready_sent = true;
            if let Some((event, payload)) = &script.emit_after_ready {
                let _ = io
                    .send(PluginToHost::Emit(Emit {
                        event: event.clone(),
                        payload: payload.clone(),
                    }))
                    .await;
            }

            // 消息循环。
            loop {
                let Some(msg) = io.recv().await else {
                    obs.lock().unwrap().channel_closed = true;
                    return;
                };
                match msg {
                    HostToPlugin::Invoke(inv) => {
                        obs.lock().unwrap().invokes.push(inv.clone());
                        match &script.reaction {
                            Reaction::ReplyOk => {
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
                            Reaction::ReplyError { code, message } => {
                                let _ = io
                                    .send(PluginToHost::InvokeResult(InvokeResult {
                                        call_id: inv.call_id.clone(),
                                        outcome: InvokeOutcome::Error {
                                            code: code.clone(),
                                            message: message.clone(),
                                        },
                                    }))
                                    .await;
                            }
                            Reaction::WaitCancel | Reaction::Silent => {}
                            Reaction::CrashOnInvoke => return, // run 结束 = 插件退出
                            Reaction::Refresh { next } => {
                                let _ = io
                                    .send(PluginToHost::Register(Register {
                                        contributions: next.clone(),
                                    }))
                                    .await;
                                let _ = io
                                    .send(PluginToHost::InvokeResult(InvokeResult {
                                        call_id: inv.call_id.clone(),
                                        outcome: InvokeOutcome::ok("refreshed"),
                                    }))
                                    .await;
                            }
                        }
                    }
                    HostToPlugin::Cancel(c) => {
                        obs.lock().unwrap().cancels.push(c.clone());
                        if matches!(&script.reaction, Reaction::WaitCancel) {
                            let _ = io
                                .send(PluginToHost::InvokeResult(InvokeResult {
                                    call_id: c.call_id.clone(),
                                    outcome: InvokeOutcome::Error {
                                        code: "cancelled".to_string(),
                                        message: "cancelled by host".to_string(),
                                    },
                                }))
                                .await;
                        }
                    }
                    HostToPlugin::Drain(d) => {
                        obs.lock().unwrap().drains.push(d.clone());
                        if script.exit_on_drain {
                            return;
                        }
                    }
                    HostToPlugin::Dispose => return,
                    HostToPlugin::Event(_) | HostToPlugin::Hello(_) => {}
                    HostToPlugin::HostCallResult(_) => {} // 测试插件不发起 HostCall：忽略
                }
            }
        })
    }
}

// ── 测试辅助 ────────────────────────────────────────────────────────────────

/// 构造使用指定脚本的 in-proc 运输层（每次 start / 重启都新建插件实例）。
fn transport_with(script: Script, obs: Arc<Mutex<Obs>>) -> InprocTransport {
    InprocTransport::new(move |_spec: PluginSpec| -> Box<dyn InprocPlugin> {
        Box::new(TestPlugin {
            obs: obs.clone(),
            script: script.clone(),
        })
    })
}

fn spec() -> PluginSpec {
    PluginSpec {
        plugin_id: "test-plugin".to_string(),
        config: json!({"k": 1}),
    }
}

/// 测试用调用上下文（5s 上限，防止用例卡死；超时语义单独用例验证）。
fn ctx() -> InvokeContext {
    InvokeContext {
        deadline_ms: Some(5_000),
        ..InvokeContext::default()
    }
}

/// 启动插件并断言握手成功。
async fn start_plugin(script: Script) -> (PluginHandle, Arc<Mutex<Obs>>) {
    let obs = Arc::new(Mutex::new(Obs::default()));
    let transport = transport_with(script, obs.clone());
    let supervisor = PluginSupervisor::new();
    let handle = supervisor
        .start(spec(), Box::new(transport))
        .await
        .expect("handshake should succeed");
    (handle, obs)
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

fn expect_err(result: Result<PluginHandle, TransportError>, what: &str) -> TransportError {
    match result {
        Ok(_) => panic!("expected error: {what}"),
        Err(e) => e,
    }
}

/// 一个可寻址的贡献（`kind` 由宿主自定义，框架只透传）。
fn op(name: &str) -> Contribution {
    Contribution::Custom {
        kind: "host_defined".to_string(),
        name: name.to_string(),
        payload: json!({
            "description": format!("{name} 操作"),
            "schema": {"type": "object", "properties": {"text": {"type": "string"}}},
            "priority": 42,
        }),
    }
}

// ── a–j：协议核心场景 ───────────────────────────────────────────────────────

/// a. 握手成功：Welcome + Register + Ready → Ready，贡献字段原样保留。
#[tokio::test]
async fn a_handshake_registers_contributions() {
    let registered = op("echo");
    let script = Script {
        contributions: vec![registered.clone()],
        ..Default::default()
    };
    let (handle, obs) = start_plugin(script).await;

    assert_eq!(handle.state(), PluginState::Ready);
    assert_eq!(handle.contributions(), vec![registered]);
    let ob = obs.lock().unwrap();
    assert!(ob.hello.is_some(), "插件应收到 Hello");
    assert!(ob.ready_sent, "插件应发出 Ready");
}

/// b. 版本不兼容：Welcome.version = 999 → start 返回 Err(Protocol)。
#[tokio::test]
async fn b_incompatible_version_rejected() {
    let obs = Arc::new(Mutex::new(Obs::default()));
    let script = Script {
        welcome_version: 999,
        ..Default::default()
    };
    let transport = transport_with(script, obs.clone());
    let supervisor = PluginSupervisor::new();
    let err = expect_err(
        supervisor.start(spec(), Box::new(transport)).await,
        "version 999",
    );
    assert!(
        matches!(err, TransportError::Protocol(_)),
        "expected Protocol, got {err:?}"
    );
    // 握手失败后宿主应关闭连接（插件观察到通道关闭）。
    assert!(
        wait_for(Duration::from_secs(2), || obs
            .lock()
            .unwrap()
            .channel_closed)
        .await,
        "plugin should observe channel close after handshake failure"
    );
}

/// c. invoke 成功往返：插件回 Ok{text}。
#[tokio::test]
async fn c_invoke_roundtrip() {
    let (handle, _obs) = start_plugin(Script::default()).await;
    let out = handle
        .invoke("echo", ctx(), json!({"text": "hello from plugin"}))
        .await;
    assert_eq!(out, InvokeOutcome::ok("hello from plugin"));
    // call_id 形如 <plugin_id>:<序号>。
    let (handle2, obs) = start_plugin(Script::default()).await;
    let _ = handle2.invoke("echo", ctx(), json!({})).await;
    let call_id = obs.lock().unwrap().invokes[0].call_id.clone();
    assert!(
        call_id.starts_with("test-plugin:"),
        "unexpected call_id: {call_id}"
    );
}

/// d. invoke 错误结果：插件回 Error{code:"upstream"}。
#[tokio::test]
async fn d_invoke_error_passthrough() {
    let script = Script {
        reaction: Reaction::ReplyError {
            code: "upstream".to_string(),
            message: "boom".to_string(),
        },
        ..Default::default()
    };
    let (handle, _obs) = start_plugin(script).await;
    let out = handle.invoke("echo", ctx(), json!({})).await;
    assert_eq!(
        out,
        InvokeOutcome::Error {
            code: "upstream".to_string(),
            message: "boom".to_string()
        }
    );
}

/// e. cancel 在途：插件 recv 到 Cancel 后回 Error{code:"cancelled"}；invoke 原样返回。
#[tokio::test]
async fn e_cancel_inflight_call() {
    let script = Script {
        reaction: Reaction::WaitCancel,
        ..Default::default()
    };
    let (handle, obs) = start_plugin(script).await;

    let h2 = handle.clone();
    let call = tokio::spawn(async move { h2.invoke("echo", ctx(), json!({"text": "x"})).await });
    assert!(
        wait_for(Duration::from_secs(2), || obs.lock().unwrap().invokes.len()
            == 1)
        .await,
        "plugin should receive the invoke"
    );
    let call_id = obs.lock().unwrap().invokes[0].call_id.clone();

    handle.cancel(&call_id);
    let out = tokio::time::timeout(Duration::from_secs(2), call)
        .await
        .expect("invoke should resolve after cancel")
        .unwrap();
    assert_eq!(
        out,
        InvokeOutcome::Error {
            code: "cancelled".to_string(),
            message: "cancelled by host".to_string()
        }
    );
    assert!(
        wait_for(Duration::from_secs(2), || obs
            .lock()
            .unwrap()
            .cancels
            .iter()
            .any(|c| c.call_id == call_id))
        .await,
        "plugin should observe Cancel for the call"
    );
}

/// f. 超时：插件不响应 → Error{code:"timeout"}；随后插件收到 Cancel。
#[tokio::test]
async fn f_invoke_timeout_sends_cancel() {
    let script = Script {
        reaction: Reaction::Silent,
        ..Default::default()
    };
    let (handle, obs) = start_plugin(script).await;

    let c = InvokeContext {
        deadline_ms: Some(150),
        ..Default::default()
    };
    let t0 = Instant::now();
    let out = handle.invoke("echo", c, json!({})).await;
    assert!(
        matches!(&out, InvokeOutcome::Error { code, .. } if code == "timeout"),
        "expected timeout, got {out:?}"
    );
    assert!(
        t0.elapsed() >= Duration::from_millis(140),
        "timeout returned too early: {:?}",
        t0.elapsed()
    );

    assert!(
        wait_for(Duration::from_secs(2), || obs.lock().unwrap().invokes.len()
            == 1)
        .await,
        "plugin should receive the invoke"
    );
    let call_id = obs.lock().unwrap().invokes[0].call_id.clone();
    assert!(
        wait_for(Duration::from_secs(2), || obs
            .lock()
            .unwrap()
            .cancels
            .iter()
            .any(|c| c.call_id == call_id))
        .await,
        "plugin should observe Cancel after host timeout"
    );
}

/// g. Emit 事件收集：take_events 可见。
#[tokio::test]
async fn g_emit_events_collected() {
    let script = Script {
        emit_after_ready: Some(("note/created".to_string(), json!({"id": "n1"}))),
        ..Default::default()
    };
    let (handle, _obs) = start_plugin(script).await;

    let mut got: Vec<(String, Value)> = Vec::new();
    assert!(
        wait_for(Duration::from_secs(2), || {
            let batch = handle.take_events();
            let found = !batch.is_empty();
            got.extend(batch);
            found
        })
        .await,
        "Emit should show up in take_events"
    );
    assert_eq!(got, vec![("note/created".to_string(), json!({"id": "n1"}))]);
}

/// h. 崩溃重启：首次 invoke 使插件退出 → plugin_crashed → 自动重启 → 再次调用成功。
#[tokio::test]
async fn h_crash_restarts_plugin() {
    let obs = Arc::new(Mutex::new(Obs::default()));
    let instance = Arc::new(AtomicUsize::new(0));
    let obs2 = obs.clone();
    let transport = InprocTransport::new(move |_spec: PluginSpec| -> Box<dyn InprocPlugin> {
        let n = instance.fetch_add(1, Ordering::SeqCst);
        let script = if n == 0 {
            Script {
                reaction: Reaction::CrashOnInvoke,
                ..Default::default()
            }
        } else {
            Script::default() // 重启后的实例正常应答
        };
        Box::new(TestPlugin {
            obs: obs2.clone(),
            script,
        })
    });
    let supervisor = PluginSupervisor::new();
    let handle = supervisor
        .start(spec(), Box::new(transport))
        .await
        .expect("handshake ok");
    assert_eq!(handle.restarts(), 0);

    let crashed_at = Instant::now();
    let out = handle.invoke("echo", ctx(), json!({})).await;
    assert!(
        matches!(&out, InvokeOutcome::Error { code, .. } if code == "plugin_crashed"),
        "expected plugin_crashed, got {out:?}"
    );

    assert!(
        wait_for(Duration::from_secs(5), || handle.restarts() == 1
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

    let out2 = handle.invoke("echo", ctx(), json!({"text": "again"})).await;
    assert_eq!(
        out2,
        InvokeOutcome::ok("again"),
        "restarted plugin should serve invokes"
    );
}

/// i. drain：drain() 后 Stopped；插件侧观察到 Drain。
#[tokio::test]
async fn i_drain_stops_plugin() {
    let (handle, obs) = start_plugin(Script::default()).await;
    handle
        .drain(Duration::from_secs(2))
        .await
        .expect("drain should succeed");
    assert_eq!(handle.state(), PluginState::Stopped);
    assert_eq!(
        obs.lock().unwrap().drains.len(),
        1,
        "plugin should observe Drain"
    );
}

/// j. Stopped 后 invoke 返回 Error。
#[tokio::test]
async fn j_invoke_after_stop_returns_error() {
    let (handle, _obs) = start_plugin(Script::default()).await;
    handle
        .drain(Duration::from_secs(2))
        .await
        .expect("drain ok");
    let out = handle.invoke("echo", ctx(), json!({})).await;
    assert!(
        matches!(&out, InvokeOutcome::Error { code, .. } if code == "stopped"),
        "expected stopped, got {out:?}"
    );
}

// ── k–m：运输层直测与补充语义 ───────────────────────────────────────────────

/// k. 运输层直测：插件 run 退出后 recv / send 返回 Closed。
#[tokio::test]
async fn k_transport_reports_closed_on_plugin_exit() {
    struct ExitNow;
    impl InprocPlugin for ExitNow {
        fn run(self: Box<Self>, _io: InprocIo) -> Pin<Box<dyn Future<Output = ()> + Send>> {
            Box::pin(async {})
        }
    }

    let transport =
        InprocTransport::new(|_spec: PluginSpec| -> Box<dyn InprocPlugin> { Box::new(ExitNow) });
    let conn = transport.start(spec()).await.expect("start ok");
    assert!(
        matches!(conn.recv().await, Err(TransportError::Closed)),
        "recv after plugin exit must be Closed"
    );
    assert!(
        matches!(
            conn.send(HostToPlugin::Dispose).await,
            Err(TransportError::Closed)
        ),
        "send after plugin exit must be Closed"
    );
}

/// l. Register 整体替换语义：第二次 Register 之后旧贡献不保留。
#[tokio::test]
async fn l_register_replaces_contributions() {
    let first = op("first");
    let second = op("second");
    let script = Script {
        contributions: vec![first.clone()],
        reaction: Reaction::Refresh {
            next: vec![second.clone()],
        },
        ..Default::default()
    };
    let (handle, _obs) = start_plugin(script).await;
    assert_eq!(handle.contributions(), vec![first]);

    let out = handle.invoke("refresh", ctx(), json!({})).await;
    assert_eq!(out, InvokeOutcome::ok("refreshed"));
    assert_eq!(
        handle.contributions(),
        vec![second],
        "Register 必须整体替换（旧贡献不得残留）"
    );
}

/// m. 握手超时：插件不发 Welcome → 5s 后 start 返回 Err(Timeout)。
#[tokio::test]
async fn m_handshake_times_out() {
    let obs = Arc::new(Mutex::new(Obs::default()));
    let script = Script {
        send_welcome: false,
        ..Default::default()
    };
    let transport = transport_with(script, obs);
    let supervisor = PluginSupervisor::new();
    let t0 = Instant::now();
    let err = expect_err(
        supervisor.start(spec(), Box::new(transport)).await,
        "silent plugin",
    );
    assert!(
        matches!(err, TransportError::Timeout),
        "expected Timeout, got {err:?}"
    );
    assert!(
        t0.elapsed() >= Duration::from_millis(4_900),
        "handshake timeout should be ~5s, took {:?}",
        t0.elapsed()
    );
}

/// n. 契约形状（补充）：unit variant 帧只含 type、不含 payload 字段
/// （adjacently-tagged 的线上形状如此，文档中 `{"type": …, "payload": …}`
/// 的表述对 Ready / Dispose 不成立，见交付报告）。
#[tokio::test]
async fn n_unit_variant_frame_shape() {
    assert_eq!(
        serde_json::to_value(PluginToHost::Ready).unwrap(),
        json!({"type": "ready"})
    );
    assert_eq!(
        serde_json::to_value(HostToPlugin::Dispose).unwrap(),
        json!({"type": "dispose"})
    );
    // 有载荷的变体则始终带 payload 字段（空对象也保留）。
    assert_eq!(
        serde_json::to_value(HostToPlugin::Drain(Drain { deadline_ms: 7 })).unwrap(),
        json!({"type": "drain", "payload": {"deadline_ms": 7}})
    );
}
