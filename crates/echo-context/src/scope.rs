//! Scoped registration: name-keyed entries with per-scope shadowing.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::ctx::Disposer;

/// One registered entry: the value plus its **ownership generation**.
///
/// 代次由注册时分配，撤销只作用于「自己那次注册」——晚到的旧 `Disposer`
/// 不会按名字误删后来的新注册。没有它，`register("port")` → `register("port")`
/// → 第一个 disposer 触发，就把**新的**那份删掉了（重复挂载 / 重挂的经典竞态）。
struct Entry<T> {
    generation: u64,
    value: Arc<T>,
}

/// A registry whose entries are either global or scoped to one scope key.
///
/// Lookup resolves **scoped first, then global** — a scoped entry replaces
/// its same-named global twin for that scope alone (dsh shadowing). Scope
/// keys are opaque strings supplied by the host — the framework never
/// interprets them (a host typically keys each scope by its own runtime
/// instance identity).
///
/// Registration returns a [`Disposer`]; unloading a scope's module unwinds
/// all its scoped registrations. 撤销是**按代次**的：只撤自己那次注册，
/// 不会误删同名的后来者（见 `Entry`）。
pub struct ScopedRegistry<T: Send + Sync + 'static> {
    global: RwLock<HashMap<String, Entry<T>>>,
    scoped: RwLock<HashMap<String, HashMap<String, Entry<T>>>>,
    /// 下一次注册的所有权代次（全局 / 作用域共用一条单调序列）。
    next_generation: AtomicU64,
}

impl<T: Send + Sync + 'static> Default for ScopedRegistry<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Send + Sync + 'static> ScopedRegistry<T> {
    pub fn new() -> Self {
        Self {
            global: RwLock::new(HashMap::new()),
            scoped: RwLock::new(HashMap::new()),
            next_generation: AtomicU64::new(0),
        }
    }

    fn next_generation(&self) -> u64 {
        self.next_generation.fetch_add(1, Ordering::Relaxed)
    }

