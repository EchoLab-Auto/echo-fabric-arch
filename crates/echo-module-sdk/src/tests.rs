//! SDK 单元测试：经 `tokio::io::duplex` 在内存流上跑完整协议。
//!
//! 覆盖：Hello 握手（Welcome + Register + Ready）/ invoke 往返 / 未知贡献
//! error outcome / 并发 invoke 帧不交错 / Drain 退出 / 版本不兼容拒绝 /
//! ServeOptions 覆盖 / HostCall 回呼（往返 / 服务错误 / 超时 / 断开 Closed）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use echo_module_api::{
    Contribution, Drain, Hello, HostCall, HostCallOutcome, HostCallResult, HostToModule, Invoke,
    InvokeContext, InvokeOutcome, ModuleToHost, PROTOCOL_NAME, PROTOCOL_VERSION,
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

use crate::{module_id, serve_io_with, HostClient, InvokeRequest, ModuleHandler, ServeOptions};

/// 会话超时（防用例挂死）。
const TIMEOUT: Duration = Duration::from_secs(2);

/// 测试处理器：回显 `text`；`delay_ms` 控制耗时（并发用例用）。
struct TestHandler {
    drained: Arc<AtomicBool>,
}

impl TestHandler {
    fn new() -> (Self, Arc<AtomicBool>) {
        let drained = Arc::new(AtomicBool::new(false));
        (
            Self {
                drained: Arc::clone(&drained),
            },
            drained,
        )
    }
}

#[async_trait::async_trait]
impl ModuleHandler for TestHandler {
    fn contributions(&self) -> Vec<Contribution> {
        vec![Contribution::Custom {
            kind: "host_defined".to_string(),
            name: "echo".to_string(),
            payload: json!({
                "description": "回显 text（SDK 测试）",
                "schema": {
                    "type": "object",
                    "properties": {
                        "text": {"type": "string"},
                        "delay_ms": {"type": "integer"},
                    },
                },
            }),
        }]
    }

