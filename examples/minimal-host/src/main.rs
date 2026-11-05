//! 最小宿主（**不是 EchoAgent**）——证明这套框架可以被任何 Rust 应用使用。
//!
//! 只用框架六件套，不引入 `echo-defs` / `echo-agent` / `echo-protocol`。
//! 全文约 190 行，做了六件事：
//!
//! 1. 建一个 `Ctx`（框架的服务定位器）并注册一个服务；
//! 2. 挂载一个**进程内**插件——它的 `mount` 是一次可撤销的副作用（`Disposer`）；
//! 3. 通过 `PluginSupervisor` + `StdioTransport` 拉起一个**独立进程**插件
//!    （`examples/minimal-plugin`，4 字节长度前缀 + JSON 帧）；
//! 4. 注册一个**宿主服务**，让插件反过来调用它（`HostCall` 双向通道）；
//! 5. 调用进程外插件并打印结果（结果里带着插件问到的宿主名字）；
//! 6. 卸载进程内插件（验证撤销真的发生）并排空进程外插件。
//!
//! 运行：
//! ```text
//! cargo run -p minimal-host
//! ```
//!
//! 注意全文**没有**「工具」「会话」「人格」这类概念——框架不认识它们。宿主自己
//! 决定给插件发什么调用、怎么解释返回值。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use echo_context::{Ctx, Disposer, ServiceKey};
use echo_plugin::{BuiltinPlugin, MountContext, PluginManifest, PluginMountResult, PluginRegistry};
use echo_plugin_host::api::{Contribution, InvokeContext};
use echo_plugin_host::async_trait;
use echo_plugin_host::host_service::{AsyncHostService, CallContext};
use echo_plugin_host::stdio::StdioTransport;
use echo_plugin_host::supervisor::PluginSupervisor;
use echo_plugin_host::transport::PluginSpec;
use serde_json::json;

/// 服务键 = 名字 + 精确类型。声明在使用方所在地，提供方与消费方都引用它。
static HOST_VERSION: ServiceKey<Arc<String>> = ServiceKey::new("host-version");
/// 进程内插件挂载时注册的服务。
static COUNTER: ServiceKey<Arc<AtomicUsize>> = ServiceKey::new("counter");

/// 宿主注册给插件回呼的服务（名字是**宿主词汇**，插件只知道这个字符串）。
const IDENTITY_SERVICE: &str = "demo.identity";

/// 宿主服务：插件问"你是谁"。
///
/// 实现 [`AsyncHostService`] 而不是同步版：宿主能力通常是 async 的，异步版
/// 少一次线程切换。同步实现（[`echo_plugin_host::host_service::HostService`]）
/// 仍然支持——框架会把它挪到阻塞线程池，不会占住 tokio worker。
struct Identity;