    /// Register a global entry (visible to every scope).
    pub fn register_global(self: &Arc<Self>, name: &str, entry: T) -> Disposer {
        let generation = self.next_generation();
        self.global
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                name.to_string(),
                Entry {
                    generation,
                    value: Arc::new(entry),
                },
            );
        let registry = self.clone();
        let name = name.to_string();
        Disposer::from_fn(move || {
            let mut global = registry
                .global
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // 只撤自己那一代：同名的后来者（新代次）不受影响。
            if global.get(&name).map(|entry| entry.generation) == Some(generation) {
                global.remove(&name);
            }
        })
    }

    /// Register a scoped entry (visible only to `scope`; shadows global).
    pub fn register_scoped(self: &Arc<Self>, scope: &str, name: &str, entry: T) -> Disposer {
        let generation = self.next_generation();
        self.scoped
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(scope.to_string())
            .or_default()
            .insert(
                name.to_string(),
                Entry {
                    generation,
                    value: Arc::new(entry),
                },
            );
        let registry = self.clone();
        let scope = scope.to_string();
        let name = name.to_string();
        Disposer::from_fn(move || {
            let mut scoped = registry
                .scoped
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(entries) = scoped.get_mut(&scope) {
                // 同上：代次不符 = 已被后来者顶替，撤销必须是无操作。
                if entries.get(&name).map(|entry| entry.generation) == Some(generation) {
                    entries.remove(&name);
                }
                if entries.is_empty() {
                    scoped.remove(&scope);
                }
            }
        })
    }

    /// Look up an entry: scoped first, then global (shadowing).
    pub fn lookup(&self, scope: Option<&str>, name: &str) -> Option<Arc<T>> {
        if let Some(scope) = scope {
            if let Some(entry) = self
                .scoped
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(scope)
                .and_then(|entries| entries.get(name))
            {
                return Some(Arc::clone(&entry.value));
            }
        }
        self.global
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .map(|entry| Arc::clone(&entry.value))
    }

    /// Names visible to a scope (scoped entries shadowing globals).
    pub fn names(&self, scope: Option<&str>) -> Vec<String> {
        let mut names: Vec<String> = self
            .global
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect();
        if let Some(scope) = scope {
            if let Some(entries) = self
                .scoped
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(scope)
            {
                names.extend(entries.keys().cloned());
            }
        }
        names.sort();
        names.dedup();
        names
    }

    pub fn len(&self) -> usize {
        self.global
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
            + self
                .scoped
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .values()
                .map(HashMap::len)
                .sum::<usize>()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_entry_visible_to_all_scopes() {
        let reg: Arc<ScopedRegistry<u32>> = Arc::new(ScopedRegistry::new());
        let _keep = reg.register_global("port", 3132);
        assert_eq!(reg.lookup(None, "port"), Some(Arc::new(3132)));
        assert_eq!(reg.lookup(Some("agent-a"), "port"), Some(Arc::new(3132)));
    }

    #[test]
    fn scoped_entry_shadows_global_for_its_scope_only() {
        let reg: Arc<ScopedRegistry<u32>> = Arc::new(ScopedRegistry::new());
        let _keep_global = reg.register_global("port", 3132);
        let _keep_scoped = reg.register_scoped("agent-a", "port", 3133);

        assert_eq!(
            reg.lookup(Some("agent-a"), "port"),
            Some(Arc::new(3133)),
            "scoped shadows global"
        );
        assert_eq!(
            reg.lookup(Some("agent-b"), "port"),
            Some(Arc::new(3132)),
            "other scopes see the global"
        );
        assert_eq!(reg.lookup(None, "port"), Some(Arc::new(3132)));
    }

    #[test]
    fn missing_entry_lookup_none() {
        let reg: Arc<ScopedRegistry<u32>> = Arc::new(ScopedRegistry::new());
        assert!(reg.lookup(None, "nope").is_none());
    }

    #[test]
    fn scoped_disposer_unwinds_and_removes_empty_scope() {
        let reg: Arc<ScopedRegistry<u32>> = Arc::new(ScopedRegistry::new());
        let disposer = reg.register_scoped("agent-a", "port", 3133);
        assert!(reg.lookup(Some("agent-a"), "port").is_some());
        disposer.dispose();
        assert!(reg.lookup(Some("agent-a"), "port").is_none());
        assert_eq!(reg.len(), 0, "empty scope removed");
    }

    #[test]
    fn global_disposer_unwinds() {
        let reg: Arc<ScopedRegistry<u32>> = Arc::new(ScopedRegistry::new());
        let disposer = reg.register_global("port", 3132);
        assert!(reg.lookup(None, "port").is_some());
        disposer.dispose();
        assert!(reg.lookup(None, "port").is_none());
    }

    /// 旧代次的 disposer **不得**撤销同名的新注册（按 key 无条件删除的经典 ABA）。
    #[test]
    fn stale_global_disposer_keeps_the_newer_registration() {
        let reg: Arc<ScopedRegistry<u32>> = Arc::new(ScopedRegistry::new());
        let first = reg.register_global("port", 3132);
        let _second = reg.register_global("port", 4000);

        first.dispose();

        assert_eq!(
            reg.lookup(None, "port"),
            Some(Arc::new(4000)),
            "旧 disposer 误删了后来者的注册"
        );
        assert_eq!(reg.len(), 1);
    }

    /// 作用域条目同理：旧 disposer 不能清掉同名的新注册，也不能顺手删掉作用域。
    #[test]
    fn stale_scoped_disposer_keeps_the_newer_registration() {
        let reg: Arc<ScopedRegistry<u32>> = Arc::new(ScopedRegistry::new());
        let first = reg.register_scoped("agent-a", "port", 3133);
        let _second = reg.register_scoped("agent-a", "port", 5000);

        first.dispose();

        assert_eq!(
            reg.lookup(Some("agent-a"), "port"),
            Some(Arc::new(5000)),
            "旧 disposer 误删了后来者的作用域注册"
        );
        assert_eq!(reg.len(), 1);
    }

    /// 旧 disposer 触发过后，`Drop` 不再重复生效（幂等）；新注册始终在。
    #[test]
    fn stale_disposer_is_idempotent() {
        let reg: Arc<ScopedRegistry<u32>> = Arc::new(ScopedRegistry::new());
        let first = reg.register_global("port", 3132);
        first.dispose();
        assert!(reg.lookup(None, "port").is_none());

        let _newer = reg.register_global("port", 4000);
        assert_eq!(reg.lookup(None, "port"), Some(Arc::new(4000)));
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn names_include_scoped_and_dedup() {
        let reg: Arc<ScopedRegistry<u32>> = Arc::new(ScopedRegistry::new());
        let _keep = reg.register_global("port", 3132);
        let _keep2 = reg.register_global("host", 0);
        let _keep3 = reg.register_scoped("agent-a", "port", 3133);
        let _keep4 = reg.register_scoped("agent-a", "model", 7);
        let names = reg.names(Some("agent-a"));
        assert_eq!(names, vec!["host", "model", "port"]);
    }
}
