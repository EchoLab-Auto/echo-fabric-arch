//! Service locator (`Ctx`) and reversible registration (`Disposer`).

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::Notify;

use crate::ServiceKey;

/// One registered service, erased.
type ErasedService = Box<dyn Any + Send + Sync>;

/// A service locator keyed by name.
///
/// Services are registered as `Arc<dyn Trait>` (or any `Clone + 'static`
/// value) under a string key; consumers resolve by key and trait type,
/// never importing a concrete provider. This is the Rust analogue of dsh's
/// `ctx.<key>` service lookup.
///
/// Registration returns a [`Disposer`]; disposing removes the service, so
/// plugin unload unwinds its contributions.
///
/// [`Ctx::register`] / [`Ctx::resolve`] use plain string keys; the
/// strongly-typed [`ServiceKey`] API on the same table — [`Ctx::provide`],
/// [`Ctx::service`], [`Ctx::require`], [`Ctx::service_or_wait`] — pairs each
/// name with the exact stored type and lets consumers wait for a service to
/// become ready instead of hand-rolling startup ordering.
///
/// ## 父子作用域（dsh 的 `ctx.isolate()` 对应物）
///
/// [`Ctx::child`] 建一个**子作用域**：父链上的服务对子可见，子自己的注册
/// 对父**不可见**（遮蔽 = 子作用域内的同名覆盖，随子作用域销毁而恢复）。
/// 插件树因此可以「父插件装根服务、子插件在子作用域里装自己那份」，
/// 而互不污染——这正是 dsh 里 `ctx.plugin()` 嵌套所依赖的隔离语义。
///
/// 变更通知沿父链**向上**传播（根能看见整棵树的注册/注销），等待则在
/// **整个家族**内共享一个唤醒源（子等父提供的服务不会漏唤醒）。
#[derive(Default)]
pub struct Ctx {
    services: RwLock<HashMap<String, ErasedService>>,
    /// 父作用域（`None` = 根）。
    parent: Option<Arc<Ctx>>,
    /// 变更订阅者（本作用域自己的；父链的订阅者由向上传播通知）。
    subscribers: RwLock<Vec<ChangeListener>>,
    /// Woken whenever a service is provided or a registration is disposed,
    /// so [`Ctx::service_or_wait`] can re-check the table.
    ///
    /// **家族共享**：子作用域与父共用同一个 `Notify`，因此「子等父提供的
    /// 服务」不会漏掉唤醒；多唤醒（别的键变动）无害——等待方醒来只是重查。
    ready: Arc<Notify>,
}

/// 变更回调（[`Ctx::on_change`] 的入参类型）。
///
/// 别名只是为了给 clippy 一个能读的短名字：`Arc<dyn Fn(Change) + Send + Sync>`
/// 直接写在字段上会被判"类型过于复杂"。
type ChangeListener = Arc<dyn Fn(Change) + Send + Sync>;

/// 一次服务表变更（[`Ctx::on_change`] 的载荷）。
///
/// 框架只用它做「依赖变更 → 重新核对」：注入声明里要求的键消失时，宿主
/// 可以把依赖它的插件卸下并标成待挂载。**不含取值**——订阅者若需要值，
/// 自己去 [`Ctx::resolve`]，这样通知不会把服务对象抄一份到队列里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// 某键被注册（`register` / `provide`）。
    Provided {
        /// 服务键。
        key: String,
        /// 发生变更的作用域深度（0 = 根；见 [`Ctx::depth`]）。
        depth: usize,
    },
    /// 某键被注销（disposer 生效）。
    Disposed {
        /// 服务键。
        key: String,
        /// 发生变更的作用域深度（0 = 根；见 [`Ctx::depth`]）。
        depth: usize,
    },
}

