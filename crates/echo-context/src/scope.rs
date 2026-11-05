//! Scoped registration: name-keyed entries with per-scope shadowing.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::ctx::Disposer;

/// A registry whose entries are either global or scoped to one scope key.
///
/// Lookup resolves **scoped first, then global** — a scoped entry replaces
/// its same-named global twin for that scope alone (dsh shadowing). Scope
/// keys are opaque strings supplied by the host — the framework never
/// interprets them (a host typically keys each scope by its own runtime
/// instance identity).
///
/// Registration returns a [`Disposer`]; unloading a scope's module unwinds
/// all its scoped registrations.
pub struct ScopedRegistry<T: Send + Sync + 'static> {
    global: RwLock<HashMap<String, Arc<T>>>,
    scoped: RwLock<HashMap<String, HashMap<String, Arc<T>>>>,
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
        }
    }

    /// Register a global entry (visible to every scope).
    pub fn register_global(self: &Arc<Self>, name: &str, entry: T) -> Disposer {
        self.global
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(name.to_string(), Arc::new(entry));
        let registry = self.clone();
        let name = name.to_string();
        Disposer::from_fn(move || {
            registry
                .global
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&name);
        })
    }

    /// Register a scoped entry (visible only to `scope`; shadows global).
    pub fn register_scoped(self: &Arc<Self>, scope: &str, name: &str, entry: T) -> Disposer {
        self.scoped
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(scope.to_string())
            .or_default()
            .insert(name.to_string(), Arc::new(entry));
        let registry = self.clone();
        let scope = scope.to_string();
        let name = name.to_string();
        Disposer::from_fn(move || {
            let mut scoped = registry
                .scoped
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(entries) = scoped.get_mut(&scope) {
                entries.remove(&name);
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
                return Some(entry.clone());
            }
        }
        self.global
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .cloned()
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
