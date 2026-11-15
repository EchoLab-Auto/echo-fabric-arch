//! in-proc 运输层：模块与宿主同进程，类型直连（零序列化）。
//!
//! [`InprocTransport`] 持有一个工厂（`Arc<dyn Fn(ModuleSpec) -> Box<dyn InprocModule>>`），
//! 每次 [`Transport::start`] 构造一个新模块实例，并在独立 tokio 任务中执行其
//! `run`；两侧各经一条容量 [`CHANNEL_CAPACITY`] 的 mpsc 通道交换协议消息。
//!
//! `run` 返回（或任务被 abort）视为模块退出：宿主侧 `recv` 观察为
//! [`TransportError::Closed`]，`send` 同样返回 [`TransportError::Closed`]。
//!
//! 关停是**有界**的，即使模块不再读通道（通道写满 → 宿主侧 `send` 卡在
//! `Sender::send` 上、一直占着 `to_module` 锁）也一样：见
//! `InprocConnection::shutdown`。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex};

use crate::api::{HostToModule, ModuleToHost};
use crate::transport::{ModuleConnection, ModuleSpec, Transport, TransportError};

/// 每个方向的消息通道容量。
pub const CHANNEL_CAPACITY: usize = 64;

/// 关停时等 `to_module` 锁的**上限**：拿不到就说明有 `send` 卡在「通道满」
/// 上（模块不再读），此时关不掉发送端，直接 abort 模块任务。
const TO_MODULE_LOCK_GRACE: Duration = Duration::from_millis(50);

/// 模块实现体：`run` 结束时视为模块退出。
pub trait InprocModule: Send {
    /// 由宿主在 start / 重启时调用一次，在独立任务中执行模块消息循环。
    fn run(self: Box<Self>, io: InprocIo) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

/// 模块侧端点。
pub struct InprocIo {
    rx: mpsc::Receiver<HostToModule>,
    tx: mpsc::Sender<ModuleToHost>,
}

impl InprocIo {
    /// 接收宿主消息；返回 `None` 表示宿主侧已关闭（模块应结束 `run`）。
    pub async fn recv(&mut self) -> Option<HostToModule> {
        self.rx.recv().await
    }

    /// 发送消息给宿主；返回 `false` 表示宿主侧已关闭。
    pub async fn send(&self, msg: ModuleToHost) -> bool {
        self.tx.send(msg).await.is_ok()
    }
}

/// 模块工厂：按 [`ModuleSpec`] 构造模块实例（每次 start / 重启各调用一次）。
pub type InprocFactory = Arc<dyn Fn(ModuleSpec) -> Box<dyn InprocModule> + Send + Sync>;

/// in-proc 运输层。
pub struct InprocTransport {
    factory: InprocFactory,
}

impl InprocTransport {
    /// 以模块工厂构造运输层。
    pub fn new(
        factory: impl Fn(ModuleSpec) -> Box<dyn InprocModule> + Send + Sync + 'static,
    ) -> Self {
        Self {
            factory: Arc::new(factory),
        }
    }
}

/// 宿主侧连接。
struct InprocConnection {
    /// 宿主 → 模块；`None` = shutdown 已关闭该方向。
    to_module: Mutex<Option<mpsc::Sender<HostToModule>>>,
    /// 模块 → 宿主。
    from_module: Mutex<mpsc::Receiver<ModuleToHost>>,
    /// 模块 `run` 任务句柄（shutdown 等待 / 超时强杀）。
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[async_trait]
impl Transport for InprocTransport {
    async fn start(&self, spec: ModuleSpec) -> Result<Box<dyn ModuleConnection>, TransportError> {
        let module = (self.factory)(spec);
        let (to_module, module_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (module_tx, from_module) = mpsc::channel(CHANNEL_CAPACITY);
        let io = InprocIo {
            rx: module_rx,
            tx: module_tx,
        };
        let task = tokio::spawn(module.run(io));
        Ok(Box::new(InprocConnection {
            to_module: Mutex::new(Some(to_module)),
            from_module: Mutex::new(from_module),
            task: Mutex::new(Some(task)),
        }))
    }
}

#[async_trait]
impl ModuleConnection for InprocConnection {
    async fn send(&self, msg: HostToModule) -> Result<(), TransportError> {
        let guard = self.to_module.lock().await;
        match guard.as_ref() {
            Some(tx) => tx.send(msg).await.map_err(|_| TransportError::Closed),
            None => Err(TransportError::Closed),
        }
    }

    async fn recv(&self) -> Result<ModuleToHost, TransportError> {
        let mut rx = self.from_module.lock().await;
        rx.recv().await.ok_or(TransportError::Closed)
    }

    /// 关停语义（**整体有界**）：
    /// 1. 丢弃宿主 → 模块 sender（模块侧 `recv` 得到 `None`，应自行结束）；
    /// 2. 等待模块任务结束，最多 `deadline`；
    /// 3. 超时 → `abort()` 强杀任务并返回 [`TransportError::Timeout`]（类比子进程轨
    ///    的 SIGKILL；此后模块视为已退出）。
    ///
    /// 第 1 步可能被卡住：模块不再读通道时通道很快写满，宿主侧某个 `send`
    /// 会一直等在 `Sender::send` 上并**占着 `to_module` 锁**。旧实现无条件
    /// `lock().await`，于是关停被一个读不动的模块永久挂住。现在取锁有上限
    /// （[`TO_MODULE_LOCK_GRACE`]）：拿不到就跳过「优雅关发送端」，直接 abort
    /// ——任务被 abort 会释放接收端，卡住的 `send` 随即以 `Closed` 失败并放锁。
    ///
    /// 重复调用：第二次直接 `Ok(())`（无任务可等）。
    async fn shutdown(&self, deadline: Duration) -> Result<(), TransportError> {
        let closed_gracefully =
            match tokio::time::timeout(deadline.min(TO_MODULE_LOCK_GRACE), self.to_module.lock())
                .await
            {
                Ok(mut slot) => {
                    slot.take();
                    true
                }
                Err(_) => {
                    tracing::warn!(
                        "module channel is full and a send is stuck; skipping the graceful close"
                    );
                    false
                }
            };

        let mut task = match self.task.lock().await.take() {
            Some(task) => task,
            None => return Ok(()),
        };
        if closed_gracefully && tokio::time::timeout(deadline, &mut task).await.is_ok() {
            return Ok(());
        }
        // 强杀：abort 后把句柄等回来，确保任务真的结束（不留后台残骸）。
        task.abort();
        let _ = task.await;
        Err(TransportError::Timeout)
    }
}
