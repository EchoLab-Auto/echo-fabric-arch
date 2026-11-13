//! 模块树快照：把作用域 + 注册表投影成可序列化的只读结构。
//!
//! 每个宿主都想让面板/CLI 看到「挂了哪些模块、各自什么状态、谁缺依赖」——
//! 原料本来就在框架里（`registry().descriptors()` / `children_of()` /
//! `is_mounted()` / `ctx().keys()`），但拼装代码每个宿主都得重写一遍，而每个
//! 宿主的形状又都不一样。这个模块把拼装收进来，宿主只需**补上框架看不见的
//! 那部分**（子进程模块不在注册表里，它的状态在
//! `ModuleSupervisor`/`ModuleHandle` 手里）。
//!
//! # 用法
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use echo_module::ModuleScope;
//! # fn demo(root: &Arc<ModuleScope>) {
//! let mut tree = root.snapshot();
//! tree.host = "my-app".into();
//! tree.host_services = vec!["my.control".into()];
//! // 子进程模块由宿主自己追加（框架不知道 supervisor 的存在）
//! // tree.modules.push(my_subprocess_node());
//! let json = serde_json::to_value(&tree).expect("可序列化");
//! # }
//! ```
//!
//! # 只读
//!
//! 快照是**观测**，不是控制面：没有任何启停或挂载操作。控制面请走宿主自己的
//! 服务（`HostCall`），否则面板就绕过了宿主的授权与副作用管理。

use serde::{Deserialize, Serialize};

use crate::registry::ModuleDescriptor;
use crate::scope::ModuleScope;

/// 快照的线格式版本。
///
/// 新增字段一律 `#[serde(default)]`，因此消费方按旧版本解码新快照不会失败；
/// 只有**语义变化**（同名字段含义变了）才需要 bump 它。
pub const TREE_SCHEMA_VERSION: u32 = 1;

/// 一整棵模块树。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleTreeSnapshot {
    #[serde(default)]
    pub schema_version: u32,
    /// 宿主名（框架不知道，由宿主填）。
    #[serde(default)]
    pub host: String,
    /// 框架版本（本 crate 的 `CARGO_PKG_VERSION`）。
    #[serde(default)]
    pub framework: String,
    /// 根作用域里所有可见的服务键（含父链，按名字排序）。
    #[serde(default)]
    pub context_services: Vec<String>,
    /// 宿主注册的服务名（框架不知道，由宿主填）。
    #[serde(default)]
    pub host_services: Vec<String>,
    /// 顶层模块节点（树形，`children` 递归）。
    #[serde(default)]
    pub modules: Vec<ModuleTreeNode>,
}

/// 树上的一个模块。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleTreeNode {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    /// 开放标签（`ModuleManifest.kind`），框架不解释。
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub description: String,
    /// `in_process` / `subprocess`。
    #[serde(default)]
    pub runtime: String,
    /// 挂载开关（配置说它该不该跑）。
    pub enabled: bool,
    /// 此刻是否真的挂载着。
    pub mounted: bool,
    /// `ready` / `pending` / `failed` / `disabled` / `stopped` / `draining`。
    #[serde(default)]
    pub state: String,
    /// 挂载失败原因（`state = failed` 时有值）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 硬依赖（`Inject::required`）。
    #[serde(default)]
    pub required_services: Vec<String>,
    /// 软依赖（`Inject::optional`）。
    #[serde(default)]
    pub optional_services: Vec<String>,
    /// 此刻还缺的硬依赖（空 = 不缺）。
    #[serde(default)]
    pub pending_services: Vec<String>,
    /// 子进程启动命令（进程内模块为空）。
    #[serde(default)]
    pub command: String,
    /// 子作用域里的子模块（`MountContext::nest` 建的树）。
    #[serde(default)]
    pub children: Vec<ModuleTreeNode>,
}

impl ModuleScope {
    /// 把本作用域（含子作用域）投影成快照。
    ///
    /// 只读注册表与作用域，不猜测父子关系：子节点来自
    /// [`children_of`](Self::children_of)，服务依赖来自 descriptor 的
    /// `Inject` 声明。
    pub fn snapshot(&self) -> ModuleTreeSnapshot {
        ModuleTreeSnapshot {
            schema_version: TREE_SCHEMA_VERSION,
            host: String::new(),
            framework: env!("CARGO_PKG_VERSION").to_string(),
            context_services: self.ctx().keys(),
            host_services: Vec::new(),
            modules: nodes_of(self),
        }
    }
}

