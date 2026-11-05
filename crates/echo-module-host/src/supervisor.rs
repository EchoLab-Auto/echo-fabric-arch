//! 模块监督：生命周期状态机（握手 / 贡献 / 调用 / 取消 / 排空 / 崩溃重启）。
//!
//! [`ModuleSupervisor::start`] 启动模块并完成握手，返回可 Clone 的
//! [`ModuleHandle`]；监督任务持有连接、分发模块消息，并在崩溃
//! （[`ModuleConnection::recv`] 返回 [`TransportError::Closed`]、且状态非
//! Draining/Stopped）时按 [`RestartPolicy`]（默认 on-failure）自动重启。
//!
//! 关键语义：
//! - **握手**：`Hello{protocol, version, module_id, config}` → `Welcome` 校验
//!   （protocol 一致 / [`compatible`] / module_id 一致）→ `Register`（整体替换）→
//!   `Ready`；5s 内未完成 → `Err(Timeout)`，校验失败 → `Err(Protocol)` 且
//!   state=Failed。
//! - **崩溃**：在途调用全部以 `Error{code:"module_crashed"}` 结束 → backoff
//!   （初始 200ms、×2、上限 2s）重启，最多 5 次；重启期间 state=Pending。
//! - **调用**：`call_id = <module_id>:<自增序号>`；超时（`ctx.deadline_ms`，
//!   缺省 [`DEFAULT_INVOKE_TIMEOUT_MS`]）发 `Cancel` 并返回
//!   `Error{code:"timeout"}`；[`ModuleHandle::cancel`] 为尽力通知（结果仍以
//!   模块回发的 `InvokeResult` 为准，`Error{code:"cancelled"}` 原样透传）。
//! - **排空**：Draining → `Drain{deadline_ms}` → 等通道关闭（或 deadline）→
//!   `connection.shutdown()` → Stopped。
//! - **热替换**（[`ModuleHandle::upgrade`]）：以同一 transport + 新 config 启动
//!   新实例（握手沿用 [`HANDSHAKE_TIMEOUT`]）→ 握手成功后原子切换 `shared.conn`
//!   （invoke 自此走新实例；期间旧实例持续服务，state 保持 Ready）→ 对旧实例发
//!   `Drain{deadline_ms: timeout}` 并等其退出（或 deadline）→ 剩余时间
//!   `shutdown()` 兜底强杀；握手失败则关闭新实例、旧实例不受影响（返回 Err，
//!   可重试 / 回滚）。仅 Ready 态受理，其余状态（含崩溃重启窗口 Pending）
//!   立即 `Err(Protocol("module is not ready for upgrade: <state>"))`，不排队。
//!   监督循环以 `biased` 优先处理控制消息；升级与排空互斥（串行），升级期间
//!   旧连接的消息留在运输层缓冲，由后续循环迭代或旧实例排空流程照常分发
//!   （Emit / Log / 在途 InvokeResult 不丢；切换前已发给旧实例且未在排空期内
//!   应答的在途调用将等到自身超时——已知限制）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::api::{
    compatible, Cancel, Contribution, Drain, Hello, HostCallOutcome, HostCallResult, HostToModule,
    Invoke, InvokeContext, InvokeOutcome, LogLevel, LogRecord, ModuleToHost, PROTOCOL_NAME,
    PROTOCOL_VERSION,
};
use crate::host_service::HostServiceRegistry;
use crate::transport::{ModuleConnection, ModuleSpec, Transport, TransportError};

/// 握手总超时（Hello → Welcome → Register → Ready）。
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// invoke 默认超时（`InvokeContext::deadline_ms` 为 None 时）。
pub const DEFAULT_INVOKE_TIMEOUT_MS: u64 = 600_000;

/// 控制通道容量（drain / upgrade 请求）。
const CONTROL_CAPACITY: usize = 16;

/// 热替换握手失败后，关闭新实例的宽限期（`shutdown` 尽力；超时即强杀）。
const FAILED_INSTANCE_SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

/// 模块生命周期状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleState {
    /// 启动 / 重启中（尚未就绪）。
    Pending,
    /// 握手完成，可接受调用。
    Ready,
    /// 排空中（不再接受新调用）。
    Draining,
    /// 已停止（正常退出 / 已排空）。
    Stopped,
    /// 已失败（握手失败 / 重启预算耗尽等，附原因）。
    Failed(String),
}

impl ModuleState {
    /// 稳定的小写标签（进协议 / 日志；`Failed` 的原因单独取）。
    ///
    /// 用固定字符串而不是 `Debug`：这是**线格式**，面板按它分支渲染，
    /// 不能随 `Debug` 派生变化而静默漂移。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::Draining => "draining",
            Self::Stopped => "stopped",
            Self::Failed(_) => "failed",
        }
    }

    /// `Failed` 携带的原因（其它状态 = None）。
    pub fn error(&self) -> Option<&str> {
        match self {
            Self::Failed(reason) => Some(reason.as_str()),
            _ => None,
        }
    }
}

/// 崩溃重启策略（默认 on-failure：backoff 200ms 起、翻倍、上限 2s、最多 5 次）。
#[derive(Debug, Clone)]
pub struct RestartPolicy {
    /// 首次重启等待。
    pub initial_backoff: Duration,
    /// backoff 上限（翻倍后截断）。
    pub max_backoff: Duration,
    /// 最大重启尝试次数（预算耗尽 → state=Failed）。
    pub max_restarts: u32,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            initial_backoff: Duration::from_millis(200),
            max_backoff: Duration::from_secs(2),
            max_restarts: 5,
        }
    }
}

/// 模块 → 宿主方向的**频率限额**（防失控模块刷屏 / 刷回呼）。
///
/// 与 [`RestartPolicy`]（崩溃重启预算）同层：都是宿主对模块的保护策略，
/// 一份策略作用于本监督者启动的全部模块。
///
/// 只限**投递即忘**的帧与被动的回呼：
/// - `Emit`：超限直接丢弃并计数（事件本就带缓冲封顶，这里防的是 CPU 空转）；
/// - `Log`：超限丢弃（日志是诊断信息，刷屏只会把真问题淹掉）；
/// - `HostCall`：超限**回一条错误回执**而不是丢弃——模块在等回执，丢掉会让它
///   永久挂住；错误文本让它自己看到背压。
///
/// **绝不动** `InvokeResult`：那是宿主在等的调用结果，丢弃 = 调用挂死。
///
/// 缺省值刻意给得宽松（合法模块远够不到，失控循环会被挡住）：事件 2000/s、
/// 回呼 500/s、日志 200/s。`None` = 不限。
#[derive(Debug, Clone)]
pub struct RatePolicy {
    /// 每秒允许的 `Emit` 帧数。
    pub events_per_sec: Option<u32>,
    /// 每秒允许的 `HostCall` 数。
    pub host_calls_per_sec: Option<u32>,
    /// 每秒允许的 `Log` 帧数。
    pub logs_per_sec: Option<u32>,
}

impl Default for RatePolicy {
    fn default() -> Self {
        Self {
            events_per_sec: Some(2000),
            host_calls_per_sec: Some(500),
            logs_per_sec: Some(200),
        }
    }
}

impl RatePolicy {
    /// 全不限（测试 / 需要绝对吞吐的场景）。
    pub fn unlimited() -> Self {
        Self {
            events_per_sec: None,
            host_calls_per_sec: None,
            logs_per_sec: None,
        }
    }
}

/// 三路限额的区分（窗口 + 缺省值 + 日志标签一处定义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RateBudget {
    /// `Emit`（投递即忘）。
    Events,
    /// `HostCall`（模块在等回执 → 超限回错误而不是丢弃）。
    HostCalls,
    /// `Log`（投递即忘）。
    Logs,
}

impl RateBudget {
    fn label(self) -> &'static str {
        match self {
            Self::Events => "events",
            Self::HostCalls => "host_calls",
            Self::Logs => "logs",
        }
    }
}

/// 秒级滑动窗口计数器（够用且无依赖：三个 u32 + 一个窗口起点）。
///
/// 为什么不用令牌桶：限额是**粗粒度保护**，秒级窗口的边界突刺（最多 2×）
/// 对"防失控"完全够用；令牌桶要维护充电速率与容量两个参数，配置面更复杂。
#[derive(Debug, Default)]
struct Window {
    /// 窗口起点（`Instant`；None = 尚未开始）。
    started: Option<std::time::Instant>,
    count: u32,
}

