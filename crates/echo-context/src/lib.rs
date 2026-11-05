//! EchoAgentCore service context — the Rust analogue of dsh's `ctx`.
//!
//! Three mechanisms:
//!
//! - [`Ctx`] — a service locator keyed by name. Services are registered as
//!   `Arc<dyn Trait>` (or any `Clone + 'static` value) and resolved by key;
//!   registration returns a [`Disposer`] that unwinds it, so every
//!   contribution is a reversible effect (dsh `ctx.effect()`). On top of the
//!   string-keyed API, the strongly-typed [`ServiceKey`] API
//!   ([`Ctx::provide`], [`Ctx::service`], [`Ctx::require`],
//!   [`Ctx::service_or_wait`]) pairs each name with the exact stored type and
//!   lets consumers wait for a service to become ready.
//! - [`EventBus`] — typed events with dispatch modes: [`DispatchMode::Observe`]
//!   (fan-out), [`DispatchMode::Waterfall`] (around-middleware with `next()`
//!   delegation), [`DispatchMode::Parallel`] (all listeners concurrently),
//!   [`DispatchMode::Serial`] (in order; currently identical to `Observe` — no short-circuit, no production emitter yet). Events are
//!   merge-extensible via the [`Event`] trait (the Rust analogue of dsh's
//!   declaration merging): any new event type is a new `Event` implementor.
//! - [`ScopedRegistry`] — name-keyed registrations with per-scope shadowing:
//!   a scoped entry replaces its same-named global twin for that scope alone.
//! - **父子作用域**：[`Ctx::child`] 造一个父链子作用域（子遮蔽父、父对其可见），
//!   变更通知 [`Ctx::on_change`] 沿父链向上传播，等待 [`Ctx::wait_for_keys`]
//!   在整个家族内共享唤醒源——这是模块树（父模块宿主化子模块）的底座。
//!
//! # 全局态不在此（v0.3.0）
//!
//! 曾有一个 `kernel` 模块——一张 `TypeId` 键控的进程级单例表。它是宿主应用的
//! 机制（使用者全在应用层，本 crate 及其兄弟 crate 一处不用），留在框架里会
//! 破坏"同一进程能起两套宿主"这个复用前提。现已搬到宿主侧（EchoAgentCore 的
//! `echo-bootstrap`）。
//!
//! 要表达"这个宿主里只有一份服务"，请用 [`Ctx`]：它是显式注入的，可以有任意
//! 多份，互不干扰。

mod ctx;
mod event;
mod scope;
mod service_key;

pub use ctx::{Change, Ctx, Disposer, ServiceError};
pub use event::{DispatchMode, Event, EventBus, WaterfallDecision};
pub use scope::ScopedRegistry;
pub use service_key::ServiceKey;