impl Ctx {
    /// Register a service under `key`.
    ///
    /// `T` is normally `Arc<dyn Trait>` (the erased box holds the trait
    /// object); it can also be a concrete `Clone + 'static` value. Returns a
    /// disposer that removes the registration.
    pub fn register<T: Any + Send + Sync>(self: &Arc<Self>, key: &str, service: T) -> Disposer {
        self.services
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key.to_string(), Box::new(service));
        self.publish(Change::Provided {
            key: key.to_string(),
            depth: self.depth(),
        });
        let owned = self.clone();
        let owned_key = key.to_string();
        Disposer::from_fn(move || owned.remove_and_wake(&owned_key))
    }

    /// Resolve a service by key.
    ///
    /// `T` must match the registered type exactly — for a trait-object
    /// registration, resolve with the same `Arc<dyn Trait>` type.
    ///
    /// 先查本作用域，再沿父链向上（dsh 的 ctx 继承语义）；子作用域的同名
    /// 注册遮蔽父的那一份。
    pub fn resolve<T: Any + Send + Sync + Clone>(&self, key: &str) -> Option<T> {
        if let Some(found) = {
            let guard = self
                .services
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard
                .get(key)
                .and_then(|boxed| boxed.downcast_ref::<T>())
                .cloned()
        } {
            return Some(found);
        }
        self.parent.as_ref()?.resolve::<T>(key)
    }

    /// Whether a key is registered (without resolving) — including along the
    /// parent chain.
    pub fn contains(&self, key: &str) -> bool {
        if self
            .services
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(key)
        {
            return true;
        }
        self.parent.as_ref().is_some_and(|p| p.contains(key))
    }

    /// All registered keys visible from here (本作用域 ∪ 父链，本作用域优先)。
    ///
    /// 可见 ≠ 可解析：`keys()` 会包含父链的键，但**没有值**——要值请用
    /// [`resolve`](Self::resolve) / [`service`](Self::service)。
    ///
    /// 返回顺序**按名字排序**：底层是 `HashMap`（迭代顺序随机），而这份清单会进
    /// `MissingService.available` 与面板——随机顺序会让同一个错误每次长得不一样。
    pub fn keys(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .services
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .cloned()
            .collect();
        if let Some(parent) = &self.parent {
            for key in parent.keys() {
                if !out.contains(&key) {
                    out.push(key);
                }
            }
        }
        // 排序：底层是 HashMap，迭代顺序随机；而这份清单是要给人看的
        // （`MissingService.available`、面板、文档示例）。
        out.sort();
        out
    }

    /// 建一个**子作用域**（父链 + 共享唤醒源 + 独立订阅者表）。
    ///
    /// 子作用域的服务对父不可见；父的对其可见。用于插件树：父插件在根
    /// 作用域装公共服务，子插件在自己的子作用域里装私有实现。
    pub fn child(self: &Arc<Self>) -> Arc<Ctx> {
        Arc::new(Ctx {
            services: RwLock::new(HashMap::new()),
            parent: Some(self.clone()),
            subscribers: RwLock::new(Vec::new()),
            ready: self.ready.clone(),
        })
    }

    /// 父作用域（根为 `None`）。
    pub fn parent(&self) -> Option<&Arc<Ctx>> {
        self.parent.as_ref()
    }

    /// 距根的深度（根 = 0）。变更通知里带着它，便于订阅者判断"哪层变了"。
    pub fn depth(&self) -> usize {
        match &self.parent {
            Some(parent) => 1 + parent.depth(),
            None => 0,
        }
    }

    /// 订阅本作用域（含子作用域）的服务表变更。
    ///
    /// 通知沿父链向上传播，所以在根上订阅就能看见整棵树。返回的
    /// [`Disposer`] 取消订阅。回调在**注册方线程**同步执行——别在里面做
    /// 重活，更别再去改这个 `Ctx`（会撞同一把锁）。
    pub fn on_change(self: &Arc<Self>, f: impl Fn(Change) + Send + Sync + 'static) -> Disposer {
        let entry: ChangeListener = Arc::new(f);
        self.subscribers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(entry.clone());
        let owned = self.clone();
        Disposer::from_fn(move || {
            let mut subs = owned
                .subscribers
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // 按地址比对：订阅者是 `Arc<dyn Fn>`，没有 Eq 可用。
            let target = Arc::as_ptr(&entry) as *const ();
            subs.retain(|s| Arc::as_ptr(s) as *const () != target);
        })
    }

    /// 等这些键**全部**在（本作用域或父链上），最多等 `timeout`。
    ///
    /// 命中立即返回；超时返回 [`ServiceError::Missing`]，`key` = 第一个仍
    /// 缺失的键，`available` = 当前可见的全部键。这是「声明式 inject 的
    /// 就绪等待」的底座：插件声明要什么，宿主拿它等，不用手编启动顺序。
    ///
    /// 与 [`service_or_wait`](Self::service_or_wait) 同款防丢唤醒：先把
    /// `Notified` 注册进等待表再复查一遍表。
    pub async fn wait_for_keys(
        &self,
        keys: &[String],
        timeout: Duration,
    ) -> Result<(), ServiceError> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(missing) = keys.iter().find(|key| !self.contains(key)) {
                let missing = missing.clone();
                // 注册等待者再复查（防丢唤醒）。
                let notified = self.ready.notified();
                tokio::pin!(notified);
                let _ = notified.as_mut().enable();
                if self.contains(&missing) {
                    continue;
                }
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    return Err(ServiceError::Missing {
                        key: missing,
                        available: self.keys(),
                    });
                }
                tokio::select! {
                    _ = notified => {}
                    _ = tokio::time::sleep(remaining) => {
                        // a provide may have raced the timer
                        if self.contains(&missing) {
                            continue;
                        }
                        return Err(ServiceError::Missing {
                            key: missing,
                            available: self.keys(),
                        });
                    }
                }
                continue;
            }
            return Ok(());
        }
    }

    /// 把变更通知给本作用域的订阅者，并沿父链向上传播。
    fn publish(&self, change: Change) {
        let subs = self
            .subscribers
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        for sub in subs {
            sub(change.clone());
        }
        self.ready.notify_waiters();
        if let Some(parent) = &self.parent {
            parent.publish(change);
        }
    }

    /// Provide `service` under a strongly-typed `key`.
    ///
    /// Unlike [`register`](Self::register), a name can only be provided once:
    /// if `key.name()` is already occupied — by *any* type, including a
    /// string-keyed [`register`](Self::register) — the call fails with
    /// [`ServiceError::Conflict`] and the table is left unchanged.
    ///
    /// On success the service is visible to [`service`](Self::service) /
    /// [`require`](Self::require) immediately, every
    /// [`service_or_wait`](Self::service_or_wait) waiter is woken, and the
    /// returned [`Disposer`] removes the service (waking waiters again) when
    /// dropped or disposed.
    pub fn provide<T: Any + Send + Sync>(
        self: &Arc<Self>,
        key: &ServiceKey<T>,
        service: T,
    ) -> Result<Disposer, ServiceError> {
        {
            let mut services = self
                .services
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if services.contains_key(key.name()) {
                return Err(ServiceError::Conflict {
                    key: key.name().to_string(),
                });
            }
            services.insert(key.name().to_string(), Box::new(service));
        }
        // 与 `register` 同款：广播给订阅者（父链向上传播）+ 唤醒等待者。
        self.publish(Change::Provided {
            key: key.name().to_string(),
            depth: self.depth(),
        });
        let ctx = self.clone();
        let name = key.name();
        Ok(Disposer::from_fn(move || ctx.remove_and_wake(name)))
    }

    /// Resolve the service stored under a typed `key`.
    ///
    /// Returns `None` both when the key is not registered and when it holds a
    /// value of a different type.
    pub fn service<T: Any + Send + Sync + Clone>(&self, key: &ServiceKey<T>) -> Option<T> {
        if let Some(found) = {
            let services = self
                .services
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            services
                .get(key.name())
                .and_then(|boxed| boxed.downcast_ref::<T>())
                .cloned()
        } {
            return Some(found);
        }
        self.parent.as_ref()?.service(key)
    }

    /// Resolve the service stored under a typed `key`, or report what *is*
    /// available when it is missing.
    pub fn require<T: Any + Send + Sync + Clone>(
        &self,
        key: &ServiceKey<T>,
    ) -> Result<T, ServiceError> {
        match self.service(key) {
            Some(service) => Ok(service),
            None => Err(ServiceError::Missing {
                key: key.name().to_string(),
                available: self.keys(),
            }),
        }
    }

    /// Resolve the service stored under a typed `key`, waiting up to
    /// `timeout` for it to be provided.
    ///
    /// Returns immediately on a hit; otherwise waits for a
    /// provide/dispose notification and re-checks the table. Returns `None`
    /// on timeout. The loop registers with the notifier *before* its final
    /// table check, so a `provide` racing the wait cannot be lost.
    pub async fn service_or_wait<T: Any + Send + Sync + Clone>(
        &self,
        key: &ServiceKey<T>,
        timeout: Duration,
    ) -> Option<T> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            // Fast path: already provided.
            if let Some(service) = self.service(key) {
                return Some(service);
            }
            // A `Notified` future only joins the wait list when polled (or
            // explicitly `enable`d), so register before the re-check below: a
            // `provide` racing this loop is then either seen by the re-check
            // or guaranteed to wake the registered future.
            let notified = self.ready.notified();
            tokio::pin!(notified);
            let _ = notified.as_mut().enable();
            if let Some(service) = self.service(key) {
                return Some(service);
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return self.service(key);
            }
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(remaining) => {
                    // Final look: a `provide` may have raced the timer.
                    return self.service(key);
                }
            }
        }
    }

    /// Remove `name` (if present) and wake every
    /// [`service_or_wait`](Self::service_or_wait) waiter so it can re-check
    /// the table.
    fn remove_and_wake(self: &Arc<Self>, name: &str) {
        let removed = {
            let mut services = self
                .services
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            services.remove(name).is_some()
        };
        // 只有真的移除了才广播：`Disposer` 可能是重复生效的空操作，
        // 那不该让订阅者做一次无意义的重新核对。
        if removed {
            self.publish(Change::Disposed {
                key: name.to_string(),
                depth: self.depth(),
            });
        } else {
            self.ready.notify_waiters();
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.services
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
}

/// Errors produced by the strongly-typed [`ServiceKey`] API.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServiceError {
    /// The key is already registered (by any producer, under any type).
    #[error("service key `{key}` is already provided")]
    Conflict { key: String },
    /// The key is missing, or holds a type other than the one requested.
    #[error("service key `{key}` is not available; registered keys: {available:?}")]
    Missing { key: String, available: Vec<String> },
}

