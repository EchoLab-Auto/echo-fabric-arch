//! 模块事件通道（`Contribution::Events` 的进程内落地）。
//!
//! 协议里的 `Contribution::Events { subscribe }` 一直是"声明了没人读"（SDK 的
//! `serve` 里 `Contribution::Events(_) => None`，宿主侧零消费）。本模块补上
//! **in-process** 的那一半：
//!
//! | 方向 | 机制 | 载体 |
//! |---|---|---|
//! | 模块 → 宿主 | [`MountContext::emit`]（宿主注入 [`EventSink`]） | [`ModuleEvent`] |
//! | 宿主 → 模块 | [`Module::on_event`] + [`Module::subscribed_events`] | 同上 |
//!
//! 语义与子进程轨一致（`Emit` / `Event` 帧），只是同进程时省掉序列化。框架
//! 只搬 `name` + `payload`，**不解释**任何名字——事件词汇表归宿主。
//!
//! [`MountContext::emit`]: crate::MountContext::emit
//! [`Module::on_event`]: crate::Module::on_event
//! [`Module::subscribed_events`]: crate::Module::subscribed_events

use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// 一条模块事件。
///
/// `module` 由**宿主**填充（模块自己 emit 时不必知道自己的 id；注册表在挂载
/// 时把 id 注入 `MountContext`，[`MountContext::emit`] 用的是它）。
///
/// [`MountContext::emit`]: crate::MountContext::emit
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModuleEvent {
    /// 来源模块 id（宿主侧填充；宿主自身发出的事件为空串）。
    #[serde(default)]
    pub module: String,
    /// 事件名（点分小写约定，如 `module.mounted`）。框架不设词汇表。
    pub name: String,
    /// 载荷（自由 JSON；框架不解释）。
    #[serde(default)]
    pub payload: serde_json::Value,
}

impl ModuleEvent {
    /// 造一条事件。
    pub fn new(
        module: impl Into<String>,
        name: impl Into<String>,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            module: module.into(),
            name: name.into(),
            payload,
        }
    }

    /// 宿主侧事件（`module` 空串）。
    pub fn host(name: impl Into<String>, payload: serde_json::Value) -> Self {
        Self::new("", name, payload)
    }
}

/// 事件出口（宿主注入）。
///
/// 为什么是 `Fn` 而不是队列：投递是**同步、即发即走**——模块不该因为宿主
/// 那边慢而卡在 `mount` 里，也不该指望有返回值（fire-and-forget 与协议里的
/// `Emit` 帧同语义）。要不要缓冲是宿主自己的事。
pub type EventSink = Arc<dyn Fn(ModuleEvent) + Send + Sync>;

