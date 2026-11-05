//! HostCall（P2）宿主侧集成测试：模块 → 宿主服务回呼。
//!
//! 覆盖：正常回值（服务结果原样带回）/ 未知服务（`service_not_found`）/
//! 模块退出后回发失败不 panic（监督循环继续、重启后照常服务，且结果不会
//! 串到新实例）。
//!
//! 测试模块经 [`InprocTransport`] 直连协议类型；宿主服务经
//! [`ModuleSupervisor::host_services`] 注册。

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use echo_module_host::api::{
    capabilities, HostCall, HostCallOutcome, HostCallResult, HostToModule, InvokeContext,
    InvokeOutcome, InvokeResult, ModuleToHost, Welcome, PROTOCOL_NAME, PROTOCOL_VERSION,
};
use echo_module_host::host_service::HostService;
use echo_module_host::inproc::{InprocIo, InprocModule, InprocTransport};
use echo_module_host::supervisor::{ModuleState, ModuleSupervisor, RestartPolicy};
use echo_module_host::transport::ModuleSpec;
use serde_json::{json, Value};

// ── 测试模块 ────────────────────────────────────────────────────────────────

/// 测试观测（模块侧共享）。
#[derive(Default)]
struct Obs {
    /// 模块发出的 HostCall（按到达顺序）。
    host_calls: Vec<HostCall>,
    /// 模块收到的 HostCallResult。
    host_call_results: Vec<HostCallResult>,
}

/// 测试模块脚本。
#[derive(Clone)]
struct CallScript {
    /// 收到 Invoke 后发出的 HostCall 的 service / method。
    service: String,
    method: String,
    /// true = 等 HostCallResult 后回 InvokeResult（结果编进文本）；
    /// false = 发出 HostCall 后直接结束 run（模拟崩溃）。
    await_result: bool,
    /// true = 不回 HostCall、直接回 `pong`（重启后的实例用）。
    direct_reply: bool,
}

impl Default for CallScript {
    fn default() -> Self {
        Self {
            service: "sanitizer".to_string(),
            method: "redact".to_string(),
            await_result: true,
            direct_reply: false,
        }
    }
}

struct CallModule {
    obs: Arc<Mutex<Obs>>,
    script: CallScript,
}

impl InprocModule for CallModule {
    fn run(self: Box<Self>, mut io: InprocIo) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let CallModule { obs, script } = *self;
        Box::pin(async move {
            // 握手：Hello → Welcome + Ready。
            let Some(HostToModule::Hello(hello)) = io.recv().await else {
                return;
            };
            let _ = io
                .send(ModuleToHost::Welcome(Welcome {
                    protocol: PROTOCOL_NAME.to_string(),
                    version: PROTOCOL_VERSION,
                    module_id: hello.module_id,
                    capabilities: vec![capabilities::HOST_CALL.to_string()],
                }))
                .await;
            let _ = io.send(ModuleToHost::Ready).await;

            loop {
                let Some(msg) = io.recv().await else {
                    return;
                };
                match msg {
                    HostToModule::Invoke(inv) => {
                        if script.direct_reply {
                            let _ = io
                                .send(ModuleToHost::InvokeResult(InvokeResult {
                                    call_id: inv.call_id.clone(),
                                    outcome: InvokeOutcome::ok("pong"),
                                }))
                                .await;
                            continue;
                        }

                        let call = HostCall {
                            call_id: "host:1".to_string(),
                            service: script.service.clone(),
                            method: script.method.clone(),
                            payload: json!({"text": "secret"}),
                        };
                        obs.lock().unwrap().host_calls.push(call.clone());
                        if !io.send(ModuleToHost::HostCall(call)).await {
                            return;
                        }
                        if !script.await_result {
                            return; // 模拟崩溃：不等结果直接退出 run
                        }
                        // 等匹配的 HostCallResult（忽略其它消息）。
                        loop {
                            match io.recv().await {
                                Some(HostToModule::HostCallResult(result)) => {
                                    obs.lock().unwrap().host_call_results.push(result.clone());
                                    let text = match &result.outcome {
                                        HostCallOutcome::Ok { result } => format!("ok:{result}"),
                                        HostCallOutcome::Error { code, message } => {
                                            format!("err:{code}:{message}")
                                        }
                                    };
                                    let _ = io
                                        .send(ModuleToHost::InvokeResult(InvokeResult {
                                            call_id: inv.call_id.clone(),
                                            outcome: InvokeOutcome::ok(text),
                                        }))
                                        .await;
                                    break;
                                }
                                Some(_) => {}
                                None => return,
                            }
                        }
                    }
                    HostToModule::Drain(_) => return,
                    HostToModule::Dispose => return,
                    _ => {}
                }
            }
        })
    }
}

