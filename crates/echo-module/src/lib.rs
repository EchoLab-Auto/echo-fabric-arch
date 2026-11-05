//! Module system for EchoAgentCore — the "everything is a module" seam.
//!
//! 本 crate 是**框架层**的一部分：只认识 manifest / 生命周期 / 注册表，
//! 不认识任何应用领域词汇（见 `document/design/echo-fabric-arch-extraction.md`
//! 的边界判据）。领域适配在宿主侧（`echo-module-adapter`），因此依赖方向
//! 始终单向：`echo-module` 不依赖 `echo-defs`，也不依赖业务内核。
//!
//! Design (see document/modules/core-modules.md):
//! - A module is a manifest + lifecycle hooks; mounting is a reversible side
//!   effect (returns disposers), matching dsh's "registrations are effects".
//! - Modules are **source-level** (compiled into the binary). Hot-reload
//!   applies to data modules (skills/tools/manifests); code modules reload
//!   via atomic binary replacement + process restart (the self-update path).

pub mod event;
pub mod manifest;
pub mod module;
pub mod registry;
mod runtime_state;
pub mod scope;
pub mod tree;

pub use event::{EventSink, ModuleEvent, ALL_EVENTS};
pub use manifest::{ModuleError, ModuleManifest, ModuleRuntime};
pub use module::{
    BuiltinModule, CommandFuture, Inject, Module, ModuleMountError, ModuleMountResult,
    ModuleTextBlock, MountContext, ServiceSlot,
};
pub use registry::{ModuleDefinition, ModuleDescriptor, ModuleRegistry, ReconcileReport};
pub use scope::ModuleScope;
pub use tree::{ModuleTreeNode, ModuleTreeSnapshot, TREE_SCHEMA_VERSION};
