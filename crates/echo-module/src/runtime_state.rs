//! 模块**运行态**：框架自己维护的那部分状态，宿主不必再包一层。
//!
//! ## 为什么有这个模块（2026-11，宿主适配层上收）
//!
//! 在把内置能力逐一搬成模块之后，宿主侧还剩一个 1,559 行的 `ModuleHost`
//! ——它包着 `ModuleRegistry`，又自己维护了四样状态：缓存的 `MountContext`、
//! 事件订阅**兴趣集**、投递**重入保护**、投递**计数**。
//!
//! 那四样都**与任何领域无关**，任何宿主都会需要：
//!
//! | 状态 | 为什么通用 |
//! |---|---|
//! | 缓存的 `MountContext` | [`ModuleRegistry::set_enabled`] / `restart` / `reconcile` 都要它；宿主每次命令还得自己传，只能自己存一份 |
//! | 兴趣集 | `emit` 是热路径（流式正文每 token 一条），没人订阅时要能一次原子读就返回 |
//! | 重入保护 | 模块的 `on_event` 里再触发一次 `emit` 会自递归 |
//! | 投递计数 | 排查「事件为什么没到」的第一手证据 |
//!
//! 于是它们上收到这里：注册表成为**唯一的状态持有者**，宿主只剩领域翻译
//! （例如把自家事件类型翻成 [`crate::ModuleEvent`]）与配置格式。
//!
//! ## 兴趣集为什么只统计**已挂载**的模块
//!
//! 没挂载的模块不该收到事件；替它"占位"订阅会让热路径付无意义的序列化
//! 代价。刷新时机：挂载 / 卸载 / 启停 / 重启 / `reconcile` 之后——由注册表
//! 自己在这些路径上调，宿主不必记得。

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::module::MountContext;

/// 已挂载模块订阅的事件名（热路径闸门）。
#[derive(Default)]
pub(crate) struct EventInterest {
    /// 是否有人订阅（热路径的快速判定；与 `names` / `all` 保持一致）。
    any: AtomicBool,
    /// 精确订阅的名字。
    names: HashSet<String>,
    /// 有模块订阅了 [`crate::ALL_EVENTS`]（全收）。
    all: bool,
}

impl EventInterest {
    /// 该事件名是否有人要（假设 `any` 已为真）。
    pub(crate) fn wants(&self, name: &str) -> bool {
        self.all || self.names.contains(name)
    }
}

/// 运行态聚合（注册表独占，见模块文档）。
///
/// 名字刻意不叫 `ModuleRuntime`——那个词已经是 manifest 里的**运行形态**
/// （`in_process` / `subprocess`），两者不是一回事。
#[derive(Default)]
pub(crate) struct RuntimeState {
    /// 最近一次 `set_mount_ctx` 的上下文（供无参版本的生命周期方法使用）。
    mount_ctx: Option<MountContext>,
    /// 订阅兴趣集。
    interest: EventInterest,
    /// 投递重入保护：模块的 `on_event` 若又触发一次 `emit`，内层直接丢。
    delivering: AtomicBool,
    /// 累计投递成功条数（诊断用；不是指标系统）。
    delivered: AtomicU64,
    /// 因重入被丢弃 / 投递失败的条数。
    failed: AtomicU64,
}

impl RuntimeState {
    pub(crate) fn set_mount_ctx(&mut self, ctx: MountContext) {
        self.mount_ctx = Some(ctx);
    }

    pub(crate) fn mount_ctx(&self) -> Option<MountContext> {
        self.mount_ctx.clone()
    }

    /// 是否有任何已挂载模块订阅了事件（**一次原子读**，热路径第一道闸门）。
    pub(crate) fn has_subscribers(&self) -> bool {
        self.interest.any.load(Ordering::Relaxed)
    }

    /// 该事件名当前是否有人订阅（不序列化；热路径第二道闸门）。
    pub(crate) fn wants(&self, name: &str) -> bool {
        self.has_subscribers() && self.interest.wants(name)
    }

    /// 重算兴趣集。`subscriptions` 是「已挂载且已启用」的模块各自声明的
    /// 事件名列表——注册表负责筛出这些模块。
    pub(crate) fn refresh_interest(&mut self, subscriptions: impl Iterator<Item = Vec<String>>) {
        let mut names = HashSet::new();
        let mut all = false;
        for declared in subscriptions {
            for name in declared {
                if name == crate::ALL_EVENTS {
                    all = true;
                } else {
                    names.insert(name);
                }
            }
        }
        let any = all || !names.is_empty();
        self.interest.names = names;
        self.interest.all = all;
        self.interest.any.store(any, Ordering::Relaxed);
    }

    /// 进入投递段；返回 `false` = 已经在投递（内层该被丢弃）。
    pub(crate) fn enter_delivery(&self) -> bool {
        !self.delivering.swap(true, Ordering::SeqCst)
    }

    /// 离开投递段。
    pub(crate) fn leave_delivery(&self) {
        self.delivering.store(false, Ordering::SeqCst);
    }

    pub(crate) fn record_delivered(&self, count: usize) {
        if count > 0 {
            self.delivered.fetch_add(count as u64, Ordering::Relaxed);
        }
    }

    pub(crate) fn record_failed(&self) {
        self.failed.fetch_add(1, Ordering::Relaxed);
    }

    /// `(投递成功条数, 丢弃/失败条数)`。
    pub(crate) fn counters(&self) -> (u64, u64) {
        (
            self.delivered.load(Ordering::Relaxed),
            self.failed.load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 兴趣集：`*` 让所有名字都命中；精确名字只命中自己。
    #[test]
    fn interest_matches_exact_names_and_wildcards() {
        let mut runtime = RuntimeState::default();
        runtime.refresh_interest(vec![vec!["tool.done".into()]].into_iter());
        assert!(runtime.has_subscribers());
        assert!(runtime.wants("tool.done"));
        assert!(!runtime.wants("other"));

        runtime.refresh_interest(vec![vec![crate::ALL_EVENTS.to_string()]].into_iter());
        assert!(runtime.wants("anything"));
    }

    /// 无人订阅：`has_subscribers` 为假（热路径一次原子读就返回）。
    #[test]
    fn empty_interest_is_the_fast_path() {
        let runtime = RuntimeState::default();
        assert!(!runtime.has_subscribers());
        assert!(!runtime.wants("x"));
    }

    /// 重入保护：第一次进入成功，内层被拒，离开后恢复。
    #[test]
    fn delivery_is_not_reentrant() {
        let runtime = RuntimeState::default();
        assert!(runtime.enter_delivery(), "第一次进入应当成功");
        assert!(!runtime.enter_delivery(), "内层必须被拒（否则自递归）");
        runtime.leave_delivery();
        assert!(runtime.enter_delivery(), "离开后可以再次进入");
    }

    /// 计数：成功条数按投递量累加，失败按次数累加。
    #[test]
    fn counters_track_deliveries_and_drops() {
        let runtime = RuntimeState::default();
        runtime.record_delivered(3);
        runtime.record_delivered(0); // 不累加
        runtime.record_failed();
        assert_eq!(runtime.counters(), (3, 1));
    }
}
