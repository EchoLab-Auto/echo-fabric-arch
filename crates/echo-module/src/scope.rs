//! 模块树：作用域（`Ctx` 父子）+ 归属（谁挂的子模块）+ 级联卸载。
//!
//! 一个 [`ModuleScope`] = **一层**：一个 [`Ctx`] 作用域 + 一个
//! [`ModuleRegistry`]。根作用域的 ctx 就是宿主的服务表；子作用域的 ctx 是
//! 它的 [`Ctx::child`]（父链继承、子遮蔽父）。
//!
//! ## 与 dsh 的对应
//!
//! dsh 里 `ctx.module(Foo)` 把 Foo 挂成**当前上下文的子模块**：Foo 自己的
//! `ctx` 继承父的服务、但它注册的东西不污染父；父被 dispose 时子级联退出。
//! 本模块给的是同一套结构的最小型：
//!
//! | dsh | 这里 |
//! |---|---|
//! | `ctx.module(Foo)` | `scope.mount(Foo)`（或模块在 mount 里 `mctx.nest()`） |
//! | 子 ctx 继承父 | `Ctx::child`（父链查找 + 遮蔽） |
//! | 父 dispose → 子 dispose | `unmount(父)` → 该模块名下子作用域 `shutdown()` |
//! | `ctx.isolate()` | `Ctx::child` |
//!
//! ## 归属如何记录
//!
//! 注册表在 `mount(id)` 期间把 id 压进 [`TreeLinks::mounting`]，模块在
//! mount 里调用 [`MountContext::nest`] 时，新建的子作用域就记在
//! **栈顶那个 id** 名下。于是"这个子作用域属于谁"不需要模块自报，也不会
//! 因为模块挂别人的子作用域而串味。
//!
//! [`MountContext::nest`]: crate::MountContext::nest

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use echo_context::Ctx;

use crate::module::{ModuleMountError, ModuleMountResult, MountContext};
use crate::registry::ModuleRegistry;
use crate::Module;

/// 一层作用域的内部归属表（`ModuleScope` 与它的注册表共享）。
#[derive(Default)]
pub(crate) struct TreeLinks {
    /// 当前正在挂载的模块 id（栈；嵌套挂载时非空）。
    mounting: Mutex<Vec<String>>,
    /// `owner 模块 id` → 它在挂载期建出的子作用域。
    children: Mutex<BTreeMap<String, Vec<Arc<ModuleScope>>>>,
}

impl TreeLinks {
    fn enter(&self, id: &str) {
        self.mounting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(id.to_string());
    }

    fn leave(&self, id: &str) {
        let mut stack = self
            .mounting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // 只弹栈顶自己：中间被别的 mount 压进来时不该误弹。
        if stack.last().map(String::as_str) == Some(id) {
            stack.pop();
        } else if let Some(pos) = stack.iter().rposition(|entry| entry == id) {
            stack.remove(pos);
        }
    }

    pub(crate) fn current_owner(&self) -> Option<String> {
        self.mounting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .cloned()
    }

    fn adopt(&self, owner: &str, child: Arc<ModuleScope>) {
        self.children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(owner.to_string())
            .or_default()
            .push(child);
    }

    pub(crate) fn take_children(&self, owner: &str) -> Vec<Arc<ModuleScope>> {
        self.children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(owner)
            .unwrap_or_default()
    }

    pub(crate) fn children_of(&self, owner: &str) -> Vec<Arc<ModuleScope>> {
        self.children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(owner)
            .cloned()
            .unwrap_or_default()
    }

    fn total_children(&self) -> usize {
        self.children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(Vec::len)
            .sum()
    }
}

/// 一层作用域：独立 `Ctx` + 独立模块注册表 + 归属关系。
///
/// 用 [`ModuleScope::root`] 建根，用 [`ModuleScope::nest`] /
/// [`MountContext::nest`] 建子。卸载父模块会级联卸载它名下的子作用域。
///
/// [`MountContext::nest`]: crate::MountContext::nest
pub struct ModuleScope {
    ctx: Arc<Ctx>,
    registry: ModuleRegistry,
    depth: usize,
    links: Arc<TreeLinks>,
}

