//! 空库——本 crate 只有 `tests/` 里的仓库级门禁。
//!
//! 为什么单独成一个 crate：门禁审计的是**跨 crate 的仓库属性**（依赖方向、
//! 全局态），属于任何一个具体 crate 都不合适。它刻意零依赖：一旦它依赖了被
//! 审计的 crate，就成不了独立审计。
//!
//! 门禁清单：
//! - `tests/no_core_dependency.rs`：框架不反向依赖宿主（只有 sdk → api 一条边）
//! - `tests/no_global_state.rs`：库代码零进程级单例
//! - `tests/domain_agnostic.rs`：**全部** crate 的生产代码不含应用领域词汇
//!
//! 第三条覆盖每一个 crate——包括 `echo-module-api` / `echo-module-sdk`。它们曾
//! 被长期排除（理由是"协议表面尚未收敛"），代价是一整套聊天平台类型在豁免下
//! 长了出来。现在协议的领域分类改由宿主自定义的 `Contribution::Custom`
//! 承载，这两件也一起纳入审计。
