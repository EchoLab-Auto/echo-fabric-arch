//! 模块**自述身份**（`Register.manifest`）的宿主侧契约。
//!
//! 背景：宿主对外部模块（源码不在宿主仓里、只给一个二进制）原本只能按配置行
//! 合成身份——`kind` 只能是运输拼写（`stdio`），`package` / `requires` /
//! `provides` 全空。于是宿主拿不到任何"这模块该归谁管"的信息：作用域门控按
//! `kind` 派生（未登记的值 fail-closed 到进程级），包级启停与依赖排序也无从
//! 谈起。这条能力补齐后，"宿主完全不认识该模块的源码"才真的可行。
//!
//! 这里只测**宿主侧接缝**（握手校验 + 句柄暴露 + 重注册替换）。claim 转成
//! 宿主 `ModuleManifest` 的那一步在 `echo-module`（宿主侧类型），见其单测。

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use echo_module_host::api::*;
use echo_module_host::inproc::{InprocIo, InprocModule, InprocTransport};
use echo_module_host::supervisor::{ModuleHandle, ModuleState, ModuleSupervisor};
use echo_module_host::transport::ModuleSpec;

/// 模块脚本：握手时是否带 claim、带什么 claim、以及是否在首次 Invoke 后重注册。
#[derive(Clone, Default)]
struct Script {
    /// `Welcome.module_id`（None = 回显 Hello 的）。
    module_id: Option<String>,
    /// 首次 `Register` 携带的身份。
    claim: Option<ModuleManifestClaim>,
    /// 收到 Invoke 后再注册一次（测试重注册路径）。
    re_register: Option<ModuleManifestClaim>,
}

struct Peer {
    script: Script,
    /// 重注册是否已发生（避免重复触发）。
    re_registered: Arc<Mutex<bool>>,
}

impl InprocModule for Peer {
    fn run(self: Box<Self>, mut io: InprocIo) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let Peer {
            script,
            re_registered,
        } = *self;
        Box::pin(async move {
            // Hello → Welcome
            loop {
                match io.recv().await {
                    None => return,
                    Some(HostToModule::Hello(h)) => {
                        let id = script.module_id.clone().unwrap_or(h.module_id);
                        let _ = io
                            .send(ModuleToHost::Welcome(Welcome {
                                protocol: PROTOCOL_NAME.to_string(),
                                version: PROTOCOL_VERSION,
                                module_id: id,
                                capabilities: vec![],
                            }))
                            .await;
                        break;
                    }
                    Some(_) => {}
                }
            }
            let _ = io
                .send(ModuleToHost::Register(Register {
                    contributions: vec![],
                    manifest: script.claim.clone().map(Box::new),
                }))
                .await;
            let _ = io.send(ModuleToHost::Ready).await;

            while let Some(msg) = io.recv().await {
                match msg {
                    HostToModule::Invoke(inv) => {
                        if let Some(claim) = script.re_register.clone() {
                            // 先把「要不要发」算成一个 bool，再进 await：
                            // 持锁跨 await 会让整个 future 不是 Send。
                            let should_send = {
                                let mut done = re_registered.lock().unwrap();
                                let first = !*done;
                                *done = true;
                                first
                            };
                            if should_send {
                                let _ = io
                                    .send(ModuleToHost::Register(Register {
                                        contributions: vec![],
                                        manifest: Some(Box::new(claim)),
                                    }))
                                    .await;
                            }
                        }
                        let _ = io
                            .send(ModuleToHost::InvokeResult(InvokeResult {
                                call_id: inv.call_id,
                                outcome: InvokeOutcome::Ok {
                                    text: "ok".to_string(),
                                    images: vec![],
                                    result: serde_json::Value::Null,
                                },
                            }))
                            .await;
                    }
                    HostToModule::Drain(_) | HostToModule::Dispose => return,
                    _ => {}
                }
            }
        })
    }
}

fn spec() -> ModuleSpec {
    ModuleSpec {
        module_id: "ext.widget".to_string(),
        config: serde_json::json!({}),
    }
}

fn transport(script: Script) -> (InprocTransport, Arc<Mutex<bool>>) {
    let re_registered = Arc::new(Mutex::new(false));
    let shared = re_registered.clone();
    let transport = InprocTransport::new(move |_spec| {
        Box::new(Peer {
            script: script.clone(),
            re_registered: shared.clone(),
        })
    });
    (transport, re_registered)
}