impl Window {
    /// 记一次并返回是否放行。
    fn allow(&mut self, limit: Option<u32>) -> bool {
        let Some(limit) = limit else {
            return true;
        };
        let now = std::time::Instant::now();
        match self.started {
            Some(start) if now.duration_since(start) < Duration::from_secs(1) => {}
            _ => {
                self.started = Some(now);
                self.count = 0;
            }
        }
        self.count = self.count.saturating_add(1);
        self.count <= limit
    }
}

/// 一个模块的三路窗口计数 + 超限计数（观测用）。
#[derive(Debug, Default)]
struct RateMeters {
    events: Window,
    host_calls: Window,
    logs: Window,
    /// 累计被限流的帧数（与窗口无关；面板/日志读它判断"这个模块在刷"）。
    limited: u64,
}

/// 模块监督者（Clone / Arc 共享；`start` 可并发启动多个模块实例）。
#[derive(Clone, Default)]
pub struct ModuleSupervisor {
    policy: RestartPolicy,
    /// 模块 → 宿主方向的频率限额（[`RatePolicy`]）。
    rate: RatePolicy,
    /// 宿主服务注册表（P2 HostCall）；`new()` 时创建，Clone 共享同一注册表。
    host_services: Arc<HostServiceRegistry>,
}

impl ModuleSupervisor {
    pub fn new() -> Self {
        Self::default()
    }

    /// 宿主服务注册表（供外部在模块启动前后注册 [`crate::host_service::HostService`]）。
    pub fn host_services(&self) -> Arc<HostServiceRegistry> {
        Arc::clone(&self.host_services)
    }

    /// 覆盖默认重启策略。
    pub fn with_restart_policy(mut self, policy: RestartPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// 覆盖默认频率限额（`RatePolicy::unlimited()` = 完全不限）。
    pub fn with_rate_policy(mut self, rate: RatePolicy) -> Self {
        self.rate = rate;
        self
    }

    /// 启动模块：spawn 监督任务并等待握手完成。
    ///
    /// 握手 = `Hello` → `Welcome`（校验 protocol / version / module_id）→
    /// `Register`（存贡献）→ `Ready`。5s 内未完成返回
    /// [`TransportError::Timeout`]；校验失败返回 [`TransportError::Protocol`]
    /// （此时模块状态为 [`ModuleState::Failed`]）。
    ///
    /// 注：与 P0 骨架中的签名相比这里是 `async fn`——握手必须等待模块应答
    /// （`Transport::start` 本身也是 async），同步签名无法在返回前得到
    /// Welcome 的校验结果。
    pub async fn start(
        &self,
        spec: ModuleSpec,
        transport: Box<dyn Transport>,
    ) -> Result<ModuleHandle, TransportError> {
        let (ctl_tx, ctl_rx) = mpsc::channel(CONTROL_CAPACITY);
        let shared = Arc::new(Shared::new(
            spec.module_id.clone(),
            self.policy.clone(),
            ctl_tx,
            self.host_services.clone(),
            self.rate.clone(),
        ));
        let (init_tx, init_rx) = oneshot::channel();
        let task_shared = shared.clone();
        tokio::spawn(async move {
            supervise(task_shared, transport, spec, ctl_rx, init_tx).await;
        });
        match init_rx.await {
            Ok(Ok(())) => Ok(ModuleHandle { shared }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(TransportError::Io(
                "supervisor task ended before handshake completed".to_string(),
            )),
        }
    }
}

/// 单个模块实例的句柄（Clone；与监督任务共享状态）。
#[derive(Clone)]
pub struct ModuleHandle {
    shared: Arc<Shared>,
}

impl ModuleHandle {
    /// 模块实例 id。
    pub fn module_id(&self) -> &str {
        &self.shared.module_id
    }

    /// 当前状态快照。
    pub fn state(&self) -> ModuleState {
        self.shared.state()
    }

    /// 最近一次 `Register` 的贡献集合（整体替换语义）。
    pub fn contributions(&self) -> Vec<Contribution> {
        self.shared.contributions()
    }

    /// 已发生的重启次数（按重启尝试计数，含成功与失败尝试）。
    pub fn restarts(&self) -> u32 {
        self.shared.restarts.load(Ordering::SeqCst)
    }

    /// 自最近一次启动（[`ModuleSupervisor::start`]）以来完成的热替换次数。
    ///
    /// 仅 [`ModuleHandle::upgrade`] 成功原子切换后 +1；崩溃自动重启不计入
    /// （见 [`ModuleHandle::restarts`]）。
    pub fn hot_replaces(&self) -> u32 {
        self.shared.hot_replaces.load(Ordering::SeqCst)
    }

    /// 累计被[频率限额](RatePolicy)挡下的帧数（`Emit` / `Log` 丢弃 +
    /// `HostCall` 拒绝）。0 = 该模块一直守规矩。
    pub fn rate_limited(&self) -> u64 {
        self.shared.rate_limited()
    }

    /// 连接代际（epoch；泛化 01a §1.3）：每次新连接就绪即自增
    /// （首次启动 / 崩溃重启 / 热替换换代）；宿主侧管理面缓存读时按它
    /// 惰性失效（见 `RemoteInstance::management_state`）。
    pub fn generation(&self) -> u64 {
        self.shared.generation.load(Ordering::SeqCst)
    }

    /// 排空：state=Draining → 发 `Drain{deadline_ms}` → 等待模块通道关闭
    /// （或 deadline）→ `connection.shutdown()` → state=Stopped。
    ///
    /// 已 Stopped / Failed（监督任务已退出）时直接 `Ok(())`（幂等）。
    /// 若 shutdown 超时，仍置 Stopped 并返回 `Err(Timeout)`。
    pub async fn drain(&self, deadline: Duration) -> Result<(), TransportError> {
        match self.shared.state() {
            ModuleState::Stopped | ModuleState::Failed(_) => return Ok(()),
            _ => {}
        }
        let (ack_tx, ack_rx) = oneshot::channel();
        if self
            .shared
            .ctl
            .send(Control::Drain {
                deadline,
                ack: ack_tx,
            })
            .await
            .is_err()
        {
            // 监督任务已退出（正常路径：已 Stopped / Failed）：视为完成。
            if !matches!(
                self.shared.state(),
                ModuleState::Failed(_) | ModuleState::Stopped
            ) {
                self.shared.set_state(ModuleState::Stopped);
            }
            return Ok(());
        }
        match ack_rx.await {
            Ok(res) => res,
            // 监督任务在应答前退出（不应发生）：按已停止处理。
            Err(_) => Ok(()),
        }
    }

    /// **停止但保留可重启**：排空连接后进入 `Stopped`，监督任务继续存活，
    /// 之后可用 [`ModuleHandle::start`] 重新拉起。
    ///
    /// 与 [`ModuleHandle::drain`] 的差别只有一条：drain 是终态（监督任务
    /// 退出，句柄再也不能拉起），stop 是可逆的——面板的启停开关用它，
    /// 因为「关掉一个外部模块」之后还要能「再打开」。
    ///
    /// 已是 `Stopped` 时幂等返回 `Ok(())`；`Failed` 时也允许 stop
    /// （把它从失败态收敛成干净的停止态，便于随后 start 重试）。
    pub async fn stop(&self, deadline: Duration) -> Result<(), TransportError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self
            .shared
            .ctl
            .send(Control::Stop {
                deadline,
                ack: ack_tx,
            })
            .await
            .is_err()
        {
            // 监督任务已不在（终态 Drain 之后）：视为已停止。
            return Ok(());
        }
        match ack_rx.await {
            Ok(res) => res,
            Err(_) => Ok(()),
        }
    }

    /// 从 `Stopped` 重新拉起（[`ModuleHandle::stop`] 的逆操作）。
    ///
    /// 仅 `Stopped` 受理；其它状态返回 `Protocol("module is not stopped: …")`
    /// ——`Ready` 想换实现请用 [`ModuleHandle::upgrade`]（保留旧实例直到新实例
    /// 就绪），`Failed` 请先 `stop` 收敛再 `start`。
    ///
    /// 握手沿用首次启动的 5s 上限与同一份 `ModuleSpec`（配置要换用 upgrade）。
    pub async fn start(&self) -> Result<(), TransportError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self
            .shared
            .ctl
            .send(Control::Start { ack: ack_tx })
            .await
            .is_err()
        {
            return Err(TransportError::Io(
                "module supervisor is gone; cannot start".to_string(),
            ));
        }
        match ack_rx.await {
            Ok(res) => res,
            Err(_) => Err(TransportError::Io(
                "module supervisor dropped the start request".to_string(),
            )),
        }
    }

