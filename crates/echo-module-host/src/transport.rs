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
/// 宿主侧模块连接：双向协议消息 + 关停。
///
/// 崩溃（对端退出）的观察信号：对端消失后
/// [`recv`](ModuleConnection::recv) 返回 [`TransportError::Closed`]、
/// [`send`](ModuleConnection::send) 同样返回 [`TransportError::Closed`]。
///
/// **取消安全契约**：`recv` / `send` 的 future 可能被 `select!` 或调用方
/// 取消。丢弃 future 不得丢失已读字节、不得让后续帧交错或错位——
/// 实现须把半帧进度保存在连接内，下一次调用从断点继续（stdio 实现用
/// 连接内读缓冲 / 写偏移；inproc 的通道收发本身取消安全）。
#[async_trait]
pub trait ModuleConnection: Send + Sync {
    /// 发送消息；对端关闭后返回 [`TransportError::Closed`]。
    ///
    /// 取消语义：future 被丢弃时，本帧可能只写出一部分；实现须保证后续
    /// `send` 先补完它再写新帧（不会交错）。因此取消不等于「未发送」。
    ///
    /// **写坏即封死**：若某帧写到一半就失败（对端消失 / 管道破裂），管道里
    /// 留着「帧前缀 + 半截帧体」。实现须把写方向标记为不可用，之后所有
    /// `send` 直接 `Closed`——**绝不**把新帧追加到残帧后面，否则对端会把两帧
    /// 连起来解析（误执行）。
    ///
    /// 注意 `send` 可能**长时间阻塞**（对端不读 → 管道/通道写满），所以调用
    /// 方凡是承诺有界的地方都必须自己给它预算。
    async fn send(&self, msg: HostToModule) -> Result<(), TransportError>;

    /// 接收消息；对端关闭（崩溃 / 退出）时返回 [`TransportError::Closed`]。
    ///
    /// 取消语义：future 被丢弃时已读字节留在连接内，下一次 `recv` 从断点
    /// 续读，流不会损坏。
    async fn recv(&self) -> Result<ModuleToHost, TransportError>;
    /// 关停连接并释放资源（子进程 / 任务 / 管道）。
    ///
    /// `deadline` 是**整个关停流程**的预算（不是每一步各一份）。
    /// **实现必须整体有界**：不得因为「对端不读、写方向被一个卡住的 `send`
    /// 占着」而无限等待。拿不到写方向、或写方向留有半帧时，实现应**强制
    /// 关闭**（stdio：`start_kill()` 并封死写方向；inproc：`abort()` 任务），
    /// 并返回 [`TransportError::Timeout`] 说明「没能优雅关停，已强杀」。
    ///
    /// 「超时就丢掉 future」是不合格的实现：那只让调用方早退，进程 / 任务
    /// 与写方向状态都还留着，下一次关停照样挂。
    async fn shutdown(&self, deadline: Duration) -> Result<(), TransportError>;
}