impl std::fmt::Debug for ModuleScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModuleScope")
            .field("depth", &self.depth)
            .field("ids", &self.registry.ids())
            .field("children", &self.links.total_children())
            .finish()
    }
}

impl ModuleScope {
    /// 建**根作用域**（`ctx` 就是宿主的服务表）。
    pub fn root(ctx: Arc<Ctx>) -> Arc<Self> {
        Self::build(ctx, 0)
    }

    fn build(ctx: Arc<Ctx>, depth: usize) -> Arc<Self> {
        Self::build_with_sink(ctx, depth, None)
    }

    fn build_with_sink(ctx: Arc<Ctx>, depth: usize, sink: Option<crate::EventSink>) -> Arc<Self> {
        let links = Arc::new(TreeLinks::default());
        let scope = Arc::new(Self {
            ctx,
            registry: ModuleRegistry::new(),
            depth,
            links,
        });
        // 注册表通过弱引用回指本作用域：卸载时据此级联卸子作用域。
        // 用 Weak 而不是 Arc——`scope → registry → scope` 成环会泄漏。
        scope.registry.attach_tree(Arc::downgrade(&scope));
        if let Some(sink) = sink {
            scope.registry.set_event_sink(sink);
        }
        scope
    }

    /// 本作用域的服务表（子作用域是父的 child，父服务对它可见）。
    pub fn ctx(&self) -> &Arc<Ctx> {
        &self.ctx
    }

    /// 本作用域自己的模块注册表。
    pub fn registry(&self) -> &ModuleRegistry {
        &self.registry
    }

    /// 距根的深度（根 = 0）。
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// 本作用域已注册的模块 id（注册顺序）。
    pub fn ids(&self) -> Vec<String> {
        self.registry.ids()
    }

    /// 挂一个模块到本作用域（register + mount，立即校验注入）。
    ///
    /// 与直接用 `registry.register_and_mount` 的区别：这里的挂载上下文带着
    /// **本作用域**（`MountContext::nest` 可用），并且本模块在 mount 里建的
    /// 子作用域会记在它名下——卸载它时级联卸载。
    pub fn mount(&self, module: Arc<dyn Module>) -> Result<(), String> {
        let id = module.manifest().id.clone();
        self.registry.register(module)?;
        self.registry
            .mount(&id, &self.mount_context())
            .map(|_| ())
            .map_err(|e| format!("module {id} mount failed: {e}"))
    }

    /// 挂一个模块，先等它的 inject 就绪（最多 `timeout`）。
    pub async fn mount_when_ready(
        &self,
        module: Arc<dyn Module>,
        timeout: Duration,
    ) -> Result<(), String> {
        let id = module.manifest().id.clone();
        self.registry.register(module)?;
        self.registry
            .mount_when_ready(&id, &self.mount_context(), timeout)
            .await
            .map(|_| ())
            .map_err(|e| format!("module {id} mount failed: {e}"))
    }

    /// 卸一个模块（级联卸掉它名下所有子作用域，子先于父）。
    pub fn unmount(&self, id: &str) -> Result<(), ModuleMountError> {
        self.registry.unmount(id)
    }

    /// 用已注册的 id 在本作用域挂载（配置与注入由注册表按 id 处理）。
    pub fn mount_id(&self, id: &str) -> ModuleMountResult {
        self.registry.mount(id, &self.mount_context())
    }

    /// 卸掉本作用域的全部模块（子作用域随各自 owner 一并级联）。
    pub fn shutdown(&self) {
        self.registry.unmount_all();
    }