// ── 测试服务 ────────────────────────────────────────────────────────────────

/// 正常服务：`redact` 返回 `{"redacted": "***"}`；其余方法 `method_not_found`。
struct Sanitizer;

impl HostService for Sanitizer {
    fn call(&self, method: &str, _payload: Value) -> Result<Value, (String, String)> {
        match method {
            "redact" => Ok(json!({"redacted": "***"})),
            other => Err((
                "method_not_found".to_string(),
                format!("unknown method: {other}"),
            )),
        }
    }
}

/// 慢服务：睡 `delay` 再返回（连接关闭用例：返回时模块已退出）。
struct SlowService {
    delay: Duration,
}

impl HostService for SlowService {
    fn call(&self, _method: &str, _payload: Value) -> Result<Value, (String, String)> {
        std::thread::sleep(self.delay);
        Ok(json!({"late": true}))
    }
}

// ── 测试辅助 ────────────────────────────────────────────────────────────────

fn spec() -> ModuleSpec {
    ModuleSpec {
        module_id: "host-call-module".to_string(),
        config: json!({}),
    }
}

fn transport_for(script: CallScript, obs: Arc<Mutex<Obs>>) -> InprocTransport {
    InprocTransport::new(move |_spec: ModuleSpec| -> Box<dyn InprocModule> {
        Box::new(CallModule {
            obs: obs.clone(),
            script: script.clone(),
        })
    })
}