/// 订阅声明里代表"全部"的通配名（`subscribed_events` 返回它即全收）。
pub const ALL_EVENTS: &str = "*";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ModuleManifest;
    use crate::module::{BuiltinModule, MountContext};
    use crate::registry::ModuleRegistry;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// 模块 emit → 宿主收到，且 `module` 字段由宿主填（模块不必知道自己的 id）。
    #[test]
    fn module_emit_reaches_the_host_sink_with_its_id() {
        let reg = ModuleRegistry::new();
        let seen: Arc<Mutex<Vec<ModuleEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_seen = seen.clone();
        reg.set_event_sink(Arc::new(move |event| sink_seen.lock().unwrap().push(event)));
        reg.register(Arc::new(BuiltinModule::new(
            ModuleManifest::builtin("e.emit", "e", "1", "tool", "", ""),
            |mctx: &MountContext| {
                assert_eq!(mctx.module_id(), "e.emit", "挂载时注入模块 id");
                mctx.emit("turn.started", serde_json::json!({"n": 1}));
                Ok(vec![])
            },
        )))
        .unwrap();
        reg.mount_all(&MountContext::new());

        let events = seen.lock().unwrap().clone();
        // 顺序不固定：模块在 mount 闭包里 emit 的事件先于注册表在挂载完成
        // 后发的 `module.mounted`。所以按名字找，不按序断言。
        let mounted_ev = events
            .iter()
            .find(|e| e.name == "module.mounted")
            .expect("挂载成功应有 lifecycle 事件");
        assert_eq!(mounted_ev.payload["id"], "e.emit");
        let emitted = events
            .iter()
            .find(|e| e.name == "turn.started")
            .expect("模块 emit 的事件应到达宿主");
        assert_eq!(emitted.module, "e.emit", "来源 id 由宿主填");
        assert_eq!(emitted.payload["n"], 1);
    }

    /// 没有出口时 emit 是**静默丢弃**（降级而不是错误）。
    #[test]
    fn emit_without_a_sink_is_a_noop() {
        let reg = ModuleRegistry::new();
        let ran = Arc::new(AtomicUsize::new(0));
        let flag = ran.clone();
        reg.register(Arc::new(BuiltinModule::new(
            ModuleManifest::builtin("e.bare", "b", "1", "tool", "", ""),
            move |mctx: &MountContext| {
                assert!(mctx.events.is_none());
                mctx.emit("nothing.listens", serde_json::Value::Null);
                flag.fetch_add(1, Ordering::SeqCst);
                Ok(vec![])
            },
        )))
        .unwrap();
        reg.mount_all(&MountContext::new());
        assert_eq!(ran.load(Ordering::SeqCst), 1, "没有出口不该让挂载失败");
    }

    /// 订阅匹配：只有声明过的事件名（或 `*`）会收到。
    #[test]
    fn deliver_event_only_reaches_subscribers() {
        let reg = ModuleRegistry::new();
        let got_a: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let got_all = Arc::new(Mutex::new(Vec::new()));
        let got_silent = Arc::new(Mutex::new(Vec::new()));

        let (a, all, silent) = (got_a.clone(), got_all.clone(), got_silent.clone());
        for (id, subs) in [
            ("e.one", vec!["module.mounted"]),
            ("e.all", vec![ALL_EVENTS]),
            ("e.none", vec![]),
        ] {
            let slot = match id {
                "e.one" => a.clone(),
                "e.all" => all.clone(),
                _ => silent.clone(),
            };
            reg.register(Arc::new(
                BuiltinModule::new(ModuleManifest::builtin(id, id, "1", "tool", "", ""), |_| {
                    Ok(vec![])
                })
                .with_subscriptions(subs)
                .with_event_handler(move |event: &ModuleEvent| {
                    slot.lock().unwrap().push(event.name.clone());
                    Ok(())
                }),
            ))
            .unwrap();
        }
        reg.mount_all(&MountContext::new());
        let _ = (a, all, silent);

        assert_eq!(
            reg.deliver_event(&ModuleEvent::host(
                "module.mounted",
                serde_json::Value::Null
            )),
            2,
            "一个精确订阅 + 一个全订阅，共 2 个"
        );
        assert_eq!(
            reg.deliver_event(&ModuleEvent::host("other.event", serde_json::Value::Null)),
            1,
            "只有全订阅的那个收到"
        );
        assert_eq!(*got_a.lock().unwrap(), vec!["module.mounted".to_string()]);
        assert_eq!(
            *got_all.lock().unwrap(),
            vec!["module.mounted".to_string(), "other.event".to_string()]
        );
        assert!(got_silent.lock().unwrap().is_empty(), "没订阅就不收");
    }

    /// 未挂载的模块收不到事件（"禁用的模块不获得任何副作用"同款）。
    #[test]
    fn unmounted_modules_do_not_receive_events() {
        let reg = ModuleRegistry::new();
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        reg.register(Arc::new(
            BuiltinModule::new(
                ModuleManifest::builtin("e.off", "o", "1", "tool", "", ""),
                |_| Ok(vec![]),
            )
            .with_subscriptions([ALL_EVENTS])
            .with_event_handler(move |_: &ModuleEvent| {
                h.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
        ))
        .unwrap();
        // 注册但禁用 → 不挂载
        reg.apply_disabled(&["e.off".to_string()]);
        reg.mount_all(&MountContext::new());
        assert_eq!(
            reg.deliver_event(&ModuleEvent::host("x", serde_json::Value::Null)),
            0
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    /// 卸载会发 `module.unmounted`（模块树/生命周期可观测）。
    #[test]
    fn mount_and_unmount_emit_lifecycle_events() {
        let reg = ModuleRegistry::new();
        let names: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = names.clone();
        reg.set_event_sink(Arc::new(move |event| sink.lock().unwrap().push(event.name)));
        reg.register(Arc::new(BuiltinModule::new(
            ModuleManifest::builtin("e.life", "l", "1", "tool", "", ""),
            |_| Ok(vec![]),
        )))
        .unwrap();
        reg.mount_all(&MountContext::new());
        reg.unmount("e.life").unwrap();
        assert_eq!(
            *names.lock().unwrap(),
            vec!["module.mounted".to_string(), "module.unmounted".to_string()]
        );
    }
}