async fn wait_for(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = std::time::Instant::now();
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

/// 声明了身份的模块：宿主能在句柄上读到完整 claim。
#[tokio::test]
async fn declared_manifest_is_exposed_on_the_handle() {
    let claim = ModuleManifestClaim::new("ext.widget", "widget")
        .with_name("小部件")
        .with_version("1.2.3")
        .with_description("宿主完全不认识的模块")
        .with_tag("outside")
        .with_author("Acme")
        .with_package("acme.widgets")
        .with_requires(["acme.host.store"]);
    let (transport, _) = transport(Script {
        claim: Some(claim.clone()),
        ..Script::default()
    });
    let supervisor = ModuleSupervisor::new();
    let handle: ModuleHandle = supervisor
        .start(spec(), Box::new(transport))
        .await
        .expect("handshake should succeed");

    let got = handle.manifest_claim().expect("claim must be exposed");
    assert_eq!(got, claim, "宿主必须原样拿到模块自述的身份");
    // 来源 / 运行时由宿主决定，claim 里根本没有这两个字段——这里确认的是
    // "模块不需要、也无法声明它们"。
    assert_eq!(got.package_id(), "acme.widgets");
}

/// 未声明身份的模块（旧模块）：`None`，宿主据此回退到"按配置行合成"。
#[tokio::test]
async fn module_without_claim_reports_none() {
    let (transport, _) = transport(Script::default());
    let supervisor = ModuleSupervisor::new();
    let handle = supervisor
        .start(spec(), Box::new(transport))
        .await
        .expect("handshake should succeed");
    assert!(
        handle.manifest_claim().is_none(),
        "旧模块不发 manifest —— 宿主必须能区分「没声明」与「声明了空身份」"
    );
}

/// 身份 id 与握手 id 不一致：握手 fail-closed（防冒充）。
#[tokio::test]
async fn mismatched_manifest_id_is_rejected() {
    let (transport, _) = transport(Script {
        claim: Some(ModuleManifestClaim::new("someone.else", "widget")),
        ..Script::default()
    });
    let supervisor = ModuleSupervisor::new();
    let err = match supervisor.start(spec(), Box::new(transport)).await {
        Ok(_) => panic!("id 不一致必须拒绝"),
        Err(e) => e,
    };
    let message = format!("{err}");
    assert!(
        message.contains("manifest id mismatch"),
        "错误消息要点名是身份 id 不一致（不是笼统的协议错误）：{message}"
    );
    assert!(
        message.contains("someone.else"),
        "错误消息要带上冒用的 id，便于定位是哪个模块：{message}"
    );
}

/// 重注册（热替换后新版模块换了身份）：身份与贡献一起整体替换。
#[tokio::test]
async fn re_registration_replaces_the_claim() {
    let first = ModuleManifestClaim::new("ext.widget", "widget").with_name("旧名");
    let second = ModuleManifestClaim::new("ext.widget", "widget").with_name("新名");
    let (transport, _re_registered) = transport(Script {
        claim: Some(first.clone()),
        re_register: Some(second.clone()),
        ..Script::default()
    });
    let supervisor = ModuleSupervisor::new();
    let handle = supervisor
        .start(spec(), Box::new(transport))
        .await
        .expect("handshake should succeed");
    assert_eq!(handle.manifest_claim(), Some(first));

    // 触发一次 Invoke → 模块重注册。
    let _ = handle
        .invoke("noop", InvokeContext::default(), serde_json::json!({}))
        .await;
    assert!(
        wait_for(Duration::from_secs(2), || {
            handle.manifest_claim() == Some(second.clone())
        })
        .await,
        "重注册后句柄上的身份应更新为新值（实际：{:?}）",
        handle.manifest_claim().map(|c| c.name)
    );
}

/// 重注册带**错 id** 时整帧被忽略：身份不变（也不能只换一半）。
#[tokio::test]
async fn re_registration_with_mismatched_id_is_ignored() {
    let first = ModuleManifestClaim::new("ext.widget", "widget").with_name("正确");
    let bogus = ModuleManifestClaim::new("someone.else", "widget").with_name("冒用");
    let (transport, re_registered) = transport(Script {
        claim: Some(first.clone()),
        re_register: Some(bogus),
        ..Script::default()
    });
    let supervisor = ModuleSupervisor::new();
    let handle = supervisor
        .start(spec(), Box::new(transport))
        .await
        .expect("handshake should succeed");

    let _ = handle
        .invoke("noop", InvokeContext::default(), serde_json::json!({}))
        .await;
    assert!(
        wait_for(Duration::from_secs(2), || *re_registered.lock().unwrap()).await,
        "模块应已发出重注册帧"
    );
    // 给 dispatch 一点时间处理那一帧（它是同步的，这里只是避免与 invoke 的结果
    // 竞争——帧先于 InvokeResult 到达，所以等到结果就该已处理完）。
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        handle.manifest_claim(),
        Some(first),
        "错 id 的重注册必须整帧丢弃，不能半换（身份与贡献要么一起换要么都不换）"
    );
    assert_eq!(handle.state(), ModuleState::Ready, "拒帧不应导致模块掉线");
}
