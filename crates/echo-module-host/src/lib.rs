//! # echo-module-host — 模块宿主
//!
//! 实现总览见 `document/design/decoupling-plan.md`（Phase 0 / Phase 2）。宿主提供：
//! - [`transport`]：运输层抽象（[`transport::Transport`] / [`transport::ModuleConnection`]）；
//! - [`inproc`]：in-proc 参考实现 [`inproc::InprocTransport`]（类型直连，零序列化）；
//! - [`stdio`]：子进程实现 [`stdio::StdioTransport`]（4 字节小端长度前缀 + JSON）；
//! - [`supervisor`]：模块生命周期监督 [`supervisor::ModuleSupervisor`]
//!   （握手 / 贡献注册 / 调用 / 取消 / 排空 / 崩溃重启）；
//!
//! **领域适配不在本 crate**（把协议里的贡献翻译成宿主自己的 trait 对象）——
//! 见 `echo-module-adapter`：宿主只认协议（`echo-module-api`），因此可作为一个
//! 领域中立的 Rust 模块机制被整体复用。
//! - [`bootstrap`]：配置驱动启动封装（[`bootstrap::launch_specs`] /
//!   [`bootstrap::start_modules`] / [`bootstrap::start_from_file`]）。
//!
//! 协议契约（冻结）见 [`echo_module_api`]；一致性用例见 `tests/conformance.rs`
//! （inproc）与 `tests/conformance_stdio.rs`（stdio 子进程）。

pub use echo_module_api as api;

pub mod bootstrap;
pub mod host_service;
pub mod inproc;
pub mod stdio;
pub mod supervisor;
pub mod transport;

/// 实现 [`AsyncHostService`] 需要 `#[async_trait]` 宏；这里re-export，
/// 让宿主不必为此单独加一次依赖（与 `echo-module-sdk` 的做法一致）。
pub use async_trait::async_trait;
pub use bootstrap::{launch_specs, start_from_file, start_modules, LaunchSpec, StartedModules};
pub use host_service::{AsyncHostService, CallContext, HostService, HostServiceRegistry};
pub use inproc::{InprocIo, InprocModule, InprocTransport};
pub use stdio::{StdioConnection, StdioTransport};
pub use supervisor::{ModuleHandle, ModuleState, ModuleSupervisor, RestartPolicy};
pub use transport::{ModuleConnection, ModuleSpec, Transport, TransportError};