    /// 热替换：用新配置启动一个新进程实例，握手成功后原子切换流量，
    /// 然后优雅排空旧实例；失败则保留旧实例继续服务（可回滚）。
    ///
    /// 语义：
    /// - 仅 [`ModuleState::Ready`] 受理；Stopped / Failed / Draining / Pending
    ///   （含崩溃重启窗口）立即返回
    ///   `Err(Protocol("module is not ready for upgrade: <state>"))`，**不排队**；
    /// - 新实例经同一 transport 以 `ModuleSpec { module_id: 原 id, config }`
    ///   启动，握手沿用 5s 上限；此间旧实例持续服务（state 保持 Ready）；
    /// - 握手失败 → 关闭新实例（`shutdown` 尽力）、旧实例与贡献集合不受影响，
    ///   返回 Err（可重试 / 回滚）；
    /// - 握手成功 → 原子切换（此后 `invoke` 走新实例）→ 对旧实例发
    ///   `Drain{deadline_ms: timeout}` 并等其退出（或 deadline）→
    ///   `shutdown` 兜底强杀；返回 Ok 前旧实例已被排空 / 强杀。
    ///   旧实例排空期间的 Emit / Log / 在途 InvokeResult 仍照常分发；
    ///   切换前已发给旧实例、且旧实例未在排空期内应答的在途调用不在本次
    ///   升级的等待范围内（各自按调用超时兜底——已知限制）。
    /// - 成功 +1 [`ModuleHandle::hot_replaces`]；旧实例强杀兜底不影响成功结果。
    ///
    /// `timeout` 是旧实例的优雅排空期限（`Drain.deadline_ms` 与其后兜底
    /// `shutdown` 的预算）；新实例握手使用固定 [`HANDSHAKE_TIMEOUT`]。
    /// 监督任务已退出（如并发 drain 先完成）时返回非就绪错误。
    /// 并发 `upgrade` / `drain` 在监督循环内串行执行，按到达顺序处理。
    pub async fn upgrade(&self, config: Value, timeout: Duration) -> Result<(), TransportError> {
        self.upgrade_inner(config, timeout, None).await
    }

    async fn upgrade_inner(
        &self,
        config: Value,
        timeout: Duration,
        expected_contributions: Option<Vec<Contribution>>,
    ) -> Result<(), TransportError> {
        let state = self.shared.state();
        if state != ModuleState::Ready {
            return Err(not_ready_error(&state));
        }
        let (ack_tx, ack_rx) = oneshot::channel();
        if self
            .shared
            .ctl
            .send(Control::Upgrade {
                config,
                timeout,
                expected_contributions,
                ack: ack_tx,
            })
            .await
            .is_err()
        {
            return Err(self.upgrade_unavailable_error());
        }
        match ack_rx.await {
            Ok(res) => res,
            Err(_) => Err(self.upgrade_unavailable_error()),
        }
    }

    /// 热替换，但要求新实例的贡献集合与当前完全一致。
    ///
    /// 宿主若在启动期把贡献物化成注册表条目（工具/schema/实例描述），
    /// 贡献变化就必须走宿主级重装配，不能只换子进程。本方法在新实例
    /// 握手后、流量切换前 fail-closed；不兼容时旧实例持续服务。
    pub async fn upgrade_preserving_contributions(
        &self,
        config: Value,
        timeout: Duration,
    ) -> Result<(), TransportError> {
        self.upgrade_inner(config, timeout, Some(self.contributions()))
            .await
    }

    /// 监督任务已退出 / 竞态下的非就绪错误（[`ModuleHandle::upgrade`] 用）。
    fn upgrade_unavailable_error(&self) -> TransportError {
        let state = self.shared.state();
        if state == ModuleState::Ready {
            TransportError::Io("supervisor task ended before upgrade completed".to_string())
        } else {
            not_ready_error(&state)
        }
    }

    /// 调用一个贡献。
    ///
    /// - `call_id = <module_id>:<自增序号>`（从 1 开始），登记在途表等待
    ///   `InvokeResult`；
    /// - 超时 = `ctx.deadline_ms` 或默认 [`DEFAULT_INVOKE_TIMEOUT_MS`]；超时后
    ///   发送 `Cancel` 并返回 `Error{code:"timeout"}`；
    /// - 模块回发的任何结果（含 `Error{code:"cancelled"}`）原样返回；
    /// - 模块不可用时的错误码：`stopped` / `failed` / `draining` / `not_ready` /
    ///   `module_crashed` / `transport_error`。
    pub async fn invoke(
        &self,
        contribution: &str,
        ctx: InvokeContext,
        payload: Value,
    ) -> InvokeOutcome {
        self.shared.invoke(contribution, ctx, payload).await
    }

    /// 反复 invoke 同一个贡献，直到谓词返回 `true` 或超时。
    ///
    /// 「握手完成」不等于「模块内部的服务真的起来了」：`state() == Ready` 只说明
    /// 协议握手结束，而宿主常常还要等它把 HTTP 端口绑上、把数据加载完。这类
    /// 就绪探测每个宿主都会写一遍（轮询 + 超时 + 判据），所以收在这里。
    ///
    /// 返回第一个满足 `predicate` 的 `result` 载荷；超时 / 模块进入
    /// `Failed`·`Stopped` / 一直返回 `Error` 都返回 `None`。
    ///
    /// ```no_run
    /// # use std::time::Duration;
    /// # use serde_json::Value;
    /// # async fn demo(handle: &echo_module_host::ModuleHandle) {
    /// let ready = handle
    ///     .wait_until(
    ///         "my.panel",
    ///         Default::default(),
    ///         Value::Null,
    ///         Duration::from_secs(5),
    ///         |r| r.get("serving").and_then(Value::as_bool) == Some(true),
    ///     )
    ///     .await;
    /// assert!(ready.is_some(), "面板 5 秒内没说自己开始服务");
    /// # }
    /// ```
    pub async fn wait_until<P>(
        &self,
        contribution: &str,
        ctx: InvokeContext,
        payload: Value,
        timeout: Duration,
        mut predicate: P,
    ) -> Option<Value>
    where
        P: FnMut(&Value) -> bool,
    {
        const POLL: Duration = Duration::from_millis(200);
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            // 模块已经不可能再就绪了就别等满超时。
            match self.state() {
                ModuleState::Failed(_) | ModuleState::Stopped => return None,
                _ => {}
            }
            if let InvokeOutcome::Ok { result, .. } = self
                .invoke(contribution, ctx.clone(), payload.clone())
                .await
            {
                if predicate(&result) {
                    return Some(result);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(POLL.min(timeout)).await;
        }
    }

    /// 取消在途调用：在途表标记 + 尽力发送 `Cancel`（fire-and-forget）。
    ///
    /// 调用结果仍以模块回发的 `InvokeResult` 为准（`cancelled` 原样透传）；
    /// 模块无响应时由 [`invoke`](ModuleHandle::invoke) 的超时兜底。
    pub fn cancel(&self, call_id: &str) {
        self.shared.cancel(call_id);
    }

    /// 取走累计的模块事件（`Emit`），返回后队列清空。
    pub fn take_events(&self) -> Vec<(String, Value)> {
        self.shared.take_events()
    }
}

/// 模块事件（`Emit`）缓冲上限。
///
/// 事件当前**无生产消费方**（仅测试经 `take_events` 取用）——不封顶则
/// 模块持续 `Emit` 会无界堆积。封顶后丢新事件并计数（保留先到语义，
/// 避免 Vec 头部移除的 O(n) 搬移）；接线消费方后可评估放宽。
const MAX_PENDING_EVENTS: usize = 1024;

/// 监督任务与句柄共享的状态。
struct Shared {
    module_id: String,
    policy: RestartPolicy,
    /// 频率限额 + 三路窗口计数（[`RateMeters`]）。
    rate: RatePolicy,
    rate_meters: Mutex<RateMeters>,
    state: Mutex<ModuleState>,
    contributions: Mutex<Vec<Contribution>>,
    restarts: AtomicU32,
    /// 完成的热替换次数（仅成功原子切换计数；崩溃重启不计入）。
    hot_replaces: AtomicU32,
    /// 连接代际（epoch；泛化 01a §1.3）：每次新连接就绪（首次启动 /
    /// 崩溃重启 / 热替换换代）即自增；宿主侧管理面缓存按它惰性失效。
    generation: AtomicU64,
    /// 调用序号（`call_id` 后缀，从 1 开始）。
    seq: AtomicU64,
    pending: Mutex<HashMap<String, PendingCall>>,
    events: Mutex<Vec<(String, Value)>>,
    /// 事件缓冲满后丢弃的事件数（封顶保护的可观测性）。
    dropped_events: AtomicU64,
    /// 当前连接（None = 未启动 / 重启中 / 已停）。
    conn: Mutex<Option<Arc<dyn ModuleConnection>>>,
    /// 控制通道（drain / upgrade 请求）。
    ctl: mpsc::Sender<Control>,
    /// 宿主服务注册表（P2 HostCall 回呼路由）。
    host_services: Arc<HostServiceRegistry>,
}

/// 在途调用。
struct PendingCall {
    result_tx: oneshot::Sender<InvokeOutcome>,
    /// 是否收到过 [`ModuleHandle::cancel`] 标记（仅记录；结果仍以模块回发为准）。
    cancel_requested: bool,
}

/// 监督控制命令。
enum Control {
    /// 排空请求；`ack` 携带最终结果。
    Drain {
        deadline: Duration,
        ack: oneshot::Sender<Result<(), TransportError>>,
    },
    /// 热替换请求；`ack` 携带最终结果（见 [`ModuleHandle::upgrade`]）。
    Upgrade {
        config: Value,
        timeout: Duration,
        expected_contributions: Option<Vec<Contribution>>,
        ack: oneshot::Sender<Result<(), TransportError>>,
    },
    /// **停止但保留监督任务**（可再 [`Control::Start`]）：排空后进入
    /// `Stopped`，监督循环继续等命令而不是退出。
    ///
    /// 与 [`Control::Drain`] 的差别就是这一条：Drain 是终态（任务退出），
    /// Stop 是「可逆的停」，用于面板的启停开关真正停掉子进程。
    Stop {
        deadline: Duration,
        ack: oneshot::Sender<Result<(), TransportError>>,
    },
    /// 从 `Stopped` 重新握手拉起（[`Control::Stop`] 的逆操作）。
    Start {
        ack: oneshot::Sender<Result<(), TransportError>>,
    },
}

/// `select!` 输出（避免在分支 future 存活期间借用 `ctl_rx`）。
enum Step {
    Msg(Result<ModuleToHost, TransportError>),
    Ctl(Option<Control>),
}

/// 重启流程结果。
enum Restart {
    /// 重启成功，新连接已就绪。
    Restarted,
    /// 重启等待期间收到排空请求并已完成。
    Drained,
    /// 重启等待期间收到**可逆停止**请求（`Control::Stop`）：已进 `Stopped`，
    /// 监督任务仍在，等后续 `Control::Start`。
    Stopped,
    /// 放弃（预算耗尽 / 控制端消失）。
    Dead,
}

impl Shared {
    fn new(
        module_id: String,
        policy: RestartPolicy,
        ctl: mpsc::Sender<Control>,
        host_services: Arc<HostServiceRegistry>,
        rate: RatePolicy,
    ) -> Self {
        Self {
            module_id,
            policy,
            rate,
            rate_meters: Mutex::new(RateMeters::default()),
            state: Mutex::new(ModuleState::Pending),
            contributions: Mutex::new(Vec::new()),
            restarts: AtomicU32::new(0),
            hot_replaces: AtomicU32::new(0),
            generation: AtomicU64::new(0),
            seq: AtomicU64::new(0),
            pending: Mutex::new(HashMap::new()),
            events: Mutex::new(Vec::new()),
            dropped_events: AtomicU64::new(0),
            conn: Mutex::new(None),
            ctl,
            host_services,
        }
    }

