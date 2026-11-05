//! 运输层抽象：宿主经 [`Transport`] 启动模块实例，经 [`ModuleConnection`]
//! 交换协议消息（[`HostToModule`] / [`ModuleToHost`]）。
//!
//! 已实现：
//! - [`crate::inproc::InprocTransport`]：类型直连（零序列化）；
//! - [`crate::stdio::StdioTransport`]：4 字节小端长度前缀 + JSON（手工帧读写，见模块文档）。
//!
//! 计划中：`DylibTransport`（实验轨）；`WasmTransport`（可选沙箱轨）。

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::api::{HostToModule, ModuleToHost};

/// 启动一个模块实例所需的规格（对应配置行：id + config 原样透传）。
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleSpec {
    /// 宿主侧的模块实例 id（Hello/Welcome 握手比对）。
    pub module_id: String,
    /// 模块配置（Hello.config 原样透传）。
    pub config: Value,
}

/// 运输层 / 连接错误。
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    /// 对端已关闭：连接断开（崩溃 / 退出）或已 shutdown。
    #[error("connection closed")]
    Closed,
    /// 操作超时（握手 / 关停期限等）。
    #[error("operation timed out")]
    Timeout,
    /// 协议层错误（握手不兼容 / 非法消息等）。
    #[error("protocol error: {0}")]
    Protocol(String),
    /// IO / 传输层错误（spawn 失败 / 管道损坏等）。
    #[error("io error: {0}")]
    Io(String),
}

/// 运输层：把 [`ModuleSpec`] 变成一条宿主侧连接。
#[async_trait]
pub trait Transport: Send + Sync {
    /// 启动模块实例；失败即表示模块不可用（由调用方决定重试 / 熔断）。
    async fn start(&self, spec: ModuleSpec) -> Result<Box<dyn ModuleConnection>, TransportError>;
}

/// 宿主侧模块连接：双向协议消息 + 关停。
///
/// 崩溃（对端退出）的观察信号：对端消失后
/// [`recv`](ModuleConnection::recv) 返回 [`TransportError::Closed`]、
/// [`send`](ModuleConnection::send) 同样返回 [`TransportError::Closed`]。
#[async_trait]
pub trait ModuleConnection: Send + Sync {
    /// 发送消息；对端关闭后返回 [`TransportError::Closed`]。
    async fn send(&self, msg: HostToModule) -> Result<(), TransportError>;

    /// 接收消息；对端关闭（崩溃 / 退出）时返回 [`TransportError::Closed`]。
    async fn recv(&self) -> Result<ModuleToHost, TransportError>;

    /// 优雅关停：关闭宿主 → 模块方向并等待模块退出；deadline 内未退出返回
    /// [`TransportError::Timeout`]（实现可强杀，见各实现文档）。
    async fn shutdown(&self, deadline: Duration) -> Result<(), TransportError>;
}