/// 测试用调用上下文（5s 上限，防止用例卡死）。
fn ctx() -> InvokeContext {
    InvokeContext {
        deadline_ms: Some(5_000),
        ..InvokeContext::default()
    }
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

// ── 用例 ────────────────────────────────────────────────────────────────────

/// 正常回值：注册服务 → 模块 invoke 中回呼 → 结果原样带回（call_id 配对）。
#[tokio::test]
async fn host_call_roundtrip_returns_service_result() {
    let supervisor = ModuleSupervisor::new();
    supervisor
        .host_services()
        .register("sanitizer", Arc::new(Sanitizer));

    let obs = Arc::new(Mutex::new(Obs::default()));
    let transport = transport_for(CallScript::default(), obs.clone());
    let handle = supervisor
        .start(spec(), Box::new(transport))
        .await
        .expect("handshake ok");

    let out = handle.invoke("call", ctx(), json!({})).await;
    match out {
        InvokeOutcome::Ok { text, .. } => {
            assert!(text.starts_with("ok:"), "unexpected text: {text}");
        }
        other => panic!("expected Ok outcome, got {other:?}"),
    }

    let ob = obs.lock().unwrap();
    assert_eq!(ob.host_calls.len(), 1);
    assert_eq!(ob.host_calls[0].call_id, "host:1");
    assert_eq!(ob.host_calls[0].service, "sanitizer");
    assert_eq!(ob.host_calls[0].method, "redact");
    assert_eq!(ob.host_calls[0].payload, json!({"text": "secret"}));
    assert_eq!(ob.host_call_results.len(), 1);
    assert_eq!(ob.host_call_results[0].call_id, "host:1");
    match &ob.host_call_results[0].outcome {
        HostCallOutcome::Ok { result } => assert_eq!(result["redacted"], json!("***")),
        other => panic!("expected Ok outcome, got {other:?}"),
    }
}

/// 未知服务：注册表返回 `service_not_found`，错误码原样到达模块。
#[tokio::test]
async fn unknown_service_maps_to_service_not_found() {
    let supervisor = ModuleSupervisor::new(); // 未注册任何服务

    let obs = Arc::new(Mutex::new(Obs::default()));
    let script = CallScript {
        service: "nope".to_string(),
        ..Default::default()
    };
    let transport = transport_for(script, obs.clone());
    let handle = supervisor
        .start(spec(), Box::new(transport))
        .await
        .expect("handshake ok");

    let out = handle.invoke("call", ctx(), json!({})).await;
    match out {
        InvokeOutcome::Ok { text, .. } => {
            assert!(
                text.starts_with("err:service_not_found"),
                "unexpected text: {text}"
            );
        }
        other => panic!("expected Ok outcome, got {other:?}"),
    }

    let ob = obs.lock().unwrap();
    assert_eq!(ob.host_call_results.len(), 1);
    match &ob.host_call_results[0].outcome {
        HostCallOutcome::Error { code, .. } => assert_eq!(code, "service_not_found"),
        other => panic!("expected Error outcome, got {other:?}"),
    }
}

/// 模块发出 HostCall 后立即退出：慢服务返回时连接已关闭 → 回发仅 debug
/// （不 panic）；监督循环继续自动重启，新实例照常服务，且结果不串实例。
#[tokio::test]
async fn host_call_result_send_failure_is_swallowed() {
    let supervisor = ModuleSupervisor::new().with_restart_policy(RestartPolicy {
        initial_backoff: Duration::from_millis(500),
        max_backoff: Duration::from_millis(500),
        ..Default::default()
    });
    supervisor.host_services().register(
        "slow",
        Arc::new(SlowService {
            delay: Duration::from_millis(50),
        }),
    );

    let obs = Arc::new(Mutex::new(Obs::default()));
    let instance = Arc::new(AtomicUsize::new(0));
    let obs2 = obs.clone();
    let transport = InprocTransport::new(move |_spec: ModuleSpec| -> Box<dyn InprocModule> {
        let n = instance.fetch_add(1, Ordering::SeqCst);
        let script = if n == 0 {
            CallScript {
                service: "slow".to_string(),
                method: "noop".to_string(),
                await_result: false, // 发完 HostCall 直接退出
                ..Default::default()
            }
        } else {
            CallScript {
                direct_reply: true,
                ..Default::default()
            }
        };
        Box::new(CallModule {
            obs: obs2.clone(),
            script,
        })
    });
    let handle = supervisor
        .start(spec(), Box::new(transport))
        .await
        .expect("handshake ok");

    // 第一次调用：模块发 HostCall 后退出（模拟崩溃）→ 在途调用以 module_crashed 结束。
    let out = handle.invoke("call", ctx(), json!({})).await;
    assert!(
        matches!(&out, InvokeOutcome::Error { code, .. } if code == "module_crashed"),
        "expected module_crashed, got {out:?}"
    );

    // 慢服务返回时连接已关闭：回发失败仅 debug，监督循环不受影响 → 自动重启。
    assert!(
        wait_for(Duration::from_secs(5), || handle.restarts() == 1
            && handle.state() == ModuleState::Ready)
        .await,
        "module should restart to Ready; state={:?} restarts={}",
        handle.state(),
        handle.restarts()
    );

    // 新实例照常服务。
    let out2 = handle.invoke("call", ctx(), json!({})).await;
    assert_eq!(out2, InvokeOutcome::ok("pong"));

    // 旧实例的 HostCallResult 不应串到新实例（发送失败被丢弃）。
    let ob = obs.lock().unwrap();
    assert_eq!(
        ob.host_calls.len(),
        1,
        "first instance sent exactly one HostCall"
    );
    assert!(
        ob.host_call_results.is_empty(),
        "HostCallResult must not leak to another instance: {:?}",
        ob.host_call_results
    );
}
