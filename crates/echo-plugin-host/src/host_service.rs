//! 宿主服务注册表：插件经 `HostCall` 回呼宿主内注册的服务。
//!
//! 宿主侧实现 [`AsyncHostService`]（推荐）或 [`HostService`]（同步，兼容旧代码），
//! 以名字注册进 [`HostServiceRegistry`]；插件发送
//! `PluginToHost::HostCall { service, method, payload }` 后，supervisor 在后台任务
//! 中查表调用，并把结果（或错误）经 `HostToPlugin::HostCallResult` 回发。
//!
//! # 调用者身份从哪来
//!
//! [`CallContext::caller`] 由 **supervisor 从连接归属推导**（它一定知道自己在
//! 为哪个 `plugin_id` 跑 serve 循环），**线上协议里没有这个字段** —— 因此插件
//! 无法把自己的调用伪装成别的插件。宿主服务可以据此做来源授权与审计：
//!
//! ```no_run
//! # use echo_plugin_host::host_service::{AsyncHostService, CallContext, HostCallError};
//! # use serde_json::Value;
//! struct AdminOnly;
//!
//! # #[async_trait::async_trait]
//! # impl AsyncHostService for AdminOnly {
//! async fn call(
//!     &self,
//!     cx: CallContext,
//!     method: &str,
//!     _payload: Value,
//! ) -> Result<Value, HostCallError> {
//!     if cx.caller != "trusted.panel" {
//!         return Err(("forbidden".into(), format!("{} 无权重启服务", cx.caller)));
//!     }
//!     Ok(serde_json::json!({ "method": method }))
//! }
//! # }
//! ```
//!
//! # 同步与异步
//!
//! 两种实现可以混用，注册表在调用时抹平差异：
//!
//! | 实现 | 执行方式 | 什么时候用 |
//! |---|---|---|
//! | [`AsyncHostService`] | 直接在调用任务里 await | 绝大多数情况（宿主能力通常是 async 的） |
//! | [`HostService`] | 挪到 `spawn_blocking` 线程池 | 遗留同步实现，或明确要跑阻塞 IO |
//!
//! 同步实现被挪出 async worker 是有意的：一个做阻塞 IO 的同步服务若内联执行，
//! 会把 tokio 的 worker 线程占住，连累同一 worker 上所有插件的调用。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use serde_json::Value;

/// 一次宿主回呼的来源信息。
///
/// 由 supervisor 填写，插件无法伪造（协议里没有对应字段）。
///
/// 持有 `String` 而不是 `&str`：带生命周期的参数会让 `#[async_trait]` 的方法
/// 签名变复杂（每个实现都要写对 `'_`），而调用方本来就有一份 `String` 要交出
/// 去。相对每次 HostCall 已有的 JSON 编解码，这一次克隆可以忽略。
#[derive(Debug, Clone)]
pub struct CallContext {
    /// 发起本次回呼的插件 id。
    pub caller: String,
}

/// 宿主服务调用的错误：`(机器可读错误码, 人类可读消息)`。
pub type HostCallError = (String, String);

/// 一个可被插件回呼的宿主服务（**异步**，推荐实现这个）。
#[async_trait::async_trait]
pub trait AsyncHostService: Send + Sync {
    /// 方法调用：Ok 返回 JSON 结果；Err 携带 `(code, message)`。
    ///
    /// 服务内的未知方法应返回 `("method_not_found", …)`（注册表不代查方法名）。
    async fn call(
        &self,
        cx: CallContext,
        method: &str,
        payload: Value,
    ) -> Result<Value, HostCallError>;
}

/// 一个可被插件回呼的宿主服务（**同步**，兼容旧实现）。
///
/// 调用时会被挪到 `spawn_blocking` 线程池执行，因此阻塞 IO 是安全的；
/// 但新代码优先用 [`AsyncHostService`]——少一次线程切换，且能直接 await
/// 宿主的 async 能力（存储、网络、其他服务）。
pub trait HostService: Send + Sync {
    /// 方法调用：Ok 返回 JSON 结果；Err = (机器可读错误码, 人类可读消息)。
    ///
    /// 服务内的未知方法应返回 `("method_not_found", …)`（注册表不代查方法名）。
    ///
    /// 签名里没有调用者信息——需要它的实现请改用 [`AsyncHostService`]。
    fn call(&self, method: &str, payload: Value) -> Result<Value, HostCallError>;
}

/// 注册表里的两种实现。
enum Slot {
    Async(Arc<dyn AsyncHostService>),
    /// 同步实现：调用时挪到 `spawn_blocking`。
    Sync(Arc<dyn HostService>),
}

impl Clone for Slot {
    fn clone(&self) -> Self {
        match self {
            Self::Async(s) => Self::Async(Arc::clone(s)),
            Self::Sync(s) => Self::Sync(Arc::clone(s)),
        }
    }
}

/// 宿主服务注册表（按名字登记；`Arc` 共享，可 Clone 语义）。
#[derive(Default)]
pub struct HostServiceRegistry {
    services: RwLock<HashMap<String, Slot>>,
}

impl HostServiceRegistry {
    /// 空注册表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册（或覆盖）一个**异步**服务。
    pub fn register_async(&self, name: &str, service: Arc<dyn AsyncHostService>) {
        self.insert(name, Slot::Async(service));
    }

    /// 注册（或覆盖）一个**同步**服务。
    pub fn register(&self, name: &str, service: Arc<dyn HostService>) {
        self.insert(name, Slot::Sync(service));
    }

