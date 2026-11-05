//! # echo-module-sdk — 模块侧运行时
//!
//! Phase 3 试点产物（见 `document/design/decoupling-plan.md`）：让模块二进制
//! **零样板**实现 [`echo_module_api`] 协议——实现 [`ModuleHandler`] 后调用
//! [`serve_stdio`]，SDK 负责：
//!
//! - **帧读写**：4 字节小端 `u32` 长度前缀 + JSON（与宿主
//!   `echo-module-host::stdio` 同一编码）；单帧上限 [`MAX_FRAME_BYTES`]，
//!   读侧以 `read_exact` 容忍分段到达；
//! - **握手**：`Hello` → 校验协议名 / 版本（[`echo_module_api::compatible`]）→
//!   `Welcome` + `Register`（[`ModuleHandler::contributions`]）+ `Ready`；
//!   不兼容时写 `Failed` 并以错误退出（模块 main 侧转 `exit(1)`）；
//! - **调用**：每个 `Invoke` 并发处理（各 spawn 一个任务），结果经内部 mpsc
//!   交由**单写者任务**串行写出——帧不会交错；
//! - **控制**：`Cancel` → [`ModuleHandler::on_cancel`]；`Drain` →
//!   [`ModuleHandler::on_drain`] 后等待在途调用完成并退出（0）；`Dispose` /
//!   stdin EOF → 中止在途任务并退出；
//! - **回呼**（P2）：`Ready` 后经 [`ModuleHandler::on_ready`] 交付
//!   [`HostClient`]，模块可随时向宿主注册的服务发起 `HostCall`
//!   （默认 30s 超时；连接断开时在途调用以 [`HostCallError::Closed`] 结束）。
//!
//! 最小模块：
//!
//! ```no_run
//! use echo_module_sdk::{
//!     async_trait, serve_stdio, Contribution, InvokeOutcome, InvokeRequest, ModuleHandler,
//! };
//!
//! struct MyModule;
//!
//! #[async_trait]
//! impl ModuleHandler for MyModule {
//!     fn contributions(&self) -> Vec<Contribution> {
//!         vec![]
//!     }
//!     async fn invoke(&self, _req: InvokeRequest) -> InvokeOutcome {
//!         InvokeOutcome::ok("ok")
//!     }
//! }
//!
//! #[tokio::main]
//! async fn main() {
//!     if let Err(e) = serve_stdio(MyModule).await {
//!         eprintln!("module exited: {e}");
//!         std::process::exit(1);
//!     }
//! }
//! ```
//!
//! 泛型核心 [`serve_io`] / [`serve_io_with`] 供测试 / 内嵌运输复用
//! （如 `tokio::io::duplex`）。
//!
//! 约定：帧只走 stdout，模块自身的日志请走 stderr。

#![warn(missing_docs)]

mod error;
mod frame;
mod serve;
#[cfg(test)]
mod tests;

pub use async_trait::async_trait;
pub use echo_module_api;
pub use echo_module_api::{capabilities, Contribution, InvokeContext, InvokeOutcome};
pub use frame::MAX_FRAME_BYTES;
pub use serde_json;
pub use serve::{
    serve_io, serve_io_with, serve_stdio, serve_stdio_with, HostCallError, HostClient,
    DEFAULT_HOST_CALL_TIMEOUT,
};

use serde_json::Value;

/// 模块处理器：模块作者实现本 trait，[`serve_stdio`] 完成全部协议工作。
#[async_trait::async_trait]
pub trait ModuleHandler: Send + Sync + 'static {
    /// 接收宿主下发配置（`Hello.config`；`modules.toml [module.config]` 或
    /// 宿主组装的实例分配结果）。在 [`contributions`](Self::contributions)
    /// 之前调用一次；返回 `Err` = 拒绝启动（宿主收到 `Failed{invalid_config}`）。
    /// 缺省忽略配置。
    async fn configure(&self, _config: Value) -> Result<(), String> {
        Ok(())
    }

    /// 要注册的贡献集合（Hello 后经 `Register` 全量上报）。
    fn contributions(&self) -> Vec<Contribution>;

    /// 处理一次调用。
    ///
    /// SDK 已校验贡献名在 [`contributions`](Self::contributions) 之内；返回
    /// [`InvokeOutcome`] 即终结该 `call_id`（结果原样回发宿主）。
    async fn invoke(&self, req: InvokeRequest) -> InvokeOutcome;

    /// `Ready` 发出后的初始化完成通知（P2 新增；缺省空实现，老模块零改动）。
    ///
    /// SDK 在 `Welcome` + `Register` + `Ready` 入队后调用一次；模块借此保存
    /// [`HostClient`] 句柄，后续用它向宿主注册的服务发起回呼（`HostCall`）。
    /// 本方法在消息循环开始前被 await——如需在其中立即发起回呼，请自行
    /// spawn 到后台任务（直接 await 会等到读循环运行后才可能收到结果）。
    async fn on_ready(&self, _host: HostClient) {}

    /// `Cancel` 通知（尽力送达；对端可能已完成）。缺省空实现。
    async fn on_cancel(&self, _call_id: &str) {}

    /// `Drain` 排空通知（随后进程退出；`deadline_ms` 为宿主给定的排空期限）。
    /// 缺省空实现。
    async fn on_drain(&self, _deadline_ms: u64) {}
}

/// 一次调用的请求（`Invoke` 消息的模块侧视图）。
#[derive(Debug, Clone, PartialEq)]
pub struct InvokeRequest {
    /// 调用 id（回发结果时原样带回）。
    pub call_id: String,
    /// 目标贡献名（工具名）。
    pub contribution: String,
    /// 调用上下文（会话归属等）。
    pub ctx: InvokeContext,
    /// 调用参数（JSON Schema 由贡献注册时声明）。
    pub payload: Value,
}

/// 模块自报 id：`ECHO_MODULE_ID`（非空时）否则 `"stdio-module"`。
pub fn module_id() -> String {
    match std::env::var("ECHO_MODULE_ID") {
        Ok(id) if !id.trim().is_empty() => id,
        _ => "stdio-module".to_string(),
    }
}

/// 服务选项（[`serve_stdio_with`] / [`serve_io_with`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeOptions {
    /// `Welcome` 自报模块 id。
    ///
    /// **缺省空串 = 回显宿主 Hello 的 `module_id`**（实例身份由宿主掌握，
    /// 模块零配置即可）；非空 = 显式覆盖（与宿主不一致会被宿主拒绝，
    /// 该检查用于抓配置错误）。`ECHO_MODULE_ID` 环境变量提供 `module_id()`
    /// 辅助值，供需要自主身份的模块使用。
    pub module_id: String,
    /// `Welcome` 能力列表；缺省 `["tools", "cancel", "host_call"]`（双方都声明才可用）。
    pub capabilities: Vec<String>,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            // 空 = 回显宿主 Hello 的 module_id（见字段文档）。
            module_id: String::new(),
            capabilities: vec![
                capabilities::SERVICES.to_string(),
                capabilities::CANCEL.to_string(),
                capabilities::HOST_CALL.to_string(),
            ],
        }
    }
}
