//! # echo-plugin-api — 插件协议（冻结契约）
//!
//! P0 阶段产物（见 `document/design/decoupling-plan.md`）。本 crate 是宿主（kernel）
//! 与插件之间的**唯一线上契约**：同一组消息覆盖全部运输层——
//!
//! - **inproc**：类型直连（零序列化，插件与宿主同进程编译）；
//! - **subprocess**：4 字节 LE 长度前缀 + JSON（默认；编解码实现位于
//!   `echo-plugin-host` 的 stdio 运输层，Phase 2），msgpack 留 feature；
//! - dylib / wasm（实验/可选轨）：JSON 载荷。
//!
//! ## 兼容性规则（对齐 `echo-federation::FedFrame` 既有惯例）
//!
//! - adjacently-tagged enum（`{"type": …, "payload": …}`）；变体名与字段名是
//!   线上契约，**只增不改**。注：无载荷变体（`Ready` / `Dispose`）序列化为
//!   仅含 `"type"` 的单键对象（无 `payload` 键），已由契约测试固化；
//! - 新增字段一律 `#[serde(default)]`（旧端点可解码新帧、新端点可解码旧帧）；
//! - Hello/Welcome 握手时比对 [`PROTOCOL_NAME`] 与 [`PROTOCOL_VERSION`]：
//!   主版本不一致拒绝连接；
//! - [`HostToPlugin::Dispose`] 与崩溃在运输层同形（连接关闭）——宿主在主动
//!   Dispose 前必须**先停用重启**（supervisor 侧语义），否则会被误判为崩溃
//!   触发重启；当前 supervisor 以 Drain 收尾、不发送 Dispose；
//! - 协议变更必须过 conformance 测试套件（`echo-plugin-host/tests/conformance.rs`）
//!   并按需 bump `PROTOCOL_VERSION`。

pub mod contribution;
pub mod protocol;

pub use contribution::{
    CommandContribution, Contribution, EventContribution,
    InstanceCapabilities, InstanceContribution, ServiceContribution,
    SkillContribution, TextBlockContribution, ToolContribution,
};
pub use protocol::{
    compatible, Cancel, Drain, Emit, EventNotification, Failure, Hello, HostCall, HostCallOutcome,
    HostCallResult, HostToPlugin, Invoke, InvokeContext, InvokeOutcome, InvokeResult, LogLevel,
    LogRecord, PluginToHost, Register, Welcome, PROTOCOL_NAME, PROTOCOL_VERSION,
};

/// 能力 / feature 名单常量（`Welcome.capabilities` 用的稳定字符串）。
///
/// 双方都声明才可用；宿主与插件勿硬编码字面量。
pub mod capabilities {
    /// 插件提供面向模型的工具贡献。
    pub const TOOLS: &str = "tools";
    /// 插件提供技能贡献。
    pub const SKILLS: &str = "skills";
    /// 插件提供以键注册的服务贡献。
    pub const SERVICES: &str = "services";
    /// 插件订阅宿主事件（`HostToPlugin::Event` 才会送达）。
    pub const EVENTS: &str = "events";
    /// 插件支持取消语义（Cancel 送达 + `Error{code:"cancelled"}` 回发）。
    pub const CANCEL: &str = "cancel";
    /// 插件支持宿主服务回呼（`HostCall` / `HostCallResult`；P2 新增）。
    pub const HOST_CALL: &str = "host_call";
    /// 插件提供插件实例贡献（`Contribution::PluginInstance`；W1.3 / P1，2026-10
    /// 实例插件化）。wire 字符串保持 `adapter` 以兼容已部署插件（2026-10 去
    /// 适配器化时改名，握手比对的是字符串，因此不能与旧值断开）。
    pub const PLUGIN_INSTANCE: &str = "adapter";
}

#[cfg(test)]
mod tests;