    /// 为**当前正在挂载的模块**建一个子作用域；不在挂载期时 `None`。
    ///
    /// 子作用域的 ctx 是 [`Ctx::child`]（父链继承），注册表独立。
    /// 通常不直接调用——模块侧用 [`MountContext::nest`]。
    ///
    /// [`MountContext::nest`]: crate::MountContext::nest
    pub fn nest(self: &Arc<Self>) -> Option<Arc<ModuleScope>> {
        let owner = self.links.current_owner()?;
        // 子作用域**继承事件出口**：整棵树的事件汇到同一个接收方，
        // 接收方靠 `depth` / `module` 区分来源。
        let child =
            Self::build_with_sink(self.ctx.child(), self.depth + 1, self.registry.event_sink());
        self.links.adopt(&owner, child.clone());
        Some(child)
    }

    /// 某模块名下已建出的子作用域（只读视图，用于诊断/测试）。
    pub fn children_of(&self, owner: &str) -> Vec<Arc<ModuleScope>> {
        self.links.children_of(owner)
    }

    /// 本作用域名下子作用域总数（不含孙子）。
    pub fn child_scope_count(&self) -> usize {
        self.links.total_children()
    }

    /// 递归描述整棵子树（诊断用）：`[(depth, 模块 id)]`。
    pub fn walk(&self) -> Vec<(usize, String)> {
        let mut out: Vec<(usize, String)> = self
            .registry
            .ids()
            .into_iter()
            .map(|id| (self.depth, id))
            .collect();
        for children in self
            .links
            .children
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
            .values()
        {
            for child in children {
                out.extend(child.walk());
            }
        }
        out
    }

    /// 本作用域的挂载上下文：共享 ctx、带树把手（`MountContext::nest` 可用）。
    /// 压栈"正在挂载的模块"（注册表内部用）。
    pub(crate) fn links_enter(&self, id: &str) {
        self.links.enter(id);
    }

    /// 弹栈（`OwnerGuard::drop`）。
    pub(crate) fn links_leave(&self, id: &str) {
        self.links.leave(id);
    }

    /// 取走某模块名下的子作用域（卸载时级联用）。
    pub(crate) fn links_take_children(&self, owner: &str) -> Vec<Arc<ModuleScope>> {
        self.links.take_children(owner)
    }