#[async_trait]
impl AsyncHostService for Identity {
    async fn call(
        &self,
        cx: CallContext,
        method: &str,
        _payload: serde_json::Value,
    ) -> Result<serde_json::Value, (String, String)> {
        if method != "name" {
            return Err(("method_not_found".into(), format!("未知方法 {method}")));
        }
        // `cx.caller` 由 supervisor 从连接归属填 —— 插件无法伪造。
        // 真实宿主在这里做来源授权（"这个插件能不能调这个方法"）。
        Ok(json!({
            "host": "minimal-host",
            "caller": cx.caller,
        }))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ── 1. 服务定位器：宿主自己决定有哪些服务 ────────────────────────────────
    let ctx = Arc::new(Ctx::default());
    let _keep = ctx.provide(
        &HOST_VERSION,
        Arc::new(env!("CARGO_PKG_VERSION").to_string()),
    )?;
    println!("[1] Ctx 就绪：keys = {:?}", ctx.keys());
    println!(
        "    require(host-version) = {}",
        ctx.require(&HOST_VERSION)?.as_str()
    );

    // ── 2. 进程内插件：manifest + 挂载闭包，注册是副作用（可撤销）─────────────
    let counter = Arc::new(AtomicUsize::new(0));
    let disposals = Arc::new(AtomicUsize::new(0));

    let manifest = PluginManifest::builtin(
        "demo.counter",
        "计数器",
        "1.0.0",
        "counter", // 开放标签：任何字符串都行，框架只透传不解释
        "counter",
        "挂载时把计数器注册进 Ctx",
    );
    let registration_counter = Arc::clone(&counter);
    let registration_disposals = Arc::clone(&disposals);
    let mount_fn = move |mctx: &MountContext| -> PluginMountResult {
        let Some(services) = &mctx.ctx else {
            return Ok(vec![]); // 宿主没给 Ctx：什么也不做
        };
        let disposer = services
            .provide(&COUNTER, Arc::clone(&registration_counter))
            .map_err(|e| echo_plugin::PluginMountError::Mount(e.to_string()))?;
        let count = Arc::clone(&registration_disposals);
        // 卸载时撤销注册，并记一笔——证明「注册即副作用」不是口号。
        Ok(vec![
            disposer,
            Disposer::from_fn(move || {
                count.fetch_add(1, Ordering::SeqCst);
            }),
        ])
    };

    let registry = PluginRegistry::default();
    registry.register(Arc::new(BuiltinPlugin::new(manifest, mount_fn)))?;
    let mount_errors = registry.mount_all(&MountContext::new().with_ctx(Arc::clone(&ctx)));
    println!(
        "[2] 进程内插件已挂载（错误 {mount_errors:?}），Ctx keys = {:?}",
        ctx.keys()
    );
    ctx.require(&COUNTER)?.fetch_add(41, Ordering::SeqCst);

    // ── 3. 进程外插件：supervisor 负责握手 / 贡献注册 / 监督 / 崩溃重启 ────────
    let supervisor = PluginSupervisor::new();
    // 宿主服务必须在 start() 之前注册：插件在 Ready 之后随时可能回呼。
    supervisor
        .host_services()
        .register_async(IDENTITY_SERVICE, Arc::new(Identity));
    // 与宿主同目录的插件二进制（cargo 会先把它构建出来）。
    let program = std::env::current_exe()?
        .parent()
        .map(|dir| dir.join("minimal-plugin"))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| "minimal-plugin".into());
    let handle = supervisor
        .start(
            PluginSpec {
                plugin_id: "demo.greeter".to_string(),
                config: json!({ "note": "Hello.config 原样透传给插件" }),
            },
            Box::new(StdioTransport::new(program)),
        )
        .await?;
    let contributions: Vec<Contribution> = handle.contributions();
    println!(
        "[3] 进程外插件就绪：state={:?}，贡献 {:?}",
        handle.state(),
        contributions
            .iter()
            .map(|c| match c {
                Contribution::Service(s) => format!("service:{}", s.key),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>()
    );

    // ── 4. 调用：框架只负责把请求送过去、把结果带回来 ────────────────────────
    let outcome = handle
        .invoke(
            "greeter",
            InvokeContext::default(),
            json!({ "name": "echo-fabric-arch" }),
        )
        .await;
    println!("[4] Invoke(greeter) → {outcome:?}");
    // 这个示例同时是**可执行的证伪测试**：断言失败即非零退出，CI 直接红。
    let result = match &outcome {
        echo_plugin_host::api::InvokeOutcome::Ok { result, .. } => result,
        other => panic!("进程外插件调用失败：{other:?}"),
    };
    assert!(
        result["greeting"]
            .as_str()
            .unwrap_or_default()
            .contains("hello, echo-fabric-arch"),
        "问候语不符合预期：{result}"
    );
    // ← 双向通道的证据：插件在 invoke 里回呼了宿主的 `demo.identity`，
    //   结果里带着宿主填的身份，以及**宿主看到的调用者 id**。
    assert_eq!(result["host"], "minimal-host", "插件应拿到宿主服务的结果");
    assert_eq!(
        result["host_caller"], "demo.greeter",
        "宿主服务应看到真实的调用者 id（supervisor 填写，插件无法伪造）：{result}"
    );

    // ── 5. 收尾：卸载（撤销注册）+ 排空（插件优雅退出）────────────────────────
    registry.unmount("demo.counter")?;
    println!(
        "[6] 已卸载进程内插件：Ctx keys = {:?}，撤销次数 = {}，卸载前计数 = {}",
        ctx.keys(),
        disposals.load(Ordering::SeqCst),
        counter.load(Ordering::SeqCst),
    );
    assert_eq!(
        disposals.load(Ordering::SeqCst),
        1,
        "卸载必须执行 mount 返回的 disposer（注册是副作用，必须可撤销）"
    );
    assert!(
        !ctx.keys().contains(&"counter".to_string()),
        "卸载后服务键应消失，实际：{:?}",
        ctx.keys()
    );
    handle.drain(Duration::from_secs(5)).await?;
    println!("[6] 进程外插件已排空退出（state={:?}）", handle.state());
    println!("\n✅ 全部断言通过——这套框架可以在非 EchoAgent 的宿主里独立使用");

    Ok(())
}