/// Reversible registration handle.
///
/// Drop or call [`dispose`](Self::dispose) to unwind the registration. The
/// Rust analogue of dsh's `ctx.effect()` disposer.
pub struct Disposer {
    inner: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl Disposer {
    /// Build a disposer from a custom unwind closure.
    pub fn from_fn(f: impl FnOnce() + Send + Sync + 'static) -> Self {
        Self {
            inner: Some(Box::new(f)),
        }
    }

    /// Remove the registered service.
    pub fn dispose(mut self) {
        if let Some(f) = self.inner.take() {
            f();
        }
    }
}

impl Drop for Disposer {
    fn drop(&mut self) {
        if let Some(f) = self.inner.take() {
            f();
        }
    }
}

impl From<(&str, Arc<Ctx>)> for Disposer {
    fn from((key, ctx): (&str, Arc<Ctx>)) -> Self {
        let key = key.to_string();
        Self::from_fn(move || {
            ctx.services
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&key);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    trait Greeter: Send + Sync {
        fn greet(&self) -> &'static str;
    }
    struct Hello;
    impl Greeter for Hello {
        fn greet(&self) -> &'static str {
            "hello"
        }
    }

    fn expect_conflict(res: Result<Disposer, ServiceError>) -> ServiceError {
        match res {
            Ok(_) => panic!("expected ServiceError::Conflict, got Ok"),
            Err(err) => err,
        }
    }

    #[test]
    fn register_and_resolve_trait_object() {
        let ctx = Arc::new(Ctx::default());
        let greeter: Arc<dyn Greeter> = Arc::new(Hello);
        let _keep = ctx.register::<Arc<dyn Greeter>>("greeter", greeter);

        let got: Arc<dyn Greeter> = ctx.resolve::<Arc<dyn Greeter>>("greeter").unwrap();
        assert_eq!(got.greet(), "hello");
    }

    #[test]
    fn register_and_resolve_concrete_value() {
        let ctx = Arc::new(Ctx::default());
        let _keep = ctx.register::<u32>("port", 3132);
        assert_eq!(ctx.resolve::<u32>("port"), Some(3132));
    }

    #[test]
    fn resolve_missing_key_is_none() {
        let ctx = Arc::new(Ctx::default());
        assert!(ctx.resolve::<u32>("nope").is_none());
    }

    #[test]
    fn disposer_removes_registration() {
        let ctx = Arc::new(Ctx::default());
        let disposer = ctx.register::<u32>("port", 3132);
        assert!(ctx.contains("port"));
        disposer.dispose();
        assert!(!ctx.contains("port"));
        assert_eq!(ctx.len(), 0);
    }

    #[test]
    fn disposer_unwinds_on_drop() {
        let ctx = Arc::new(Ctx::default());
        {
            let _disposer = ctx.register::<u32>("port", 3132);
            assert!(ctx.contains("port"));
        }
        assert!(!ctx.contains("port"), "drop unwinds the registration");
    }

    #[test]
    fn type_mismatch_resolves_none() {
        let ctx = Arc::new(Ctx::default());
        let _keep = ctx.register::<u32>("port", 3132);
        assert!(ctx.resolve::<u64>("port").is_none());
    }

    #[test]
    fn re_register_replaces_and_dispose_removes() {
        let ctx = Arc::new(Ctx::default());
        let _d1 = ctx.register::<u32>("port", 3132);
        let _d2 = ctx.register::<u32>("port", 3133);
        assert_eq!(ctx.resolve::<u32>("port"), Some(3133));
        // Disposing the newest registration removes the key entirely.
        drop(_d2);
        assert!(!ctx.contains("port"));
        // The earlier disposer is now a no-op (key already gone).
        drop(_d1);
        assert!(!ctx.contains("port"));
    }

    // ---- strongly-typed ServiceKey API --------------------------------------

    #[test]
    fn provide_and_service_roundtrip() {
        let ctx = Arc::new(Ctx::default());
        let key = ServiceKey::<u32>::new("typed-port");
        assert_eq!(ctx.service(&key), None);

        let keep = ctx.provide(&key, 3132).expect("provide");
        assert_eq!(ctx.service(&key), Some(3132));
        assert!(ctx.contains("typed-port"));
        // The legacy string API shares the same table entry.
        assert_eq!(ctx.resolve::<u32>("typed-port"), Some(3132));

        keep.dispose();
        assert_eq!(ctx.service(&key), None);
        assert!(!ctx.contains("typed-port"));
    }

    #[test]
    fn provide_typed_trait_object_roundtrip() {
        static GREETER: ServiceKey<Arc<dyn Greeter>> = ServiceKey::new("typed-greeter");
        let ctx = Arc::new(Ctx::default());
        let greeter: Arc<dyn Greeter> = Arc::new(Hello);
        let keep = ctx.provide(&GREETER, greeter).expect("provide");

        let got: Arc<dyn Greeter> = ctx.service(&GREETER).expect("typed service");
        assert_eq!(got.greet(), "hello");
        assert_eq!(ctx.require(&GREETER).unwrap().greet(), "hello");

        drop(keep);
        assert!(ctx.service::<Arc<dyn Greeter>>(&GREETER).is_none());
    }

    #[test]
    fn duplicate_provide_conflicts_and_keeps_original() {
        let ctx = Arc::new(Ctx::default());
        let key = ServiceKey::<u32>::new("dup");
        let _keep = ctx.provide(&key, 1).expect("first provide");

        let err = expect_conflict(ctx.provide(&key, 2));
        assert_eq!(
            err,
            ServiceError::Conflict {
                key: "dup".to_string()
            }
        );
        assert_eq!(ctx.service(&key), Some(1), "original value stays in place");
    }

    #[test]
    fn provide_conflict_is_type_agnostic() {
        let ctx = Arc::new(Ctx::default());
        let number = ServiceKey::<u32>::new("occupied");
        let text = ServiceKey::<String>::new("occupied");
        let _keep = ctx.provide(&number, 7).expect("first provide");

        // Same name, different type → still a conflict.
        let err = expect_conflict(ctx.provide(&text, "seven".to_string()));
        assert_eq!(
            err,
            ServiceError::Conflict {
                key: "occupied".to_string()
            }
        );

        // A string-keyed registration occupies the name just the same.
        let ctx2 = Arc::new(Ctx::default());
        let _legacy = ctx2.register::<u32>("occupied", 1);
        let err = expect_conflict(ctx2.provide(&number, 7));
        assert_eq!(
            err,
            ServiceError::Conflict {
                key: "occupied".to_string()
            }
        );
    }

    #[test]
    fn service_type_mismatch_is_none() {
        let ctx = Arc::new(Ctx::default());
        let as_u32 = ServiceKey::<u32>::new("mismatch");
        let as_u64 = ServiceKey::<u64>::new("mismatch");
        let _keep = ctx.provide(&as_u32, 7).unwrap();

        assert_eq!(ctx.service(&as_u64), None, "different type under same name");
        assert_eq!(ctx.service(&as_u32), Some(7));
    }

    #[test]
    fn require_missing_reports_available_keys() {
        let ctx = Arc::new(Ctx::default());
        let _legacy = ctx.register::<u32>("legacy", 1);
        let typed = ServiceKey::<u32>::new("typed");
        let _keep = ctx.provide(&typed, 2).unwrap();

        // Typed lookup also finds a string-keyed registration under the name.
        assert_eq!(ctx.service(&ServiceKey::<u32>::new("legacy")), Some(1));

        match ctx.require(&ServiceKey::<u32>::new("nope")) {
            Err(ServiceError::Missing { key, available }) => {
                assert_eq!(key, "nope");
                assert!(
                    available.contains(&"legacy".to_string()),
                    "available: {available:?}"
                );
                assert!(
                    available.contains(&"typed".to_string()),
                    "available: {available:?}"
                );
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    fn require_returns_provided_value() {
        let ctx = Arc::new(Ctx::default());
        let key = ServiceKey::<u32>::new("present");
        let _keep = ctx.provide(&key, 42).unwrap();
        assert_eq!(ctx.require(&key).expect("require"), 42);
    }

    #[tokio::test]
    async fn service_or_wait_returns_immediately_when_registered() {
        let ctx = Arc::new(Ctx::default());
        let key = ServiceKey::<u32>::new("ready");
        let _keep = ctx.provide(&key, 3132).unwrap();
        assert_eq!(
            ctx.service_or_wait(&key, Duration::from_secs(5)).await,
            Some(3132)
        );
    }

    #[tokio::test]
    async fn service_or_wait_waits_for_late_provider() {
        let ctx = Arc::new(Ctx::default());
        let key = ServiceKey::<u32>::new("late");
        let provider_ctx = Arc::clone(&ctx);
        let provider = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            provider_ctx.provide(&key, 9).unwrap()
        });

        let got = ctx.service_or_wait(&key, Duration::from_secs(5)).await;
        assert_eq!(got, Some(9));
        // Keep the registration alive past the assertion.
        drop(provider.await.expect("provider task"));
    }

    #[tokio::test]
    async fn service_or_wait_times_out_when_provider_is_too_late() {
        let ctx = Arc::new(Ctx::default());
        let key = ServiceKey::<u32>::new("too-late");
        let provider_ctx = Arc::clone(&ctx);
        let late = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            provider_ctx.provide(&key, 1)
        });

        let started = std::time::Instant::now();
        let got = ctx.service_or_wait(&key, Duration::from_millis(50)).await;
        assert_eq!(got, None, "a 50ms wait must not see a 200ms provider");
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_millis(45),
            "should have actually waited, got {waited:?}"
        );
        assert!(
            waited < Duration::from_secs(1),
            "must not hang past its timeout, got {waited:?}"
        );