    fn mount_context(&self) -> MountContext {
        let base = MountContext::new().with_ctx(Arc::clone(&self.ctx));
        match self.registry.tree_handle() {
            Some(tree) => base.with_tree(tree),
            None => base,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ModuleManifest;
    use crate::module::BuiltinModule;
    use echo_context::Disposer;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 造一个"父模块"：在自己的 mount 里 nest 出子作用域，往里面挂子模块。
    /// 子模块往自己的 ctx 里注册一个服务（父作用域看不见它）。
    fn parent_with_child(
        parent_id: &str,
        child_id: &str,
        parent_mounts: Arc<AtomicUsize>,
        child_mounts: Arc<AtomicUsize>,
        child_disposed: Arc<AtomicUsize>,
    ) -> Arc<dyn Module> {
        let manifest = ModuleManifest::builtin(parent_id, parent_id, "1", "tool", "", "");
        let child_id = child_id.to_string();
        Arc::new(BuiltinModule::new(manifest, move |mctx: &MountContext| {
            parent_mounts.fetch_add(1, Ordering::SeqCst);
            let scope = mctx.nest().expect("有树把手才能 nest");
            let cm = child_mounts.clone();
            let cd = child_disposed.clone();
            let child = Arc::new(BuiltinModule::new(
                ModuleManifest::builtin(&child_id, &child_id, "1", "tool", "", ""),
                move |cmctx: &MountContext| {
                    cm.fetch_add(1, Ordering::SeqCst);
                    // 子作用域自己的服务：父作用域看不到。
                    let ctx = cmctx.ctx.clone().expect("子作用域有 ctx");
                    let keep = ctx.register::<u8>("child-private", 7u8);
                    let cd2 = cd.clone();
                    Ok(vec![
                        keep,
                        Disposer::from_fn(move || {
                            cd2.fetch_add(1, Ordering::SeqCst);
                        }),
                    ])
                },
            ));
            scope.mount(child).expect("子模块挂载");
            Ok(vec![])
        }))
    }

    /// 父模块 nest 出的子作用域：子模块能看见父的服务，父看不见子的。
    #[test]
    fn child_scope_shadows_and_isolates_services() {
        let root_ctx = Arc::new(Ctx::default());
        let _root_svc = root_ctx.register::<u8>("shared", 1u8);
        let scope = ModuleScope::root(root_ctx.clone());
        let module = parent_with_child(
            "p.parent",
            "p.child",
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        );
        scope.mount(module).expect("parent mounts");

        let children = scope.children_of("p.parent");
        assert_eq!(children.len(), 1, "父模块名下应有一个子作用域");
        let child = &children[0];
        assert_eq!(child.depth(), 1);
        assert_eq!(child.ids(), vec!["p.child".to_string()]);
        // 父链继承：子能看见根的服务
        assert!(child.ctx().contains("shared"), "子作用域应继承父的服务");
        // 子私有：根/父都看不见
        assert!(child.ctx().contains("child-private"));
        assert!(!root_ctx.contains("child-private"), "子服务不该漏到根");
        assert!(!scope.ctx().contains("child-private"));
        // resolve 也走父链
        assert_eq!(root_ctx.resolve::<u8>("shared"), Some(1));
        assert_eq!(child.ctx().resolve::<u8>("shared"), Some(1), "子沿父链解析");
        // 类型不符不算命中（哪怕键存在）
        assert_eq!(
            child.ctx().resolve::<i32>("shared"),
            None,
            "类型不同不算命中"
        );
    }

    /// 卸载父模块 → 子作用域级联卸载（子先于父，子模块 disposer 执行）。
    #[test]
    fn unmounting_parent_cascades_to_child_scopes() {
        let root_ctx = Arc::new(Ctx::default());
        let scope = ModuleScope::root(root_ctx);
        let child_mounts = Arc::new(AtomicUsize::new(0));
        let child_disposed = Arc::new(AtomicUsize::new(0));
        scope
            .mount(parent_with_child(
                "p.parent",
                "p.child",
                Arc::new(AtomicUsize::new(0)),
                child_mounts.clone(),
                child_disposed.clone(),
            ))
            .expect("parent mounts");
        assert_eq!(child_mounts.load(Ordering::SeqCst), 1);

        scope.unmount("p.parent").expect("unmount ok");

        assert_eq!(
            child_disposed.load(Ordering::SeqCst),
            1,
            "子模块应随父卸载而被卸载（disposer 执行）"
        );
        // 子作用域已从归属表移除
        assert!(scope.children_of("p.parent").is_empty());
        assert!(!scope.registry().is_mounted("p.parent"));
    }

    /// 嵌套两层：孙模块的副作用也在祖父卸载时被撤掉（递归级联）。
    #[test]
    fn three_levels_cascade_recursively() {
        let root_ctx = Arc::new(Ctx::default());
        let scope = ModuleScope::root(root_ctx);
        let grandchild_mounted = Arc::new(AtomicUsize::new(0));
        let grandchild_disposed = Arc::new(AtomicUsize::new(0));

        // 中层的 mount 里再 nest 一层
        let gc_m = grandchild_mounted.clone();
        let gc_d = grandchild_disposed.clone();
        let middle = Arc::new(BuiltinModule::new(
            ModuleManifest::builtin("p.middle", "m", "1", "tool", "", ""),
            move |mctx: &MountContext| {
                let inner = mctx.nest().expect("middle nests");
                let gc_m = gc_m.clone();
                let gc_d = gc_d.clone();
                let leaf = Arc::new(BuiltinModule::new(
                    ModuleManifest::builtin("p.leaf", "l", "1", "tool", "", ""),
                    move |_| {
                        gc_m.fetch_add(1, Ordering::SeqCst);
                        let d = gc_d.clone();
                        Ok(vec![Disposer::from_fn(move || {
                            d.fetch_add(1, Ordering::SeqCst);
                        })])
                    },
                ));
                inner.mount(leaf).expect("leaf mounts");
                Ok(vec![])
            },
        ));
        scope.mount(middle).expect("middle mounts");
        assert_eq!(grandchild_mounted.load(Ordering::SeqCst), 1);

        // walk 应该看到两层嵌套
        let ids: Vec<String> = scope.walk().into_iter().map(|(_, id)| id).collect();
        assert!(
            ids.contains(&"p.leaf".to_string()),
            "walk 应递归到孙子: {ids:?}"
        );

        scope.unmount("p.middle").expect("unmount ok");
        assert_eq!(
            grandchild_disposed.load(Ordering::SeqCst),
            1,
            "孙模块副作用应随中层卸载递归撤销"
        );
    }

    /// 裸注册表（无作用域）挂载同一个父模块：`nest()` 返回 None，
    /// 但挂载本身不能因此失败——树是**可选**能力。
    #[test]
    fn module_without_tree_cannot_nest_but_still_mounts() {
        let reg = ModuleRegistry::new();
        let nested = Arc::new(AtomicUsize::new(0));
        let n = nested.clone();
        reg.register(Arc::new(BuiltinModule::new(
            ModuleManifest::builtin("p.bare", "b", "1", "tool", "", ""),
            move |mctx: &MountContext| {
                assert!(mctx.nest().is_none(), "裸注册表没有树把手");
                n.fetch_add(1, Ordering::SeqCst);
                Ok(vec![])
            },
        )))
        .unwrap();
        let ctx = MountContext::new().with_ctx(Arc::new(Ctx::default()));
        reg.mount("p.bare", &ctx).expect("mount ok");
        assert_eq!(nested.load(Ordering::SeqCst), 1);
    }

    /// 子作用域的服务注册**沿父链向上通知**：根上的订阅者能看见子树变动。
    #[test]
    fn change_notifications_bubble_up_to_the_root() {
        use echo_context::Change;
        use std::sync::Mutex;

        let root_ctx = Arc::new(Ctx::default());
        let seen: Arc<Mutex<Vec<Change>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let _sub = root_ctx.on_change(move |change| sink.lock().unwrap().push(change));

        let scope = ModuleScope::root(root_ctx.clone());
        scope
            .mount(parent_with_child(
                "p.parent",
                "p.child",
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicUsize::new(0)),
            ))
            .expect("mount");

        let changes = seen.lock().unwrap().clone();
        let provided: Vec<(String, usize)> = changes
            .iter()
            .filter_map(|c| match c {
                Change::Provided { key, depth } => Some((key.clone(), *depth)),
                _ => None,
            })
            .collect();
        assert!(
            provided
                .iter()
                .any(|(key, depth)| key == "child-private" && *depth == 1),
            "根的订阅者应收到子作用域（depth=1）的注册: {provided:?}"
        );
    }

    /// 子作用域**继承事件出口**：整棵树的事件汇到同一接收方，靠 `depth` 区分。
    #[test]
    fn child_scope_inherits_the_event_sink() {
        use std::sync::Mutex;
        let seen: Arc<Mutex<Vec<crate::ModuleEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = seen.clone();
        let root_ctx = Arc::new(Ctx::default());
        let scope = ModuleScope::root(root_ctx);
        scope
            .registry()
            .set_event_sink(Arc::new(move |event| sink_seen.lock().unwrap().push(event)));

        scope
            .mount(parent_with_child(
                "t.parent",
                "t.child",
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicUsize::new(0)),
            ))
            .expect("mount");

        let events = seen.lock().unwrap().clone();
        // 顶层与子作用域的挂载事件都应到达（子作用域带 depth 1）。
        let child_mounted = events
            .iter()
            .find(|e| e.payload["id"] == "t.child")
            .expect("子作用域的挂载事件应到达同一个出口");
        assert_eq!(child_mounted.name, "module.mounted");
        assert_eq!(child_mounted.payload["depth"], 1);
    }
}