fn nodes_of(scope: &ModuleScope) -> Vec<ModuleTreeNode> {
    scope
        .registry()
        .descriptors()
        .into_iter()
        .map(|descriptor| {
            let children = scope
                .children_of(&descriptor.id)
                .iter()
                .flat_map(|child| nodes_of(child))
                .collect();
            node_of(scope, descriptor, children)
        })
        .collect()
}

fn node_of(
    scope: &ModuleScope,
    descriptor: ModuleDescriptor,
    children: Vec<ModuleTreeNode>,
) -> ModuleTreeNode {
    let mounted = scope.registry().is_mounted(&descriptor.id);
    // 判序有讲究：先看"用户关了没"，再看"是否挂载"，再看"缺依赖"（可恢复），
    // 最后才是"挂载失败"（不可恢复，见 document/lifecycle.md）。
    let state = if !descriptor.enabled {
        "disabled"
    } else if mounted {
        "ready"
    } else if !descriptor.pending_services.is_empty() {
        "pending"
    } else if descriptor.error.is_some() {
        "failed"
    } else {
        "stopped"
    };
    ModuleTreeNode {
        id: descriptor.id,
        name: descriptor.name,
        version: descriptor.version,
        kind: descriptor.kind,
        description: descriptor.description,
        runtime: descriptor.runtime,
        enabled: descriptor.enabled,
        mounted,
        state: state.to_string(),
        error: descriptor.error,
        required_services: descriptor.required_services,
        optional_services: descriptor.optional_services,
        pending_services: descriptor.pending_services,
        command: descriptor.command,
        children,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ModuleManifest, ModuleRuntime};
    use crate::module::{BuiltinModule, Inject, MountContext};
    use echo_context::Ctx;
    use serde_json::json;
    use std::sync::Arc;

    fn mount_all(root: &Arc<ModuleScope>) {
        root.registry().mount_all(
            &MountContext::new()
                .with_ctx(Arc::clone(root.ctx()))
                .with_tree(Arc::clone(root)),
        );
    }

    fn builtin(id: &str) -> BuiltinModule {
        BuiltinModule::new(
            ModuleManifest::builtin(id, id, "1.0.0", "test", ""),
            |_| Ok(Vec::new()),
        )
    }

    #[test]
    fn snapshot_reports_mounted_module_with_its_declarations() {
        let root = ModuleScope::root(Arc::new(Ctx::default()));
        let module = builtin("demo.alpha")
            .with_inject(Inject::new(["needed"]).with_optional(["nice.to.have"]));
        root.registry().register(Arc::new(module)).unwrap();
        mount_all(&root);

        let tree = root.snapshot();
        assert_eq!(tree.schema_version, TREE_SCHEMA_VERSION);
        assert_eq!(tree.framework, env!("CARGO_PKG_VERSION"));
        assert_eq!(tree.modules.len(), 1);
        let node = &tree.modules[0];
        assert_eq!(node.id, "demo.alpha");
        assert_eq!(node.state, "pending", "缺硬依赖 → pending");
        assert_eq!(node.pending_services, vec!["needed"]);
        assert_eq!(node.required_services, vec!["needed"]);
        assert_eq!(node.optional_services, vec!["nice.to.have"]);
        assert!(!node.mounted);
    }

    #[test]
    fn mounted_module_is_ready_and_context_services_are_listed() {
        let root = ModuleScope::root(Arc::new(Ctx::default()));
        root.registry()
            .register(Arc::new(builtin("demo.beta")))
            .unwrap();
        mount_all(&root);

        let tree = root.snapshot();
        assert_eq!(tree.modules[0].state, "ready");
        assert!(tree.modules[0].mounted);
        assert_eq!(tree.modules[0].runtime, ModuleRuntime::InProcess.as_str());
        assert!(tree.context_services.is_empty());
    }

    #[test]
    fn disabled_and_failed_are_distinguishable() {
        let root = ModuleScope::root(Arc::new(Ctx::default()));
        root.registry()
            .register(Arc::new(builtin("demo.gamma")))
            .unwrap();
        mount_all(&root);
        root.registry()
            .set_enabled(
                "demo.gamma",
                false,
                &MountContext::new()
                    .with_ctx(Arc::clone(root.ctx()))
                    .with_tree(Arc::clone(&root)),
            )
            .unwrap();
        let tree = root.snapshot();
        assert_eq!(tree.modules[0].state, "disabled");
        assert!(!tree.modules[0].mounted);

        // 挂载闭包报错 → failed（不进 pending，见 lifecycle.md）
        let root2 = ModuleScope::root(Arc::new(Ctx::default()));
        root2
            .registry()
            .register(Arc::new(BuiltinModule::new(
                ModuleManifest::builtin("demo.boom", "boom", "1.0.0", "test", ""),
                |_| Err(crate::module::ModuleMountError::Mount("炸了".into())),
            )))
            .unwrap();
        mount_all(&root2);
        let tree2 = root2.snapshot();
        assert_eq!(tree2.modules[0].state, "failed");
        let error = tree2.modules[0].error.as_deref().unwrap_or_default();
        assert!(
            error.contains("炸了"),
            "挂载闭包的错误必须出现在快照里（否则面板只能说\"没挂载\"）：{error}"
        );
    }

    #[test]
    fn nested_scope_becomes_children() {
        let root = ModuleScope::root(Arc::new(Ctx::default()));
        // 父模块在 mount 里建子作用域并挂一个子模块。
        let parent = BuiltinModule::new(
            ModuleManifest::builtin("demo.parent", "父", "1.0.0", "test", ""),
            |mctx: &MountContext| {
                let child = mctx.nest().expect("有树把手");
                child
                    .registry()
                    .register(Arc::new(builtin("demo.child")))
                    .expect("注册子模块");
                child.registry().mount_all(
                    &MountContext::new()
                        .with_ctx(Arc::clone(child.ctx()))
                        .with_tree(Arc::clone(&child)),
                );
                Ok(Vec::new())
            },
        );
        root.registry().register(Arc::new(parent)).unwrap();
        mount_all(&root);

        let tree = root.snapshot();
        assert_eq!(tree.modules.len(), 1, "顶层只有父模块");
        let parent_node = &tree.modules[0];
        assert_eq!(parent_node.id, "demo.parent");
        assert_eq!(parent_node.children.len(), 1, "子作用域应成为 children");
        assert_eq!(parent_node.children[0].id, "demo.child");
        assert_eq!(parent_node.children[0].state, "ready");
    }

    #[test]
    fn snapshot_roundtrips_through_json_and_tolerates_extra_fields() {
        let root = ModuleScope::root(Arc::new(Ctx::default()));
        root.registry()
            .register(Arc::new(builtin("demo.delta")))
            .unwrap();
        mount_all(&root);

        let mut tree = root.snapshot();
        tree.host = "my-app".into();
        tree.host_services = vec!["my.control".into()];
        let json = serde_json::to_value(&tree).expect("可序列化");

        // 宿主追加自己的字段是允许的：消费方按已知字段解码即可。
        let mut with_extra = json.clone();
        with_extra["my_vendor_field"] = json!({ "anything": 1 });
        let decoded: ModuleTreeSnapshot =
            serde_json::from_value(with_extra).expect("应忽略未知字段");
        assert_eq!(decoded, tree);

        // 缺字段也用 default 兜住（跨版本解码）。
        let minimal = json!({ "modules": [{ "id": "x", "enabled": true, "mounted": true }] });
        let decoded: ModuleTreeSnapshot = serde_json::from_value(minimal).expect("缺字段应可解码");
        assert_eq!(decoded.modules[0].id, "x");
        assert_eq!(decoded.schema_version, 0, "缺省取 default");
    }

    #[test]
    fn registry_helper_and_scope_snapshot_agree_on_mount_state() {
        // 这条防的是"快照说 ready 但注册表说没挂"这类漂移。
        let root = ModuleScope::root(Arc::new(Ctx::default()));
        root.registry()
            .register(Arc::new(builtin("demo.epsilon")))
            .unwrap();
        mount_all(&root);
        let tree = root.snapshot();
        assert_eq!(
            tree.modules[0].mounted,
            root.registry().is_mounted("demo.epsilon")
        );

        // 卸载后快照与注册表必须同时翻面。
        root.registry().unmount("demo.epsilon").unwrap();
        let tree = root.snapshot();
        assert!(!tree.modules[0].mounted);
        assert_eq!(tree.modules[0].state, "stopped");
        assert!(!root.registry().is_mounted("demo.epsilon"));
    }
}