    fn state(&self) -> ModuleState {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set_state(&self, state: ModuleState) {
        *self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = state;
    }

    fn contributions(&self) -> Vec<Contribution> {
        self.contributions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set_contributions(&self, list: Vec<Contribution>) {
        tracing::info!(module = %self.module_id, count = list.len(), "module registered contributions");
        *self
            .contributions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = list;
    }

    /// 频率限额：`kind` 走哪一路窗口。返回是否放行；超限时累计计数并按
    /// 「首次 + 每 1024 次」warn 一次（防日志刷屏本身变成新的刷屏源）。
    fn allow(&self, kind: RateBudget) -> bool {
        let limit = match kind {
            RateBudget::Events => self.rate.events_per_sec,
            RateBudget::HostCalls => self.rate.host_calls_per_sec,
            RateBudget::Logs => self.rate.logs_per_sec,
        };
        let mut meters = self
            .rate_meters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let window = match kind {
            RateBudget::Events => &mut meters.events,
            RateBudget::HostCalls => &mut meters.host_calls,
            RateBudget::Logs => &mut meters.logs,
        };
        if window.allow(limit) {
            return true;
        }
        meters.limited = meters.limited.saturating_add(1);
        let limited = meters.limited;
        drop(meters);
        if limited == 1 || limited.is_multiple_of(1024) {
            tracing::warn!(
                module = %self.module_id,
                budget = kind.label(),
                limit_per_sec = ?limit,
                limited,
                "module exceeded its rate budget; dropping (events/logs) or rejecting (host calls)"
            );
        }
        false
    }

    /// 累计被限流的帧数（观测 / 测试）。
    fn rate_limited(&self) -> u64 {
        self.rate_meters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .limited
    }

    fn push_event(&self, event: String, payload: Value) {
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if events.len() >= MAX_PENDING_EVENTS {
            let dropped = self.dropped_events.fetch_add(1, Ordering::Relaxed) + 1;
            // 首次与每满 1024 次告警一次（防刷屏）；计数不受日志节流影响。
            if dropped == 1 || dropped.is_multiple_of(1024) {
                tracing::warn!(
                    module = %self.module_id,
                    dropped,
                    "module event buffer full ({MAX_PENDING_EVENTS}) — dropping events"
                );
            }
            return;
        }
        events.push((event, payload));
    }

    fn take_events(&self) -> Vec<(String, Value)> {
        std::mem::take(
            &mut *self
                .events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    fn current_conn(&self) -> Option<Arc<dyn ModuleConnection>> {
        self.conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// 替换当前连接；新连接就绪（`Some`）= 新一代：`generation` 自增
    /// （泛化 01a §1.3——首次启动 / 崩溃重启 / 热替换三处均经本函数）。
    fn set_conn(&self, conn: Option<Arc<dyn ModuleConnection>>) {
        let live = conn.is_some();
        *self
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = conn;
        if live {
            self.generation.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn insert_pending(&self, call_id: String, call: PendingCall) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(call_id, call);
    }

    fn remove_pending(&self, call_id: &str) -> Option<PendingCall> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(call_id)
    }

    /// 崩溃：全部在途调用以 `module_crashed` 结束。
    fn fail_pending_calls(&self, reason: &str) {
        let drained: Vec<(String, PendingCall)> = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain()
            .collect();
        for (call_id, call) in drained {
            let _ = call.result_tx.send(InvokeOutcome::Error {
                code: "module_crashed".to_string(),
                message: format!("{reason} while call {call_id} was in flight"),
            });
        }
    }

    async fn invoke(
        &self,
        contribution: &str,
        ctx: InvokeContext,
        payload: Value,
    ) -> InvokeOutcome {
        match self.state() {
            ModuleState::Stopped => return outcome_error("stopped", "module is stopped"),
            ModuleState::Failed(reason) => {
                return outcome_error("failed", format!("module failed: {reason}"))
            }
            ModuleState::Draining => return outcome_error("draining", "module is draining"),
            _ => {}
        }
        let Some(conn) = self.current_conn() else {
            return outcome_error("not_ready", "module is not ready (starting or restarting)");
        };

        let call_id = format!(
            "{}:{}",
            self.module_id,
            self.seq.fetch_add(1, Ordering::SeqCst) + 1
        );
        let (result_tx, result_rx) = oneshot::channel();
        self.insert_pending(
            call_id.clone(),
            PendingCall {
                result_tx,
                cancel_requested: false,
            },
        );

        let msg = HostToModule::Invoke(Invoke {
            call_id: call_id.clone(),
            contribution: contribution.to_string(),
            ctx: ctx.clone(),
            payload,
        });
        if let Err(e) = conn.send(msg).await {
            self.remove_pending(&call_id);
            let code = if matches!(e, TransportError::Closed) {
                "module_crashed"
            } else {
                "transport_error"
            };
            return outcome_error(code, format!("failed to send invoke: {e}"));
        }

        let deadline_ms = ctx.deadline_ms.unwrap_or(DEFAULT_INVOKE_TIMEOUT_MS);
        match tokio::time::timeout(Duration::from_millis(deadline_ms), result_rx).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => {
                // 结果通道被提前丢弃（不应发生；防御性处理）。
                self.remove_pending(&call_id);
                outcome_error("module_crashed", "invoke result channel closed")
            }
            Err(_) => {
                // 超时：先摘除在途表，再尽力通知对端取消。
                self.remove_pending(&call_id);
                if let Err(e) = conn
                    .send(HostToModule::Cancel(Cancel {
                        call_id: call_id.clone(),
                    }))
                    .await
                {
                    tracing::debug!(module = %self.module_id, call_id = %call_id, error = %e, "failed to deliver timeout cancel");
                }
                outcome_error("timeout", format!("invoke timed out after {deadline_ms}ms"))
            }
        }
    }

    fn cancel(&self, call_id: &str) {
        let marked = match self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(call_id)
        {
            Some(call) => {
                call.cancel_requested = true;
                true
            }
            None => false,
        };
        if !marked {
            tracing::debug!(module = %self.module_id, call_id = %call_id, "cancel for unknown/finished call");
        }
        if let Some(conn) = self.current_conn() {
            let call_id = call_id.to_string();
            let module_id = self.module_id.clone();
            // fire-and-forget：尽力送达（结果仍以模块回发为准）。
            tokio::spawn(async move {
                if let Err(e) = conn
                    .send(HostToModule::Cancel(Cancel {
                        call_id: call_id.clone(),
                    }))
                    .await
                {
                    tracing::debug!(module = %module_id, call_id = %call_id, error = %e, "failed to deliver cancel");
                }
            });
        }
    }
}

fn outcome_error(code: &str, message: impl Into<String>) -> InvokeOutcome {
    InvokeOutcome::Error {
        code: code.to_string(),
        message: message.into(),
    }
}

/// 监督任务主体：握手 → 消息循环（含崩溃重启 / 排空 / 热替换）。
///
/// 循环每轮从 `shared` 取**当前**连接再 `select!`（`biased`：控制消息优先），
/// 因此热替换切换连接后自动观察新连接。升级 / 排空在循环内串行执行（互斥）：
/// 升级执行期间（含旧实例排空）不处理其它控制请求，旧连接的消息留在运输层
/// 缓冲（stdio 管道 / inproc 通道），由后续循环迭代或旧实例排空流程分发，
/// 不丢消息。
async fn supervise(
    shared: Arc<Shared>,
    transport: Box<dyn Transport>,
    spec: ModuleSpec,
    mut ctl_rx: mpsc::Receiver<Control>,
    init_tx: oneshot::Sender<Result<(), TransportError>>,
) {
    match start_and_handshake(&shared, transport.as_ref(), &spec).await {
        Ok((conn, contributions)) => {
            if let Some(list) = contributions {
                shared.set_contributions(list);
            }
            shared.set_conn(Some(conn));
            shared.set_state(ModuleState::Ready);
            tracing::info!(module = %shared.module_id, "module ready");
            let _ = init_tx.send(Ok(()));
        }
        Err(e) => {
            tracing::error!(module = %shared.module_id, error = %e, "module handshake failed");
            shared.set_state(ModuleState::Failed(e.to_string()));
            let _ = init_tx.send(Err(e));
            return;
        }
    }

    loop {
        // ── 阶段 A：无连接（Stopped）→ 等 Start / 终态 Drain ──
        //
        // 首轮必定有连接（上面的握手刚成功）；进入这里意味着上一轮是
        // `Control::Stop` 或重启期间被 Stop。**监督任务在这里继续存活**——
        // 这正是 Stop 与 Drain 的语义差别。
        if shared.current_conn().is_none() {
            match await_start_command(&shared, transport.as_ref(), &spec, &mut ctl_rx).await {
                StartPhase::Started => {}
                StartPhase::Finished => return,
                StartPhase::StartFailed(error) => {
                    // 留在 Failed，继续等下一次 Start（面板可再点一次启用）。
                    tracing::warn!(module = %shared.module_id, %error, "explicit start failed");
                    continue;
                }
            }
        }
        let Some(conn) = shared.current_conn() else {
            // 不应发生（阶段 A 出来必有连接）；防御性收尾。
            shared.set_state(ModuleState::Stopped);
            return;
        };
        let step = tokio::select! {
            // 控制消息优先（`biased`）：Drain / Upgrade 先于连接消息处理；
            // 尚未处理的连接消息留在运输层缓冲，由后续循环迭代或排空流程
            // 分发，不会丢失。
            biased;
            ctl = ctl_rx.recv() => Step::Ctl(ctl),
            msg = conn.recv() => Step::Msg(msg),
        };
        match step {
            Step::Msg(Ok(msg)) => dispatch(&shared, &conn, msg),
            Step::Msg(Err(e)) => {
                tracing::warn!(module = %shared.module_id, error = %e, "module connection lost; treating as crash");
                shared.fail_pending_calls("module connection lost");
                match restart_after_crash(&shared, transport.as_ref(), &spec, &mut ctl_rx).await {
                    Restart::Restarted => {}
                    // 可逆停止：监督任务留着，回阶段 A 等 Start。
                    Restart::Stopped => continue,
                    Restart::Drained | Restart::Dead => return,
                }
            }
            Step::Ctl(Some(Control::Drain { deadline, ack })) => {
                shared.set_state(ModuleState::Draining);
                let res = drain_connection(&shared, deadline).await;
                shared.set_conn(None);
                shared.set_state(ModuleState::Stopped);
                tracing::info!(module = %shared.module_id, "module stopped (drained)");
                let _ = ack.send(res);
                return;
            }
            Step::Ctl(Some(Control::Stop { deadline, ack })) => {
                // 可逆停止：排空后**留在循环里**等 Start（Drain 才退出）。
                shared.set_state(ModuleState::Draining);
                let res = drain_connection(&shared, deadline).await;
                shared.set_conn(None);
                shared.set_state(ModuleState::Stopped);
                tracing::info!(module = %shared.module_id, "module stopped (restartable)");
                let _ = ack.send(res);
                continue;
            }
            Step::Ctl(Some(Control::Start { ack })) => {
                // 已在运行：按「非停止态」拒绝（想换实现用 Upgrade）。
                let _ = ack.send(Err(TransportError::Protocol(format!(
                    "module is not stopped: {}",
                    state_label(&shared.state())
                ))));
            }
            Step::Ctl(Some(Control::Upgrade {
                config,
                timeout,
                expected_contributions,
                ack,
            })) => {
                let res = upgrade_instance(
                    &shared,
                    transport.as_ref(),
                    &spec,
                    config,
                    timeout,
                    expected_contributions,
                )
                .await;
                let _ = ack.send(res);
            }
            Step::Ctl(None) => {
                // 控制端全部消失（防御性分支）：静默排空后退出。
                shared.set_state(ModuleState::Draining);
                let _ = drain_connection(&shared, Duration::from_millis(500)).await;
                shared.set_conn(None);
                shared.set_state(ModuleState::Stopped);
                return;
            }
        }
    }
}

/// 启动 + 握手（首次启动与崩溃重启共用）。
///
/// 返回 `(连接, 新实例注册的贡献)`；`Register` 未到达时贡献为 `None`
/// （调用方保留现有集合）。调用方负责在此之前把状态置为
/// [`ModuleState::Pending`]（首次启动由 [`Shared::new`] 置位；重启由
/// `restart_after_crash` 置位）。
async fn start_and_handshake(
    shared: &Shared,
    transport: &dyn Transport,
    spec: &ModuleSpec,
) -> Result<(Arc<dyn ModuleConnection>, Option<Vec<Contribution>>), TransportError> {
    let conn: Arc<dyn ModuleConnection> = Arc::from(transport.start(spec.clone()).await?);
    let registered = run_handshake(shared, &conn, spec).await?;
    Ok((conn, registered))
}

/// 握手（Hello → Welcome → Register → Ready），总超时 [`HANDSHAKE_TIMEOUT`]。
///
/// 成功返回新实例注册的贡献（`Register` 未到达时为 `None`），**不**写入
/// `shared`——由调用方在握手成功后应用（热替换失败时须保持旧贡献集合）。
/// 握手期间的 `Emit` / `Log` 照常分发。
async fn run_handshake(
    shared: &Shared,
    conn: &Arc<dyn ModuleConnection>,
    spec: &ModuleSpec,
) -> Result<Option<Vec<Contribution>>, TransportError> {
    let module_id = shared.module_id.clone();

    let handshake = async {
        conn.send(HostToModule::Hello(Hello {
            protocol: PROTOCOL_NAME.to_string(),
            version: PROTOCOL_VERSION,
            module_id: module_id.clone(),
            config: spec.config.clone(),
        }))
        .await?;
        let mut welcomed = false;
        let mut registered: Option<Vec<Contribution>> = None;
        loop {
            match conn.recv().await? {
                ModuleToHost::Welcome(w) => {
                    if w.protocol != PROTOCOL_NAME {
                        return Err(TransportError::Protocol(format!(
                            "protocol mismatch: expected {PROTOCOL_NAME:?}, got {:?}",
                            w.protocol
                        )));
                    }
                    if !compatible(PROTOCOL_VERSION, w.version) {
                        return Err(TransportError::Protocol(format!(
                            "incompatible protocol version: host {PROTOCOL_VERSION}, module {}",
                            w.version
                        )));
                    }
                    if w.module_id != module_id {
                        return Err(TransportError::Protocol(format!(
                            "module id mismatch: expected {module_id:?}, got {:?}",
                            w.module_id
                        )));
                    }
                    welcomed = true;
                    tracing::debug!(module = %module_id, version = w.version, capabilities = ?w.capabilities, "module welcomed");
                }
                ModuleToHost::Register(r) => registered = Some(r.contributions),
                ModuleToHost::Ready => {
                    if !welcomed {
                        return Err(TransportError::Protocol(
                            "module sent Ready before Welcome".to_string(),
                        ));
                    }
                    return Ok(registered);
                }
                ModuleToHost::Failed(f) => {
                    return Err(TransportError::Protocol(format!(
                        "module init failed: {} ({})",
                        f.message, f.code
                    )));
                }
                ModuleToHost::Emit(e) => shared.push_event(e.event, e.payload),
                ModuleToHost::Log(l) => log_record(&module_id, &l),
                ModuleToHost::InvokeResult(r) => {
                    tracing::debug!(module = %module_id, call_id = %r.call_id, "ignoring InvokeResult during handshake");
                }
                ModuleToHost::HostCall(call) => {
                    tracing::debug!(module = %module_id, call_id = %call.call_id, "ignoring HostCall during handshake");
                }
            }
        }
    };

    match tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake).await {
        Ok(res) => res,
        Err(_) => Err(TransportError::Timeout),
    }
}

/// 崩溃后的重启循环（backoff 等待可被排空请求打断）。
async fn restart_after_crash(
    shared: &Arc<Shared>,
    transport: &dyn Transport,
    spec: &ModuleSpec,
    ctl_rx: &mut mpsc::Receiver<Control>,
) -> Restart {
    let mut backoff = shared.policy.initial_backoff;
    loop {
        let used = shared.restarts.load(Ordering::SeqCst);
        if used >= shared.policy.max_restarts {
            tracing::error!(module = %shared.module_id, used, "restart budget exhausted; giving up");
            shared.set_state(ModuleState::Failed(format!(
                "restart budget exhausted ({used}/{} restarts) after crash",
                shared.policy.max_restarts
            )));
            return Restart::Dead;
        }
        shared.set_state(ModuleState::Pending);
        let attempt = used + 1;
        tracing::warn!(
            module = %shared.module_id,
            attempt,
            max = shared.policy.max_restarts,
            backoff_ms = backoff.as_millis() as u64,
            "module crashed; scheduling restart"
        );

        // backoff（可被 Drain 打断）。每个分支都直接离开本次等待：
        // sleep 完成 → 继续外层重试循环；Drain/通道关闭 → 返回终态。
        if !backoff.is_zero() {
            let sleep = tokio::time::sleep(backoff);
            tokio::pin!(sleep);
            tokio::select! {
                _ = &mut sleep => {}
                cmd = ctl_rx.recv() => match cmd {
                    Some(Control::Drain { deadline, ack }) => {
                        shared.set_state(ModuleState::Draining);
                        let res = drain_connection(shared, deadline).await;
                        shared.set_conn(None);
                        shared.set_state(ModuleState::Stopped);
                        let _ = ack.send(res);
                        return Restart::Drained;
                    }
                    Some(Control::Stop { deadline, ack }) => {
                        // 崩溃重启窗口内的「关闭」：收敛成干净停止态，
                        // 监督任务继续存活（与 Drain 的终态语义区分）。
                        shared.set_state(ModuleState::Draining);
                        let res = drain_connection(shared, deadline).await;
                        shared.set_conn(None);
                        shared.set_state(ModuleState::Stopped);
                        let _ = ack.send(res);
                        return Restart::Stopped;
                    }
                    Some(Control::Start { ack }) => {
                        // 已在重启中：按「非就绪」拒绝（调用方无需再点一次）。
                        let _ = ack.send(Err(not_ready_error(&shared.state())));
                    }
                    Some(Control::Upgrade { ack, .. }) => {
                        // 重启窗口（Pending）内的升级请求：按「非就绪拒绝」
                        // 语义直接回报错误，不排队等待重启完成。
                        let _ = ack.send(Err(not_ready_error(&shared.state())));
                    }
                    None => {
                        shared.set_state(ModuleState::Stopped);
                        return Restart::Dead;
                    }
                },
            }
        }

        shared.restarts.fetch_add(1, Ordering::SeqCst);
        match start_and_handshake(shared, transport, spec).await {
            Ok((conn, contributions)) => {
                if let Some(list) = contributions {
                    shared.set_contributions(list);
                }
                shared.set_conn(Some(conn));
                shared.set_state(ModuleState::Ready);
                tracing::info!(module = %shared.module_id, attempt, "module restarted");
                return Restart::Restarted;
            }
            Err(e) => {
                tracing::warn!(module = %shared.module_id, attempt, error = %e, "restart attempt failed");
                backoff = (backoff * 2).min(shared.policy.max_backoff);
            }
        }
    }
}

/// 阶段 A 的等待结果（[`await_start_command`]）。
enum StartPhase {
    /// 已重新握手成功（连接就绪，可继续消息循环）。
    Started,
    /// 监督任务该结束了（收到终态 Drain / 控制端消失）。
    Finished,
    /// 本次 start 失败（状态已置 Failed）；监督任务**仍在**，可再等一次。
    StartFailed(TransportError),
}

/// 已停止态：等 `Start`（拉起）或终态 `Drain`（退出）。
///
/// 只处理"无连接"时能收到的命令：
/// - `Start` → 置 Pending → 握手 → Ready（失败置 Failed 并把错误回给调用方）；
/// - `Stop` → 幂等 `Ok(())`（已经是停的，不制造错误）；
/// - `Drain` → 终态收尾后返回 [`StartPhase::Finished`]；
/// - `Upgrade` → 「未运行」错误（想换实现必须先 Start）。
async fn await_start_command(
    shared: &Arc<Shared>,
    transport: &dyn Transport,
    spec: &ModuleSpec,
    ctl_rx: &mut mpsc::Receiver<Control>,
) -> StartPhase {
    loop {
        match ctl_rx.recv().await {
            Some(Control::Start { ack }) => {
                shared.set_state(ModuleState::Pending);
                match start_and_handshake(shared, transport, spec).await {
                    Ok((conn, contributions)) => {
                        if let Some(list) = contributions {
                            shared.set_contributions(list);
                        }
                        shared.set_conn(Some(conn));
                        shared.set_state(ModuleState::Ready);
                        tracing::info!(module = %shared.module_id, "module started");
                        let _ = ack.send(Ok(()));
                        return StartPhase::Started;
                    }
                    Err(error) => {
                        tracing::error!(module = %shared.module_id, %error, "explicit start: handshake failed");
                        shared.set_state(ModuleState::Failed(error.to_string()));
                        // `TransportError` 不实现 Clone：把可读原因回给调用方，
                        // 原始值用于返回值（两者内容一致，调用方读的是字符串）。
                        let _ = ack.send(Err(TransportError::Protocol(format!(
                            "start failed: {error}"
                        ))));
                        return StartPhase::StartFailed(error);
                    }
                }
            }
            Some(Control::Stop { ack, .. }) => {
                // 已经是停止态：幂等成功（面板重复点"关"不该报错）。
                let _ = ack.send(Ok(()));
            }
            Some(Control::Drain { ack, .. }) => {
                shared.set_state(ModuleState::Stopped);
                let _ = ack.send(Ok(()));
                return StartPhase::Finished;
            }
            Some(Control::Upgrade { ack, .. }) => {
                let _ = ack.send(Err(TransportError::Protocol(format!(
                    "module is not running: {}",
                    state_label(&shared.state())
                ))));
            }
            None => {
                shared.set_state(ModuleState::Stopped);
                return StartPhase::Finished;
            }
        }
    }
}

/// 排空：发 Drain → 等通道关闭（或 deadline）→ shutdown。
async fn drain_connection(shared: &Arc<Shared>, deadline: Duration) -> Result<(), TransportError> {
    let Some(conn) = shared.current_conn() else {
        return Ok(()); // 重启中等：无连接可排空
    };
    let started = tokio::time::Instant::now();
    shared.set_state(ModuleState::Draining);

    if let Err(e) = conn
        .send(HostToModule::Drain(Drain {
            deadline_ms: deadline.as_millis() as u64,
        }))
        .await
    {
        tracing::debug!(module = %shared.module_id, error = %e, "failed to deliver Drain");
    }

    // 等待模块主动退出（通道关闭），期间照常分发消息（在途结果 / 事件 / 日志）。
    // 回执路由按来源连接（即本条被排空的连接）——见 `dispatch` 的 W1.2 说明。
    let wait = async {
        while let Ok(msg) = conn.recv().await {
            dispatch(shared, &conn, msg);
        }
    };
    let remaining = deadline.saturating_sub(started.elapsed());
    let _ = tokio::time::timeout(remaining, wait).await;

    let remaining = deadline
        .saturating_sub(started.elapsed())
        .max(Duration::from_millis(1));
    let res = conn.shutdown(remaining).await;
    shared.set_conn(None);
    res
}

/// 热替换核心（在监督任务内串行执行，独占监督循环——升级与排空互斥）。
///
/// 流程见 [`ModuleHandle::upgrade`] 文档；要点：
/// - 仅 Ready 态受理，其余状态返回 `Protocol("module is not ready for upgrade: …")`；
/// - 新实例握手期间 state 保持 Ready（旧实例继续服务）；
/// - 握手失败：关闭新实例（`shutdown` 尽力）、旧实例与贡献集合均不受影响；
/// - 握手成功：原子切换 `shared.conn`（invoke 自此走新实例）→ 应用新注册的
///   贡献 → `hot_replaces` +1 → 排空旧实例
///   （`Drain{deadline_ms: timeout}` → 等其退出 → `shutdown` 兜底强杀）。
async fn upgrade_instance(
    shared: &Arc<Shared>,
    transport: &dyn Transport,
    spec: &ModuleSpec,
    config: Value,
    timeout: Duration,
    expected_contributions: Option<Vec<Contribution>>,
) -> Result<(), TransportError> {
    let state = shared.state();
    if state != ModuleState::Ready {
        return Err(not_ready_error(&state));
    }
    let Some(old_conn) = shared.current_conn() else {
        // 防御性：Ready 态必有连接。
        return Err(not_ready_error(&ModuleState::Pending));
    };

    let new_spec = ModuleSpec {
        module_id: spec.module_id.clone(),
        config,
    };
    tracing::info!(module = %shared.module_id, "upgrade: starting replacement instance");
    let new_conn: Arc<dyn ModuleConnection> = match transport.start(new_spec.clone()).await {
        Ok(conn) => Arc::from(conn),
        Err(e) => {
            tracing::warn!(
                module = %shared.module_id,
                error = %e,
                "upgrade: failed to start replacement; keeping current instance"
            );
            return Err(e);
        }
    };
    let contributions = match run_handshake(shared, &new_conn, &new_spec).await {
        Ok(registered) => registered,
        Err(e) => {
            tracing::warn!(
                module = %shared.module_id,
                error = %e,
                "upgrade: replacement handshake failed; keeping current instance"
            );
            // 关闭新实例（尽力；超时即强杀），旧实例不受影响。
            if new_conn
                .shutdown(FAILED_INSTANCE_SHUTDOWN_GRACE)
                .await
                .is_err()
            {
                tracing::warn!(
                    module = %shared.module_id,
                    "upgrade: replacement instance did not exit gracefully after failed handshake"
                );
            }
            return Err(e);
        }
    };

    if let (Some(expected), Some(actual)) = (&expected_contributions, &contributions) {
        if expected != actual {
            tracing::warn!(
                module = %shared.module_id,
                "upgrade: replacement contributions changed; keeping current instance"
            );
            let _ = new_conn.shutdown(FAILED_INSTANCE_SHUTDOWN_GRACE).await;
            return Err(TransportError::Protocol(
                "replacement contributions differ; host-level reassembly is required".into(),
            ));
        }
    }

    // 原子切换：invoke 自此走新实例；贡献集合同步为新实例注册
    // （新实例未发 Register 时保留现有集合）。
    shared.set_conn(Some(new_conn));
    if let Some(list) = contributions {
        shared.set_contributions(list);
    }
    shared.hot_replaces.fetch_add(1, Ordering::SeqCst);
    tracing::info!(module = %shared.module_id, "upgrade: traffic switched to replacement instance");

    drain_retired(shared, &old_conn, timeout).await;
    Ok(())
}

/// 排空被热替换下来的旧实例：`Drain{deadline_ms: timeout}` → 等其通道关闭
/// （或 deadline；期间照常分发 Emit / Log / 在途 InvokeResult）→
/// `shutdown(剩余时间)` 兜底强杀。
///
/// 旧实例的退出结果（优雅 / 强杀）不影响热替换结果：流量已在新实例上。
async fn drain_retired(shared: &Arc<Shared>, conn: &Arc<dyn ModuleConnection>, timeout: Duration) {
    let started = tokio::time::Instant::now();
    if let Err(e) = conn
        .send(HostToModule::Drain(Drain {
            deadline_ms: timeout.as_millis() as u64,
        }))
        .await
    {
        tracing::debug!(
            module = %shared.module_id,
            error = %e,
            "upgrade: failed to deliver Drain to retired instance"
        );
    }

    let wait = async {
        while let Ok(msg) = conn.recv().await {
            dispatch(shared, conn, msg);
        }
    };
    let remaining = timeout.saturating_sub(started.elapsed());
    let _ = tokio::time::timeout(remaining, wait).await;

    let remaining = timeout
        .saturating_sub(started.elapsed())
        .max(Duration::from_millis(1));
    match conn.shutdown(remaining).await {
        Ok(()) => tracing::info!(
            module = %shared.module_id,
            "upgrade: retired instance drained and exited"
        ),
        Err(e) => tracing::warn!(
            module = %shared.module_id,
            error = %e,
            "upgrade: retired instance killed after drain deadline"
        ),
    }
}

/// 状态短标签（错误消息用）。
fn state_label(state: &ModuleState) -> String {
    match state {
        ModuleState::Pending => "pending".to_string(),
        ModuleState::Ready => "ready".to_string(),
        ModuleState::Draining => "draining".to_string(),
        ModuleState::Stopped => "stopped".to_string(),
        ModuleState::Failed(reason) => format!("failed({reason})"),
    }
}

/// 「模块非 Ready，无法升级」错误（不排队语义）。
fn not_ready_error(state: &ModuleState) -> TransportError {
    TransportError::Protocol(format!(
        "module is not ready for upgrade: {}",
        state_label(state)
    ))
}

/// 分发模块 → 宿主消息。
///
/// `shared: &Arc<Shared>`：`HostCall` 分支需要把 `Arc` 移入后台任务
/// （服务调用 + 回发结果），不阻塞监督循环。
///
/// `source`：**本条消息的来源连接**。`HostCall` 回执按来源连接回发
/// （W1.2，2026-10 修复）——不能经 `current_conn()`：热替换 / 排空切换
/// 瞬间，旧实例发出的 `submit_input` 类回执会错投到新实例
/// （call_id 对新实例是孤儿，而载荷已被宿主消费；模块因收不到回执重试
/// 即造成重复投递）。
fn dispatch(shared: &Arc<Shared>, source: &Arc<dyn ModuleConnection>, msg: ModuleToHost) {
    match msg {
        ModuleToHost::InvokeResult(r) => match shared.remove_pending(&r.call_id) {
            Some(call) => {
                if call.cancel_requested {
                    tracing::debug!(module = %shared.module_id, call_id = %r.call_id, "resolving call marked cancelled");
                }
                // 结果原样透传（含 Error{code:"cancelled"}）。
                let _ = call.result_tx.send(r.outcome);
            }
            None => tracing::debug!(
                module = %shared.module_id,
                call_id = %r.call_id,
                "ignoring InvokeResult for unknown/finished call"
            ),
        },
        ModuleToHost::Emit(e) => {
            // 频率限额：超限丢弃（事件是投递即忘的，丢一条不影响正确性）。
            if !shared.allow(RateBudget::Events) {
                return;
            }
            tracing::debug!(module = %shared.module_id, event = %e.event, "module event");
            shared.push_event(e.event, e.payload);
        }
        ModuleToHost::Log(l) => {
            if !shared.allow(RateBudget::Logs) {
                return;
            }
            log_record(&shared.module_id, &l);
        }
        ModuleToHost::Register(r) => shared.set_contributions(r.contributions),
        ModuleToHost::HostCall(call) => {
            // 频率限额：超限**回一条错误回执**（模块在等，丢掉会永久挂住）。
            if !shared.allow(RateBudget::HostCalls) {
                let source = Arc::clone(source);
                let call_id = call.call_id.clone();
                tokio::spawn(async move {
                    let _ = source
                        .send(HostToModule::HostCallResult(HostCallResult {
                            call_id,
                            outcome: HostCallOutcome::Error {
                                code: "rate_limited".into(),
                                message: "宿主回呼频率超限（modules 的 rate policy）；请退避后重试"
                                    .into(),
                            },
                        }))
                        .await;
                });
                return;
            }
            // P2：宿主服务回呼。在后台任务中查表调用并回发结果。
            // 回执按**来源连接**路由（W1.2）：发送失败仅 debug（模块可能已
            // 退出 / 崩溃重启中）。
            let shared = Arc::clone(shared);
            let source = Arc::clone(source);
            tokio::spawn(async move {
                // 调用者身份**由这里填**：supervisor 知道自己为哪个 module_id
                // 跑 serve 循环，而协议帧里没有这个字段 —— 模块无法把自己的
                // 调用伪装成别的模块。宿主服务据此做来源授权与审计。
                let caller = shared.module_id.clone();
                let outcome = match shared
                    .host_services
                    .call(&caller, &call.service, &call.method, call.payload)
                    .await
                {
                    Ok(result) => HostCallOutcome::Ok { result },
                    Err((code, message)) => HostCallOutcome::Error { code, message },
                };
                if let Err(e) = source
                    .send(HostToModule::HostCallResult(HostCallResult {
                        call_id: call.call_id.clone(),
                        outcome,
                    }))
                    .await
                {
                    tracing::debug!(
                        module = %shared.module_id,
                        call_id = %call.call_id,
                        error = %e,
                        "failed to deliver HostCallResult"
                    );
                }
            });
        }
        ModuleToHost::Welcome(w) => tracing::warn!(
            module = %shared.module_id,
            version = w.version,
            "unexpected Welcome after handshake; ignored"
        ),
        ModuleToHost::Ready => {
            tracing::debug!(module = %shared.module_id, "module re-sent Ready")
        }
        ModuleToHost::Failed(f) => tracing::warn!(
            module = %shared.module_id,
            code = %f.code,
            message = %f.message,
            "module reported failure"
        ),
    }
}

/// 日志转发到 tracing（带 `module = <id>` 前缀）。
fn log_record(module_id: &str, rec: &LogRecord) {
    let fields = rec.fields.as_ref();
    match rec.level {
        LogLevel::Trace => {
            tracing::trace!(module = %module_id, fields = ?fields, "{}", rec.message)
        }
        LogLevel::Debug => {
            tracing::debug!(module = %module_id, fields = ?fields, "{}", rec.message)
        }
        LogLevel::Info => tracing::info!(module = %module_id, fields = ?fields, "{}", rec.message),
        LogLevel::Warn => tracing::warn!(module = %module_id, fields = ?fields, "{}", rec.message),
        LogLevel::Error => {
            tracing::error!(module = %module_id, fields = ?fields, "{}", rec.message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 窗口限流：配额内全放行，超了全拒；跨过 1 秒窗口重新计数。
    #[test]
    fn window_allows_up_to_limit_then_rejects() {
        let mut window = Window::default();
        for _ in 0..3 {
            assert!(window.allow(Some(3)));
        }
        assert!(!window.allow(Some(3)), "第 4 次应被拒");
        // 不限：永远放行。
        let mut unlimited = Window::default();
        for _ in 0..10_000 {
            assert!(unlimited.allow(None));
        }
        // 等一个窗口后恢复（测试里把窗口起点挪回 2 秒前，避免真睡 1 秒）。
        window.started = Some(std::time::Instant::now() - Duration::from_secs(2));
        assert!(window.allow(Some(3)), "新窗口应重新计数");
    }

    /// 频率限额三路独立：事件刷爆不影响回呼配额；被限流的帧计入 `limited`。
    #[test]
    fn rate_budgets_are_independent_and_counted() {
        let (ctl_tx, _ctl_rx) = mpsc::channel::<Control>(1);
        let shared = Shared::new(
            "rated".into(),
            RestartPolicy::default(),
            ctl_tx,
            Arc::new(HostServiceRegistry::new()),
            RatePolicy {
                events_per_sec: Some(2),
                host_calls_per_sec: Some(5),
                logs_per_sec: Some(1),
            },
        );
        // 事件：2 放 + 1 拒
        assert!(shared.allow(RateBudget::Events));
        assert!(shared.allow(RateBudget::Events));
        assert!(!shared.allow(RateBudget::Events));
        // 日志：1 放 + 1 拒
        assert!(shared.allow(RateBudget::Logs));
        assert!(!shared.allow(RateBudget::Logs));
        // 回呼：事件/日志被限不影响它（还有 5 个配额）。
        for _ in 0..5 {
            assert!(shared.allow(RateBudget::HostCalls));
        }
        assert!(!shared.allow(RateBudget::HostCalls));
        assert_eq!(shared.rate_limited(), 3, "三次被拒都计入");
    }

    /// 缺省策略是**宽松上限**（合法模块够不到）而不是「不限」——否则这条
    /// 保护在默认部署里等于没有。
    #[test]
    fn default_rate_policy_is_generous_but_finite() {
        let policy = RatePolicy::default();
        assert_eq!(policy.events_per_sec, Some(2000));
        assert_eq!(policy.host_calls_per_sec, Some(500));
        assert_eq!(policy.logs_per_sec, Some(200));
        let unlimited = RatePolicy::unlimited();
        assert!(unlimited.events_per_sec.is_none());
        assert!(unlimited.host_calls_per_sec.is_none());
        assert!(unlimited.logs_per_sec.is_none());
    }

    /// 状态标签是**线格式**（面板按它分支渲染）：五态各一个固定小写词。
    #[test]
    fn module_state_labels_are_stable_wire_strings() {
        assert_eq!(ModuleState::Pending.as_str(), "pending");
        assert_eq!(ModuleState::Ready.as_str(), "ready");
        assert_eq!(ModuleState::Draining.as_str(), "draining");
        assert_eq!(ModuleState::Stopped.as_str(), "stopped");
        assert_eq!(ModuleState::Failed("boom".into()).as_str(), "failed");
        // 原因只有 Failed 带（其它状态 None，面板据此决定是否显示错误行）。
        assert_eq!(ModuleState::Failed("boom".into()).error(), Some("boom"));
        assert_eq!(ModuleState::Ready.error(), None);
        assert_eq!(ModuleState::Stopped.error(), None);
    }

    /// 事件缓冲封顶：超限丢新并计数，消费端取走的是先到的事件。
    #[test]
    fn event_buffer_is_capped_with_drop_count() {
        let (ctl_tx, _ctl_rx) = mpsc::channel::<Control>(1);
        let shared = Shared::new(
            "test-module".into(),
            RestartPolicy::default(),
            ctl_tx,
            Arc::new(HostServiceRegistry::new()),
            RatePolicy::unlimited(),
        );
        let overflow = 50;
        for i in 0..(MAX_PENDING_EVENTS + overflow) {
            shared.push_event(format!("e{i}"), Value::Null);
        }
        let taken = shared.take_events();
        assert_eq!(taken.len(), MAX_PENDING_EVENTS, "缓冲未封顶");
        assert_eq!(taken[0].0, "e0", "应保留先到事件（丢新语义）");
        assert_eq!(
            shared.dropped_events.load(Ordering::Relaxed),
            overflow as u64,
            "丢弃计数不符"
        );
        // 再压一轮：消费后缓冲清空，恢复收纳。
        shared.push_event("after".into(), Value::Null);
        assert_eq!(shared.take_events().len(), 1);
    }
}