    fn insert(&self, name: &str, slot: Slot) {
        let replaced = self
            .services
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(name.to_string(), slot)
            .is_some();
        if replaced {
            tracing::warn!(
                service = name,
                "host service re-registered; previous implementation replaced"
            );
        } else {
            tracing::debug!(service = name, "host service registered");
        }
    }

    /// 已注册的服务名（排序后返回，便于快照/面板稳定输出）。
    pub fn names(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .services
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect();
        out.sort();
        out
    }

    /// 调用一个服务方法。
    ///
    /// `caller` 是发起回呼的插件 id（supervisor 填写；直接调用时传 `""`）。
    ///
    /// 未知服务 → `Err(("service_not_found", …))`；服务内未知方法由各实现
    /// 自行返回 `method_not_found`。
    pub async fn call(
        &self,
        caller: &str,
        service: &str,
        method: &str,
        payload: Value,
    ) -> Result<Value, HostCallError> {
        let slot = self
            .services
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(service)
            .cloned();
        let Some(slot) = slot else {
            return Err((
                "service_not_found".to_string(),
                format!("unknown host service: {service}"),
            ));
        };
        match slot {
            Slot::Async(service) => {
                let cx = CallContext {
                    caller: caller.to_string(),
                };
                service.call(cx, method, payload).await
            }
            Slot::Sync(service) => {
                let method = method.to_string();
                // 没运行时就内联执行：注册表在纯同步上下文里也要可用（测试、
                // 嵌入式宿主）。有运行时则挪到阻塞池，别占 worker。
                if tokio::runtime::Handle::try_current().is_err() {
                    return service.call(&method, payload);
                }
                match tokio::task::spawn_blocking(move || service.call(&method, payload)).await {
                    Ok(result) => result,
                    Err(e) => Err((
                        "host_service_panicked".to_string(),
                        format!("宿主服务的同步实现在阻塞池里 panic: {e}"),
                    )),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Echo;

    #[async_trait::async_trait]
    impl AsyncHostService for Echo {
        async fn call(
            &self,
            cx: CallContext,
            method: &str,
            payload: Value,
        ) -> Result<Value, HostCallError> {
            if method != "echo" {
                return Err(("method_not_found".into(), format!("未知方法 {method}")));
            }
            Ok(json!({ "caller": cx.caller, "payload": payload }))
        }
    }

    struct Legacy;

    impl HostService for Legacy {
        fn call(&self, method: &str, payload: Value) -> Result<Value, HostCallError> {
            if method != "ping" {
                return Err(("method_not_found".into(), format!("未知方法 {method}")));
            }
            Ok(json!({ "legacy": true, "payload": payload }))
        }
    }

    #[tokio::test]
    async fn async_service_receives_caller() {
        let reg = HostServiceRegistry::new();
        reg.register_async("echo", Arc::new(Echo));
        let out = reg
            .call("trusted.panel", "echo", "echo", json!({ "x": 1 }))
            .await
            .expect("调用应成功");
        assert_eq!(out["caller"], "trusted.panel", "caller 必须原样传进去");
        assert_eq!(out["payload"]["x"], 1);
    }

    #[tokio::test]
    async fn sync_service_still_works_through_blocking_pool() {
        let reg = HostServiceRegistry::new();
        reg.register("legacy", Arc::new(Legacy));
        let out = reg
            .call("any.plugin", "legacy", "ping", json!(7))
            .await
            .expect("同步服务应能经阻塞池调用");
        assert_eq!(out["legacy"], true);
        assert_eq!(out["payload"], 7);
    }

    #[tokio::test]
    async fn unknown_service_and_method_are_structured_errors() {
        let reg = HostServiceRegistry::new();
        reg.register_async("echo", Arc::new(Echo));

        let err = reg.call("p", "nope", "x", Value::Null).await.unwrap_err();
        assert_eq!(err.0, "service_not_found");
        assert!(err.1.contains("nope"));

        let err = reg.call("p", "echo", "nope", Value::Null).await.unwrap_err();
        assert_eq!(err.0, "method_not_found");
    }

    #[tokio::test]
    async fn names_are_sorted_and_cover_both_kinds() {
        let reg = HostServiceRegistry::new();
        reg.register_async("zeta", Arc::new(Echo));
        reg.register("alpha", Arc::new(Legacy));
        assert_eq!(reg.names(), vec!["alpha", "zeta"]);
    }

    #[test]
    fn sync_service_is_callable_without_a_runtime() {
        // 嵌入式宿主 / 纯同步测试上下文：没有 tokio 运行时也要能调。
        //
        // 注意不能用 `Runtime::block_on` 来测：那会把当前线程标成"有运行时"，
        // 于是走 spawn_blocking 分支，测不到内联兜底。这里手工轮询。
        let reg = HostServiceRegistry::new();
        reg.register("legacy", Arc::new(Legacy));
        let out = block_on_without_runtime(reg.call("p", "legacy", "ping", json!(1)));
        assert_eq!(out.expect("应能内联调用")["legacy"], true);
    }

    /// 手工 `block_on`：不进入任何 tokio 运行时上下文。
    fn block_on_without_runtime<F: std::future::Future>(fut: F) -> F::Output {
        use std::task::{Context, Poll, Waker};
        let mut fut = std::pin::pin!(fut);
        let mut cx = Context::from_waker(Waker::noop());
        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(out) => return out,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }
}
