//! 最小插件（standalone binary，走 stdio 协议）。
//!
//! 这个 crate **只依赖 `echo-plugin-sdk`**——不依赖任何宿主应用的东西。它证明
//! 了「写一个插件」不需要理解宿主是什么：实现 `PluginHandler`，交给
//! `serve_stdio`，剩下的（帧、握手、并发、单写者、生命周期）由 SDK 承担。
//!
//! 贡献用 `Contribution::Service`（`key` + `kind` + `version`）而不是
//! `Tool` / `Skill`——后者是宿主应用的领域分类，前者是框架的通用机制。
//! 宿主怎么解释这个 key，是宿主的事。
//!
//! # 它也演示了反方向的调用
//!
//! 插件不只是被动被调用，它可以**回呼宿主**（`HostCall`）：`on_ready` 拿到
//! `HostClient` 后随时能调宿主注册的服务。下面的 `invoke` 每次都会问一次
//! 宿主"你叫什么"，把答案拼进问候语——这样 `minimal-host` 的输出能直观
//! 证明双向通道是通的。
//!
//! 运行（由宿主 spawn，不手工跑）：
//! ```text
//! cargo run -p minimal-host
//! ```

use std::sync::Mutex;

use echo_plugin_sdk::echo_plugin_api::ServiceContribution;
use echo_plugin_sdk::{
    async_trait, serve_stdio, Contribution, HostClient, InvokeOutcome, InvokeRequest,
    PluginHandler,
};
use serde_json::{json, Value};

/// 服务键：宿主按这个名字调用（`Invoke("greeter")`）。
const SERVICE_KEY: &str = "greeter";

/// 宿主侧注册的服务名（`minimal-host` 注册了它）。名字是**宿主词汇**，
/// 框架只负责透传——插件与宿主之间没有共享的 Rust 类型。
const HOST_SERVICE: &str = "demo.identity";

#[derive(Default)]
struct Greeter {
    /// `on_ready` 才拿得到回呼句柄，所以用 `Mutex<Option<..>>` 存起来。
    ///
    /// 不在 `on_ready` 里直接 await 回呼：那时读循环可能还没起来，会自锁。
    host: Mutex<Option<HostClient>>,
}

#[async_trait]
impl PluginHandler for Greeter {
    fn contributions(&self) -> Vec<Contribution> {
        vec![Contribution::Service(ServiceContribution {
            key: SERVICE_KEY.to_string(),
            kind: "greeter".to_string(),
            version: 1,
        })]
    }

    async fn invoke(&self, req: InvokeRequest) -> InvokeOutcome {
        // SDK 已校验 `req.contribution` 在 contributions 之内，这里只需处理业务。
        match req.contribution.as_str() {
            SERVICE_KEY => {
                let name = req
                    .payload
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("world");

                // 反方向调用：问宿主一个问题。失败不该拖垮本次调用——宿主服务
                // 可能没注册（最小宿主 vs 真实宿主），这时降级成不含宿主的问候。
                let asked = {
                    let host = self.host.lock().expect("host 句柄锁").clone();
                    match host {
                        Some(host) => host.call(HOST_SERVICE, "name", Value::Null).await,
                        None => Err(echo_plugin_sdk::HostCallError::Closed),
                    }
                };
                // 宿主的回答原样带回给调用方，方便宿主自己看到"我看我自己的
                // 视角"——包括它填的 caller（证明身份是宿主侧决定的）。
                let (who, caller) = match asked {
                    Ok(v) => (
                        v.get("host").and_then(Value::as_str).unwrap_or("unknown").to_string(),
                        v.get("caller").and_then(Value::as_str).unwrap_or("unknown").to_string(),
                    ),
                    Err(e) => (format!("（宿主不可达: {e}）"), String::new()),
                };

                // `data` 而不是 `ok`：结果本来就是结构化的，没必要先编成字符串。
                InvokeOutcome::data(json!({
                    "greeting": format!("hello, {name}"),
                    "host": who,
                    "host_caller": caller,
                }))
            }
            other => InvokeOutcome::Error {
                code: "unknown_contribution".to_string(),
                message: format!("unknown contribution: {other}"),
            },
        }
    }

    async fn on_ready(&self, host: HostClient) {
        // 只保存句柄：在这里直接 await 回呼会等读循环启动，可能自锁。
        *self.host.lock().expect("host 句柄锁") = Some(host);
    }
}

#[tokio::main]
async fn main() {
    // 日志必须走 stderr——stdout 是协议通道，混入任何字节都会破坏帧。
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    if let Err(error) = serve_stdio(Greeter::default()).await {
        eprintln!("minimal-plugin exited: {error}");
        std::process::exit(1);
    }
}