    async fn invoke(&self, req: InvokeRequest) -> InvokeOutcome {
        if let Some(delay) = req
            .payload
            .get("delay_ms")
            .and_then(serde_json::Value::as_u64)
        {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        let text = req
            .payload
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        InvokeOutcome::ok(text)
    }

    async fn on_drain(&self, _deadline_ms: u64) {
        self.drained.store(true, Ordering::SeqCst);
    }
}

/// 一个 SDK 会话：客户端流 + drain 标记 + 会话任务。
struct Session {
    client: DuplexStream,
    drained: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

fn start_session() -> Session {
    start_session_with(ServeOptions::default())
}

fn start_session_with(options: ServeOptions) -> Session {
    let (server, client) = tokio::io::duplex(64 * 1024);
    let (reader, writer) = tokio::io::split(server);
    let (handler, drained) = TestHandler::new();
    let task = tokio::spawn(serve_io_with(handler, options, reader, writer));
    Session {
        client,
        drained,
        task,
    }
}

async fn send(session: &mut Session, msg: HostToModule) {
    let body = serde_json::to_vec(&msg).expect("encode frame");
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    session.client.write_all(&frame).await.expect("write frame");
}

async fn recv(session: &mut Session) -> ModuleToHost {
    let mut len_bytes = [0u8; 4];
    session
        .client
        .read_exact(&mut len_bytes)
        .await
        .expect("read frame length");
    let len = u32::from_le_bytes(len_bytes) as usize;
    let mut body = vec![0u8; len];
    session
        .client
        .read_exact(&mut body)
        .await
        .expect("read frame body");
    serde_json::from_slice(&body).expect("decode frame")
}

fn hello(version: u32) -> HostToModule {
    HostToModule::Hello(Hello {
        protocol: PROTOCOL_NAME.to_string(),
        version,
        module_id: module_id(),
        config: json!({}),
    })
}

async fn handshake(session: &mut Session) {
    send(session, hello(PROTOCOL_VERSION)).await;
    let welcome = recv(session).await;
    assert!(
        matches!(welcome, ModuleToHost::Welcome(_)),
        "first frame must be Welcome, got {welcome:?}"
    );
    let register = recv(session).await;
    assert!(
        matches!(register, ModuleToHost::Register(_)),
        "second frame must be Register, got {register:?}"
    );
    assert_eq!(recv(session).await, ModuleToHost::Ready);
}

/// 关闭客户端（stdin EOF）并断言会话干净退出。
async fn shutdown_and_assert_clean(session: Session) {
    let Session { client, task, .. } = session;
    drop(client);
    let result = tokio::time::timeout(TIMEOUT, task)
        .await
        .expect("session should finish after client EOF")
        .expect("session task should not panic");
    result.expect("session should exit cleanly on EOF");
}

async fn expect_ok_result(session: &mut Session) -> (String, String) {
    match recv(session).await {
        ModuleToHost::InvokeResult(result) => match result.outcome {
            InvokeOutcome::Ok { text, .. } => (result.call_id, text),
            other => panic!("expected Ok outcome, got {other:?}"),
        },
        other => panic!("expected InvokeResult, got {other:?}"),
    }
}
/// 期待一条 `Error{code}` 结果，返回该 code。
async fn expect_error_code(session: &mut Session) -> String {
    match recv(session).await {
        ModuleToHost::InvokeResult(result) => match result.outcome {
            InvokeOutcome::Error { code, .. } => code,
            other => panic!("expected error outcome, got {other:?}"),
        },
        other => panic!("expected InvokeResult, got {other:?}"),
    }
}

/// Hello 握手产生 Welcome + Register + Ready（顺序与字段）。
#[tokio::test]
async fn hello_handshake_produces_welcome_register_ready() {
    let mut session = start_session();
    send(&mut session, hello(PROTOCOL_VERSION)).await;

    match recv(&mut session).await {
        ModuleToHost::Welcome(welcome) => {
            assert_eq!(welcome.protocol, PROTOCOL_NAME);
            assert_eq!(welcome.version, PROTOCOL_VERSION);
            assert_eq!(welcome.module_id, module_id());
            assert_eq!(
                welcome.capabilities,
                vec![
                    "services".to_string(),
                    "cancel".to_string(),
                    "host_call".to_string()
                ]
            );
        }
        other => panic!("expected Welcome, got {other:?}"),
    }

    match recv(&mut session).await {
        ModuleToHost::Register(register) => {
            assert_eq!(register.contributions.len(), 1);
            match &register.contributions[0] {
                Contribution::Custom {
                    kind,
                    name,
                    payload,
                } => {
                    assert_eq!(kind, "host_defined");
                    assert_eq!(name, "echo");
                    let properties = payload
                        .get("schema")
                        .and_then(|value| value.get("properties"))
                        .and_then(|value| value.as_object())
                        .expect("schema should declare properties");
                    assert!(
                        !properties.is_empty(),
                        "schema properties should be non-empty"
                    );
                }
                other => panic!("expected Custom contribution, got {other:?}"),
            }
        }
        other => panic!("expected Register, got {other:?}"),
    }

    assert_eq!(recv(&mut session).await, ModuleToHost::Ready);
    shutdown_and_assert_clean(session).await;
}

/// invoke 往返：结果原样带回 call_id，文本回显。
#[tokio::test]
async fn invoke_roundtrip_returns_result() {
    let mut session = start_session();
    handshake(&mut session).await;

    send(
        &mut session,
        HostToModule::Invoke(Invoke {
            call_id: "stdio-module:1".to_string(),
            contribution: "echo".to_string(),
            ctx: InvokeContext::default(),
            payload: json!({"text": "hello sdk"}),
        }),
    )
    .await;

    match recv(&mut session).await {
        ModuleToHost::InvokeResult(result) => {
            assert_eq!(result.call_id, "stdio-module:1");
            assert_eq!(result.outcome, InvokeOutcome::ok("hello sdk"));
        }
        other => panic!("expected InvokeResult, got {other:?}"),
    }

    shutdown_and_assert_clean(session).await;
}

/// 未注册的贡献名 → error outcome（code = `unknown_contribution`）。
#[tokio::test]
async fn unknown_contribution_returns_error_outcome() {
    let mut session = start_session();
    handshake(&mut session).await;

    send(
        &mut session,
        HostToModule::Invoke(Invoke {
            call_id: "stdio-module:7".to_string(),
            contribution: "nope".to_string(),
            ctx: InvokeContext::default(),
            payload: json!({}),
        }),
    )
    .await;

    match recv(&mut session).await {
        ModuleToHost::InvokeResult(result) => {
            assert_eq!(result.call_id, "stdio-module:7");
            match result.outcome {
                InvokeOutcome::Error { code, .. } => assert_eq!(code, "unknown_contribution"),
                other => panic!("expected error outcome, got {other:?}"),
            }
        }
        other => panic!("expected InvokeResult, got {other:?}"),
    }

    shutdown_and_assert_clean(session).await;
}

/// 并发两个 invoke：慢调用先发（delay 150ms）、快调用后发；两帧都完整可解码
/// 且各自映射到正确的 call_id / 文本（帧字节不交错）。
#[tokio::test]
async fn concurrent_invokes_do_not_interleave_frames() {
    let mut session = start_session();
    handshake(&mut session).await;

    for (call_id, text, delay_ms) in [
        ("stdio-module:1", "slow", 150_u64),
        ("stdio-module:2", "fast", 0_u64),
    ] {
        send(
            &mut session,
            HostToModule::Invoke(Invoke {
                call_id: call_id.to_string(),
                contribution: "echo".to_string(),
                ctx: InvokeContext::default(),
                payload: json!({"text": text, "delay_ms": delay_ms}),
            }),
        )
        .await;
    }

    let first = expect_ok_result(&mut session).await;
    let second = expect_ok_result(&mut session).await;
    assert_eq!(
        first,
        ("stdio-module:2".to_string(), "fast".to_string()),
        "fast invoke should complete first"
    );
    assert_eq!(second, ("stdio-module:1".to_string(), "slow".to_string()));

    shutdown_and_assert_clean(session).await;
}

/// Drain：on_drain 钩子执行；会话干净退出；客户端读到 EOF。
#[tokio::test]
async fn drain_calls_hook_and_exits() {
    let mut session = start_session();
    handshake(&mut session).await;
    send(
        &mut session,
        HostToModule::Drain(Drain { deadline_ms: 1_000 }),
    )
    .await;

    let Session {
        mut client,
        drained,
        task,
    } = session;
    let result = tokio::time::timeout(TIMEOUT, task)
        .await
        .expect("drain should end the session")
        .expect("session task should not panic");
    result.expect("drain should exit cleanly");
    assert!(drained.load(Ordering::SeqCst), "on_drain hook should run");

    let mut buf = [0u8; 1];
    let read = tokio::time::timeout(TIMEOUT, client.read(&mut buf))
        .await
        .expect("EOF should arrive after drain")
        .expect("read after drain");
    assert_eq!(read, 0, "module should close its side after drain");
}

/// 版本不兼容：写 Failed 后会话以错误结束。
#[tokio::test]
async fn incompatible_version_is_rejected_with_failed() {
    let mut session = start_session();
    send(&mut session, hello(PROTOCOL_VERSION + 1)).await;

    match recv(&mut session).await {
        ModuleToHost::Failed(failure) => {
            assert_eq!(failure.code, "incompatible_protocol");
            assert!(
                failure.message.contains("incompatible handshake"),
                "unexpected message: {}",
                failure.message
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }

    let result = tokio::time::timeout(TIMEOUT, session.task)
        .await
        .expect("session should end")
        .expect("session task should not panic");
    assert!(
        result.is_err(),
        "incompatible handshake should exit with an error"
    );
}

/// ServeOptions 覆盖 module_id / capabilities（`serve_io_with` 路径）。
#[tokio::test]
async fn serve_options_override_id_and_capabilities() {
    let mut session = start_session_with(ServeOptions {
        module_id: "custom-module".to_string(),
        capabilities: vec!["services".to_string()],
    });
    send(&mut session, hello(PROTOCOL_VERSION)).await;

    match recv(&mut session).await {
        ModuleToHost::Welcome(welcome) => {
            assert_eq!(welcome.module_id, "custom-module");
            assert_eq!(welcome.capabilities, vec!["services".to_string()]);
        }
        other => panic!("expected Welcome, got {other:?}"),
    }

    let _ = recv(&mut session).await; // Register
    let _ = recv(&mut session).await; // Ready
    shutdown_and_assert_clean(session).await;
}

/// 缺省选项（module_id 为空）= 回显宿主 Hello 的 module_id——模块零配置，
/// 实例身份由宿主掌握（与 supervisor 的 module_id 一致性校验天然吻合）。
#[tokio::test]
async fn default_options_echo_host_assigned_module_id() {
    let mut session = start_session(); // ServeOptions::default()：module_id 为空
    let mut custom_hello = hello(PROTOCOL_VERSION);
    if let HostToModule::Hello(ref mut h) = custom_hello {
        h.module_id = "host-assigned-id".to_string();
    }
    send(&mut session, custom_hello).await;

    match recv(&mut session).await {
        ModuleToHost::Welcome(welcome) => {
            assert_eq!(welcome.module_id, "host-assigned-id");
        }
        other => panic!("expected Welcome, got {other:?}"),
    }

    let _ = recv(&mut session).await; // Register
    let _ = recv(&mut session).await; // Ready
    shutdown_and_assert_clean(session).await;
}

// ── HostCall（P2）：模块 → 宿主服务回呼 ──────────────────────────────────────

/// HostCall 测试处理器：`on_ready` 保存句柄；`invoke` 用保存的句柄发起回呼。
struct HostCallHandler {
    /// `on_ready` 交付的句柄（测试侧共享；会话结束后仍可复用验证 Closed）。
    client: Arc<Mutex<Option<HostClient>>>,
}

impl HostCallHandler {
    fn new() -> (Self, Arc<Mutex<Option<HostClient>>>) {
        let client = Arc::new(Mutex::new(None));
        (
            Self {
                client: Arc::clone(&client),
            },
            client,
        )
    }
}

#[async_trait::async_trait]
impl ModuleHandler for HostCallHandler {
    fn contributions(&self) -> Vec<Contribution> {
        vec![Contribution::Custom {
            kind: "host_defined".to_string(),
            name: "call_host".to_string(),
            payload: json!({"description": "发起 HostCall（SDK 测试）"}),
        }]
    }

    async fn invoke(&self, req: InvokeRequest) -> InvokeOutcome {
        let Some(client) = self.client.lock().expect("client lock poisoned").clone() else {
            return InvokeOutcome::Error {
                code: "no_client".to_string(),
                message: "on_ready did not deliver a client".to_string(),
            };
        };
        let call = match req
            .payload
            .get("timeout_ms")
            .and_then(serde_json::Value::as_u64)
        {
            Some(ms) => {
                client
                    .call_with_timeout(
                        "sanitizer",
                        "redact",
                        json!({"text": "secret"}),
                        Duration::from_millis(ms),
                    )
                    .await
            }
            None => {
                client
                    .call("sanitizer", "redact", json!({"text": "secret"}))
                    .await
            }
        };
        let text = match call {
            Ok(result) => format!("ok:{result}"),
            Err(error) => format!("err:{error}"),
        };
        InvokeOutcome::ok(text)
    }

    async fn on_ready(&self, host: HostClient) {
        *self.client.lock().expect("client lock poisoned") = Some(host);
    }
}

/// 启动一个使用 [`HostCallHandler`] 的 SDK 会话（返回已保存句柄的共享位）。
fn start_host_call_session() -> (Session, Arc<Mutex<Option<HostClient>>>) {
    let (server, client) = tokio::io::duplex(64 * 1024);
    let (reader, writer) = tokio::io::split(server);
    let (handler, stored) = HostCallHandler::new();
    let task = tokio::spawn(serve_io_with(
        handler,
        ServeOptions::default(),
        reader,
        writer,
    ));
    (
        Session {
            client,
            drained: Arc::new(AtomicBool::new(false)),
            task,
        },
        stored,
    )
}

/// 读一帧并断言是 `HostCall`（字段与测试约定一致），返回之。
async fn recv_host_call(session: &mut Session, expected_call_id: &str) -> HostCall {
    match recv(session).await {
        ModuleToHost::HostCall(call) => {
            assert_eq!(call.call_id, expected_call_id);
            assert_eq!(call.service, "sanitizer");
            assert_eq!(call.method, "redact");
            assert_eq!(call.payload, json!({"text": "secret"}));
            call
        }
        other => panic!("expected HostCall, got {other:?}"),
    }
}

fn invoke_call_host(call_id: &str, payload: serde_json::Value) -> HostToModule {
    HostToModule::Invoke(Invoke {
        call_id: call_id.to_string(),
        contribution: "call_host".to_string(),
        ctx: InvokeContext::default(),
        payload,
    })
}

/// HostCall 往返：`on_ready` 保存句柄 → invoke 中回呼 → 响应 HostCallResult
/// → `call()` 返回结果 → InvokeResult 带回。
#[tokio::test]
async fn host_call_roundtrip_via_on_ready_client() {
    let (mut session, _stored) = start_host_call_session();
    handshake(&mut session).await;

    send(&mut session, invoke_call_host("p:1", json!({}))).await;
    let call = recv_host_call(&mut session, "host:1").await;

    send(
        &mut session,
        HostToModule::HostCallResult(HostCallResult {
            call_id: call.call_id,
            outcome: HostCallOutcome::Ok {
                result: json!({"redacted": "***"}),
            },
        }),
    )
    .await;

    match recv(&mut session).await {
        ModuleToHost::InvokeResult(result) => {
            assert_eq!(result.call_id, "p:1");
            assert_eq!(
                result.outcome,
                InvokeOutcome::ok("ok:{\"redacted\":\"***\"}")
            );
        }
        other => panic!("expected InvokeResult, got {other:?}"),
    }

    shutdown_and_assert_clean(session).await;
}

/// 宿主服务错误 → `HostCallError::Service`（错误码原样带回）。
#[tokio::test]
async fn host_call_service_error_maps_to_service_error() {
    let (mut session, _stored) = start_host_call_session();
    handshake(&mut session).await;

    send(&mut session, invoke_call_host("p:1", json!({}))).await;
    let call = recv_host_call(&mut session, "host:1").await;
    send(
        &mut session,
        HostToModule::HostCallResult(HostCallResult {
            call_id: call.call_id,
            outcome: HostCallOutcome::Error {
                code: "service_not_found".to_string(),
                message: "unknown host service: sanitizer".to_string(),
            },
        }),
    )
    .await;

    match recv(&mut session).await {
        ModuleToHost::InvokeResult(result) => match result.outcome {
            InvokeOutcome::Ok { text, .. } => {
                assert!(
                    text.contains("service_not_found"),
                    "unexpected text: {text}"
                );
            }
            other => panic!("expected Ok outcome, got {other:?}"),
        },
        other => panic!("expected InvokeResult, got {other:?}"),
    }

    shutdown_and_assert_clean(session).await;
}

/// 宿主不响应 → `call_with_timeout` 超时（不挂死）。
#[tokio::test]
async fn host_call_without_response_times_out() {
    let (mut session, _stored) = start_host_call_session();
    handshake(&mut session).await;

    let started = std::time::Instant::now();
    send(
        &mut session,
        invoke_call_host("p:1", json!({"timeout_ms": 150})),
    )
    .await;
    let _call = recv_host_call(&mut session, "host:1").await; // 故意不回结果

    match recv(&mut session).await {
        ModuleToHost::InvokeResult(result) => match result.outcome {
            InvokeOutcome::Ok { text, .. } => {
                assert!(text.contains("timed out"), "unexpected text: {text}");
            }
            other => panic!("expected Ok outcome, got {other:?}"),
        },
        other => panic!("expected InvokeResult, got {other:?}"),
    }
    assert!(
        started.elapsed() >= Duration::from_millis(140),
        "timeout returned too early: {:?}",
        started.elapsed()
    );

    shutdown_and_assert_clean(session).await;
}

/// 会话结束后（连接断开）调用 → `HostCallError::Closed`（而非挂到超时）。
#[tokio::test]
async fn host_call_after_disconnect_is_closed() {
    let (mut session, stored) = start_host_call_session();
    handshake(&mut session).await;

    // 关闭客户端流（stdin EOF）→ serve 结束（写者收口、在途表清空）。
    let Session { client, task, .. } = session;
    drop(client);
    tokio::time::timeout(TIMEOUT, task)
        .await
        .expect("session should finish after client EOF")
        .expect("session task should not panic")
        .expect("session should exit cleanly on EOF");

    let client = stored
        .lock()
        .expect("client lock poisoned")
        .clone()
        .expect("on_ready should have delivered a client");
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        client.call("sanitizer", "redact", json!({"text": "secret"})),
    )
    .await
    .expect("call after disconnect should not hang");
    assert!(
        matches!(result, Err(crate::HostCallError::Closed)),
        "expected Closed, got {result:?}"
    );
}

// ---- 自定义贡献的寻址：宿主用 `kind` 分派，`name` 参与 Invoke ----

/// 领域宿主本来就该这么做：模块注册 `Contribution::Custom`（`kind` 是宿主
/// 自己的词汇），宿主持有 `kind` 的语义，框架只负责搬运与寻址。
#[tokio::test]
async fn custom_contribution_is_addressable_by_name() {
    struct CustomModule;

    #[async_trait::async_trait]
    impl ModuleHandler for CustomModule {
        fn contributions(&self) -> Vec<Contribution> {
            vec![Contribution::Custom {
                kind: "handler".to_string(),
                name: "do_it".to_string(),
                payload: json!({"some": "host-defined shape"}),
            }]
        }

        async fn invoke(&self, req: InvokeRequest) -> InvokeOutcome {
            InvokeOutcome::ok(format!("handled:{}", req.contribution))
        }
    }

    let (server, client) = tokio::io::duplex(64 * 1024);
    let (reader, writer) = tokio::io::split(server);
    let task = tokio::spawn(serve_io_with(
        CustomModule,
        ServeOptions::default(),
        reader,
        writer,
    ));
    let mut session = Session {
        client,
        drained: Arc::new(AtomicBool::new(false)),
        task,
    };

    send(&mut session, hello(PROTOCOL_VERSION)).await;
    assert!(matches!(recv(&mut session).await, ModuleToHost::Welcome(_)));
    match recv(&mut session).await {
        ModuleToHost::Register(reg) => {
            assert_eq!(reg.contributions.len(), 1);
            assert_eq!(reg.contributions[0].custom_kind(), Some("handler"));
            assert_eq!(reg.contributions[0].target_name(), Some("do_it"));
        }
        other => panic!("expected Register, got {other:?}"),
    }
    assert_eq!(recv(&mut session).await, ModuleToHost::Ready);

    send(
        &mut session,
        HostToModule::Invoke(Invoke {
            call_id: "c1".to_string(),
            contribution: "do_it".to_string(),
            ctx: InvokeContext::default(),
            payload: json!({}),
        }),
    )
    .await;
    assert_eq!(
        expect_ok_result(&mut session).await,
        ("c1".to_string(), "handled:do_it".to_string())
    );

    // 未注册的名字 → 可读错误（不是静默转发）。
    send(
        &mut session,
        HostToModule::Invoke(Invoke {
            call_id: "c2".to_string(),
            contribution: "not_registered".to_string(),
            ctx: InvokeContext::default(),
            payload: json!({}),
        }),
    )
    .await;
    assert_eq!(
        expect_error_code(&mut session).await,
        "unknown_contribution"
    );

    shutdown_and_assert_clean(session).await;
}

/// `configure` 拒绝宿主配置 → `Failed{invalid_config}`，会话以错误退出。
#[tokio::test]
async fn configure_rejection_fails_handshake() {
    struct Picky;
    #[async_trait::async_trait]
    impl ModuleHandler for Picky {
        async fn configure(&self, config: serde_json::Value) -> Result<(), String> {
            config
                .get("port")
                .map(|_| ())
                .ok_or_else(|| "port required".to_string())
        }
        fn contributions(&self) -> Vec<Contribution> {
            Vec::new()
        }
        async fn invoke(&self, _req: InvokeRequest) -> InvokeOutcome {
            InvokeOutcome::ok("")
        }
    }

    let (server, client) = tokio::io::duplex(64 * 1024);
    let (reader, writer) = tokio::io::split(server);
    let task = tokio::spawn(serve_io_with(
        Picky,
        ServeOptions::default(),
        reader,
        writer,
    ));
    let mut session = Session {
        client,
        drained: Arc::new(AtomicBool::new(false)),
        task,
    };
    send(&mut session, hello(PROTOCOL_VERSION)).await;
    match recv(&mut session).await {
        ModuleToHost::Failed(f) => assert_eq!(f.code, "invalid_config"),
        other => panic!("expected Failed, got {other:?}"),
    }
    let result = tokio::time::timeout(TIMEOUT, session.task)
        .await
        .expect("session should finish")
        .expect("no panic");
    assert!(result.is_err());
}