        late.abort();
    }

    #[tokio::test]
    async fn dispose_during_wait_does_not_hang_the_waiter() {
        let ctx = Arc::new(Ctx::default());
        let provided = ServiceKey::<u32>::new("provided-then-disposed");
        let wanted = ServiceKey::<u32>::new("wanted");

        // A typed registration whose disposal fires the wake-up below.
        let bystander = ctx.provide(&provided, 1).unwrap();

        let waiter_ctx = Arc::clone(&ctx);
        let waiter = tokio::spawn(async move {
            waiter_ctx
                .service_or_wait(&wanted, Duration::from_secs(2))
                .await
        });

        // Dispose an unrelated service while the waiter is parked: the
        // notification wakes it, it re-checks (still missing) and keeps
        // waiting — no spurious return, no hang.
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(bystander);

        // The awaited service then arrives; the waiter must observe it.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _keep = ctx.provide(&wanted, 3132).unwrap();

        let got = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("waiter must not hang")
            .expect("waiter task must not panic");
        assert_eq!(got, Some(3132));
    }

    #[tokio::test]
    async fn dispose_wake_does_not_falsely_satisfy_waiter() {
        let ctx = Arc::new(Ctx::default());
        let provided = ServiceKey::<u32>::new("bystander-typed");
        let wanted = ServiceKey::<u32>::new("stays-missing");
        let bystander = ctx.provide(&provided, 1).unwrap();

        let started = std::time::Instant::now();
        let waiter_ctx = Arc::clone(&ctx);
        let waiter = tokio::spawn(async move {
            waiter_ctx
                .service_or_wait(&wanted, Duration::from_millis(200))
                .await
        });

        // Dispose during the wait → waiter re-checks and keeps waiting.
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(bystander);

        let got = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("waiter must not hang")
            .expect("waiter task must not panic");
        assert_eq!(
            got, None,
            "an unrelated disposal must not satisfy the waiter"
        );
        // It kept waiting after the ~50ms wake (its timeout is 200ms): it did
        // not bail out early on the notification.
        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "must keep waiting after the dispose wake, returned after {:?}",
            started.elapsed()
        );
    }

    // ── 父子作用域 / 变更订阅 / 批量等待（2026-11）─────────────────────

    #[test]
    fn child_inherits_parent_services_and_isolates_its_own() {
        let root = Arc::new(Ctx::default());
        let _keep = root.register::<u32>("port", 3132);
        let child = root.child();
        let _own = child.register::<u32>("own", 7);
        assert_eq!(child.depth(), 1);
        assert_eq!(root.depth(), 0);
        assert!(child.parent().is_some());
        assert!(root.parent().is_none());

        // 父 → 子 可见
        assert_eq!(child.resolve::<u32>("port"), Some(3132));
        assert!(child.contains("port"));
        // 子 → 父 不可见
        assert!(!root.contains("own"));
        assert_eq!(root.resolve::<u32>("own"), None);
        // keys 是并集（父链含进来），且已排序
        assert_eq!(
            child.keys(),
            vec!["own".to_string(), "port".to_string()],
            "keys() 按名字排序，父链的键也含在内"
        );
    }

    #[test]
    fn keys_are_sorted_regardless_of_registration_order() {
        // HashMap 的迭代顺序随机；keys() 必须给出稳定顺序，否则
        // `MissingService.available` 这类给人看的清单每次都不一样。
        let ctx = Arc::new(Ctx::default());
        let _c = ctx.register::<u32>("zeta", 1);
        let _a = ctx.register::<u32>("alpha", 2);
        let _b = ctx.register::<u32>("mu", 3);
        assert_eq!(ctx.keys(), vec!["alpha", "mu", "zeta"]);
    }

    #[test]
    fn child_shadows_parent_under_the_same_key() {
        let root = Arc::new(Ctx::default());
        let _keep = root.register::<u32>("port", 3132);
        let child = root.child();
        let _child_keep = child.register::<u32>("port", 9999);
        assert_eq!(child.resolve::<u32>("port"), Some(9999), "子遮蔽父");
        assert_eq!(root.resolve::<u32>("port"), Some(3132), "父不受影响");
        // 子作用域撤掉后恢复父的
        drop(_child_keep);
        assert_eq!(child.resolve::<u32>("port"), Some(3132));
    }

    #[test]
    fn change_notifications_bubble_up_the_parent_chain() {
        use std::sync::Mutex;
        let root = Arc::new(Ctx::default());
        let seen: Arc<Mutex<Vec<Change>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let _sub = root.on_change(move |change| sink.lock().unwrap().push(change));

        let child = root.child();
        let keep = child.register::<u32>("deep", 1);
        keep.dispose();

        let changes = seen.lock().unwrap().clone();
        assert_eq!(
            changes,
            vec![
                Change::Provided {
                    key: "deep".into(),
                    depth: 1
                },
                Change::Disposed {
                    key: "deep".into(),
                    depth: 1
                },
            ]
        );
    }

    #[test]
    fn on_change_disposer_unsubscribes() {
        use std::sync::Mutex;
        let root = Arc::new(Ctx::default());
        let count = Arc::new(Mutex::new(0));
        let c = count.clone();
        let sub = root.on_change(move |_| *c.lock().unwrap() += 1);
        let _keep = root.register::<u32>("a", 1);
        assert_eq!(*count.lock().unwrap(), 1);
        sub.dispose();
        let _keep2 = root.register::<u32>("b", 2);
        assert_eq!(*count.lock().unwrap(), 1, "退订后不再收到");
    }

    #[tokio::test]
    async fn wait_for_keys_returns_immediately_when_ready() {
        let root = Arc::new(Ctx::default());
        let _keep = root.register::<u32>("a", 1);
        let started = std::time::Instant::now();
        root.wait_for_keys(&["a".to_string()], Duration::from_secs(5))
            .await
            .expect("ready");
        assert!(started.elapsed() < Duration::from_millis(100), "不该等");
    }

    #[tokio::test]
    async fn wait_for_keys_wakes_when_a_child_provides() {
        let root = Arc::new(Ctx::default());
        let child = root.child();
        let waiter = {
            let child = child.clone();
            tokio::spawn(async move {
                child
                    .wait_for_keys(&["late".to_string()], Duration::from_secs(2))
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _keep = child.register::<u32>("late", 1);
        waiter.await.unwrap().expect("child 的 provide 应唤醒等待");
    }

    #[tokio::test]
    async fn wait_for_keys_times_out_with_the_missing_key() {
        let root = Arc::new(Ctx::default());
        let _keep = root.register::<u32>("present", 1);
        let err = root
            .wait_for_keys(
                &["present".to_string(), "absent".to_string()],
                Duration::from_millis(30),
            )
            .await;
        match err {
            Err(ServiceError::Missing { key, available }) => {
                assert_eq!(key, "absent");
                assert_eq!(available, vec!["present".to_string()]);
            }
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn typed_provide_broadcasts_like_register() {
        use std::sync::Mutex;
        static KEY: ServiceKey<u32> = ServiceKey::new("typed.port");
        let root = Arc::new(Ctx::default());
        let seen: Arc<Mutex<Vec<Change>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let _sub = root.on_change(move |c| sink.lock().unwrap().push(c));
        let keep = root.provide(&KEY, 1u8 as u32).expect("provide");
        keep.dispose();
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![
                Change::Provided {
                    key: "typed.port".into(),
                    depth: 0
                },
                Change::Disposed {
                    key: "typed.port".into(),
                    depth: 0
                },
            ]
        );
    }
}
