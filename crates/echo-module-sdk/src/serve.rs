//! 服务循环：握手 + 消息分发 + 单写者帧输出。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use echo_module_api::{
    compatible, Failure, HostCall, HostCallOutcome, HostToModule, InvokeOutcome, InvokeResult,
    ModuleToHost, Register, Welcome, PROTOCOL_NAME, PROTOCOL_VERSION,
};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;

use crate::error::SdkError;
use crate::frame::{encode_frame, read_frame};
use crate::{InvokeRequest, ModuleHandler, ServeOptions};

/// HostCall 默认超时（[`HostClient::call`]）。
pub const DEFAULT_HOST_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// 宿主服务回呼错误（[`HostClient`]）。
#[derive(Debug, thiserror::Error)]
pub enum HostCallError {
    /// 等待结果超时（默认 [`DEFAULT_HOST_CALL_TIMEOUT`]，或显式传给
    /// [`HostClient::call_with_timeout`] 的值）。
    #[error("host call timed out")]
    Timeout,
    /// 连接已关闭（宿主断开 / serve 循环已结束，在途调用被丢弃）。
    #[error("module connection is closed")]
    Closed,
    /// 宿主服务返回错误（机器可读码 + 人类可读消息）。
    #[error("host service error [{code}]: {message}")]
    Service {
        /// 机器可读错误码（如 `service_not_found` / `method_not_found`）。
        code: String,
        /// 人类可读消息。
        message: String,
    },
    /// 请求帧编码失败（JSON 序列化 / 超限）。
    #[error("failed to encode host call: {0}")]
    Encode(String),
}

/// 模块 → 宿主服务回呼客户端（可 Clone）。
///
/// 由 SDK 在 `Ready` 发出后经 [`ModuleHandler::on_ready`] 交付；模块保存
/// 句柄后即可随时向宿主注册的服务发起回呼。`call_id` 形如 `host:<序号>`
/// （模块侧自增），与 `HostCallResult` 一一配对。
#[derive(Clone)]
pub struct HostClient {
    /// 写侧帧通道（弱引用）：serve 循环结束时自动失效（此后调用返回
    /// [`HostCallError::Closed`]，且不阻塞写者任务收口）。
    tx: mpsc::WeakUnboundedSender<Vec<u8>>,
    /// 在途调用表（`call_id` → 结果通道）；读到 `HostCallResult` 时唤醒。
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<HostCallOutcome>>>>,
    /// `call_id` 序号（从 1 开始）。
    seq: Arc<AtomicU64>,
}

impl HostClient {
    /// 调用宿主服务（默认超时 [`DEFAULT_HOST_CALL_TIMEOUT`]）。
    ///
    /// 成功返回服务结果 JSON；服务错误 → [`HostCallError::Service`]。
    pub async fn call(
        &self,
        service: &str,
        method: &str,
        payload: Value,
    ) -> Result<Value, HostCallError> {
        self.call_with_timeout(service, method, payload, DEFAULT_HOST_CALL_TIMEOUT)
            .await
    }

    /// 调用宿主服务（显式超时）。
    pub async fn call_with_timeout(
        &self,
        service: &str,
        method: &str,
        payload: Value,
        timeout: Duration,
    ) -> Result<Value, HostCallError> {
        let call_id = format!("host:{}", self.seq.fetch_add(1, Ordering::SeqCst) + 1);
        let (result_tx, result_rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(call_id.clone(), result_tx);

        let sent = match self.tx.upgrade() {
            Some(tx) => encode_frame(&ModuleToHost::HostCall(HostCall {
                call_id: call_id.clone(),
                service: service.to_string(),
                method: method.to_string(),
                payload,
            }))
            .map_err(|error| HostCallError::Encode(error.to_string()))
            .and_then(|frame| tx.send(frame).map_err(|_| HostCallError::Closed)),
            None => Err(HostCallError::Closed),
        };
        if let Err(error) = sent {
            self.pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&call_id);
            return Err(error);
        }

        match tokio::time::timeout(timeout, result_rx).await {
            Ok(Ok(HostCallOutcome::Ok { result })) => Ok(result),
            Ok(Ok(HostCallOutcome::Error { code, message })) => {
                Err(HostCallError::Service { code, message })
            }
            // 结果通道被丢弃（serve 结束 / 连接断开时清空在途表）。
            Ok(Err(_)) => Err(HostCallError::Closed),
            Err(_) => {
                self.pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&call_id);
                Err(HostCallError::Timeout)
            }
        }
    }
}

/// 在 stdio 上运行模块（读 stdin、写 stdout），直到排空 / 终止 / EOF 退出。
pub async fn serve_stdio(handler: impl ModuleHandler) -> std::io::Result<()> {
    serve_stdio_with(handler, ServeOptions::default()).await
}

/// [`serve_stdio`] 的显式选项版（`module_id` / `capabilities`）。
pub async fn serve_stdio_with(
    handler: impl ModuleHandler,
    options: ServeOptions,
) -> std::io::Result<()> {
    serve_io_with(handler, options, tokio::io::stdin(), tokio::io::stdout()).await
}

/// 泛型核心：在任意读写流上运行模块（测试经 `tokio::io::duplex` 复用）。
///
/// `writer` 由内部单写者任务独占（帧不会交错）；`reader` 在主循环中读取。
pub async fn serve_io<R, W>(
    handler: impl ModuleHandler,
    reader: R,
    writer: W,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    serve_io_with(handler, ServeOptions::default(), reader, writer).await
}

/// [`serve_io`] 的显式选项版。
pub async fn serve_io_with<R, W>(
    handler: impl ModuleHandler,
    options: ServeOptions,
    reader: R,
    writer: W,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    run(handler, options, reader, writer)
        .await
        .map_err(std::io::Error::from)
}

/// 退出路径：
/// - `Drain`：`on_drain` 后等待在途调用完成（结果全部写回）再退出；
/// - `Dispose` / stdin EOF：中止在途任务后退出。
async fn run<R, W>(
    handler: impl ModuleHandler,
    options: ServeOptions,
    mut reader: R,
    writer: W,
) -> Result<(), SdkError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let handler = Arc::new(handler);
    let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let writer_task = tokio::spawn(write_frames(rx, writer));

    // 1) 首帧必须 Hello；Hello 前 EOF 视为干净退出（宿主已在关停）。
    let Some(first) = read_frame(&mut reader).await? else {
        drop(tx);
        return finish_writer(writer_task).await;
    };
    let hello = match first {
        HostToModule::Hello(hello) => hello,
        other => {
            let message = format!("first frame must be Hello, got {other:?}");
            tracing::warn!(%message, "rejecting module connection");
            write_failure(&tx, "protocol_violation", &message);
            drop(tx);
            let _ = finish_writer(writer_task).await;
            return Err(SdkError::Protocol(message));
        }
    };

    // 2) 校验协议名与版本（与宿主 `start_and_handshake` 同口径）。
    if hello.protocol != PROTOCOL_NAME || !compatible(PROTOCOL_VERSION, hello.version) {
        let message = format!(
            "incompatible handshake: host protocol {:?} v{}, module supports {:?} v{}",
            hello.protocol, hello.version, PROTOCOL_NAME, PROTOCOL_VERSION
        );
        tracing::warn!(%message, "rejecting module connection");
        write_failure(&tx, "incompatible_protocol", &message);
        drop(tx);
        let _ = finish_writer(writer_task).await;
        return Err(SdkError::Protocol(message));
    }

    // 3) Welcome + Register + Ready（顺序入队；单写者保证线上顺序）。
    //    实例 id：显式选项非空则覆盖；否则回显宿主 Hello 的 module_id
    //    （实例身份由宿主掌握，模块零配置）。
    let effective_module_id = if options.module_id.is_empty() {
        hello.module_id.clone()
    } else {
        options.module_id.clone()
    };
    // 宿主下发配置（`Hello.config`）先于贡献收集：模块实例名 / 端口等
    // 可由配置决定。配置非法 = Failed 帧 + 退出（宿主按启动失败处理）。
    if let Err(message) = handler.configure(hello.config.clone()).await {
        tracing::warn!(%message, "module rejected host config");
        write_failure(&tx, "invalid_config", &message);
        drop(tx);
        let _ = finish_writer(writer_task).await;
        return Err(SdkError::Protocol(message));
    }
    let contributions = handler.contributions();
    let registered: Arc<HashSet<String>> = Arc::new(
        contributions
            .iter()
            .filter_map(|c| c.target_name().map(str::to_string))
            .collect(),
    );
    enqueue(
        &tx,
        &ModuleToHost::Welcome(Welcome {
            protocol: PROTOCOL_NAME.to_string(),
            version: PROTOCOL_VERSION,
            module_id: effective_module_id.clone(),
            capabilities: options.capabilities.clone(),
        }),
    )?;
    enqueue(&tx, &ModuleToHost::Register(Register { contributions }))?;
    enqueue(&tx, &ModuleToHost::Ready)?;
    tracing::debug!(
        module_id = %effective_module_id,
        contributions = registered.len(),
        "handshake complete"
    );

    // 3.5) P2 HostCall 客户端：`Ready` 已入队（单写者保证其先于后续帧写出），
    //      在此构造并交给模块保存（on_ready）。在途表与读循环共享。
    let host_calls: Arc<Mutex<HashMap<String, oneshot::Sender<HostCallOutcome>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let host_client = HostClient {
        tx: tx.downgrade(),
        pending: Arc::clone(&host_calls),
        seq: Arc::new(AtomicU64::new(0)),
    };
    handler.on_ready(host_client).await;

    // 4) 消息循环：Invoke 并发处理（结果经 mpsc 单写者串行写回）。
    let mut tasks: JoinSet<()> = JoinSet::new();
    let mut read_error: Option<SdkError> = None;
    loop {
        let msg = match read_frame(&mut reader).await {
            Ok(Some(msg)) => msg,
            // stdin EOF：宿主关停；中止在途调用后退出。
            Ok(None) => {
                tasks.abort_all();
                break;
            }
            // 读错误（帧损坏 / IO）：同样走收尾路径，避免在途 HostCall 挂死。
            Err(error) => {
                tasks.abort_all();
                read_error = Some(error);
                break;
            }
        };
        match msg {
            HostToModule::Invoke(invoke) => {
                let handler = Arc::clone(&handler);
                let registered = Arc::clone(&registered);
                let tx = tx.clone();
                tasks.spawn(async move {
                    let call_id = invoke.call_id.clone();
                    // 可路由 = 精确命中已注册贡献；或 `"<name>:<op>"` 模块实例寻址
                    // 且 `<name>` 是已注册的模块实例（P1.2）。贡献名原样交给
                    // handler（`InstanceHandler` 负责剥离实例名前缀）。
                    let known = registered.contains(&invoke.contribution)
                        || split_instance_op(&invoke.contribution)
                            .is_some_and(|(name, _)| registered.contains(&name));
                    let outcome = if known {
                        handler
                            .invoke(InvokeRequest {
                                call_id: call_id.clone(),
                                contribution: invoke.contribution,
                                ctx: invoke.ctx,
                                payload: invoke.payload,
                            })
                            .await
                    } else {
                        InvokeOutcome::Error {
                            code: "unknown_contribution".to_string(),
                            message: format!("unknown contribution: {}", invoke.contribution),
                        }
                    };
                    let _ = enqueue(
                        &tx,
                        &ModuleToHost::InvokeResult(InvokeResult { call_id, outcome }),
                    );
                });
            }
            HostToModule::Cancel(cancel) => {
                let handler = Arc::clone(&handler);
                tasks.spawn(async move {
                    handler.on_cancel(&cancel.call_id).await;
                });
            }
            HostToModule::Drain(drain) => {
                handler.on_drain(drain.deadline_ms).await;
                break;
            }
            HostToModule::Dispose => {
                tasks.abort_all();
                break;
            }
            HostToModule::Event(_) => {
                // 未订阅的事件：忽略（SDK 暂不提供事件钩子）。
            }
            HostToModule::Hello(_) => {
                tracing::warn!("ignoring duplicate Hello");
            }
            HostToModule::HostCallResult(result) => {
                let waiter = host_calls
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&result.call_id);
                match waiter {
                    Some(waiter) => {
                        let _ = waiter.send(result.outcome);
                    }
                    None => tracing::debug!(
                        call_id = %result.call_id,
                        "ignoring HostCallResult for unknown/finished call"
                    ),
                }
            }
        }
    }

    // 5) 收尾：Drain 等待在途任务完成；Dispose / EOF 已中止。写者最后收口。
    //    丢弃全部在途 HostCall（等待者得到 Closed，而非挂到超时）。
    host_calls
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    drop(tx);
    while tasks.join_next().await.is_some() {}
    let writer_result = finish_writer(writer_task).await;
    match read_error {
        Some(error) => Err(error),
        None => writer_result,
    }
}

/// 模块实例方法寻址拆分（P1.2）：`"<name>:<op>"` → `(<name>, <op>)`；
/// 无冒号 = 非模块实例调用（None）。
pub(crate) fn split_instance_op(contribution: &str) -> Option<(String, String)> {
    let (name, op) = contribution.split_once(':')?;
    if name.is_empty() || op.is_empty() {
        return None;
    }
    Some((name.to_string(), op.to_string()))
}

/// 编码并入队一帧（不等待写出；顺序由单写者保证）。
fn enqueue(tx: &mpsc::UnboundedSender<Vec<u8>>, msg: &ModuleToHost) -> Result<(), SdkError> {
    let frame = encode_frame(msg)?;
    tx.send(frame)
        .map_err(|_| SdkError::Protocol("frame writer task is gone".to_string()))
}

/// 尽力写一条 `Failed`（初始化拒绝路径）。
fn write_failure(tx: &mpsc::UnboundedSender<Vec<u8>>, code: &str, message: &str) {
    let _ = enqueue(
        tx,
        &ModuleToHost::Failed(Failure {
            code: code.to_string(),
            message: message.to_string(),
        }),
    );
}

/// 等待写者任务收口（通道关闭 = 全部帧写完）。
async fn finish_writer(
    writer_task: tokio::task::JoinHandle<std::io::Result<()>>,
) -> Result<(), SdkError> {
    match writer_task.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(SdkError::Io(error)),
        Err(error) => Err(SdkError::Protocol(format!(
            "frame writer task failed: {error}"
        ))),
    }
}

/// 单写者任务：按到达顺序逐帧 `write_all` + `flush`（stdout 有行缓冲，
/// 必须逐帧 flush，握手帧才能及时送达宿主）。
async fn write_frames<W>(
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
    mut writer: W,
) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    while let Some(frame) = rx.recv().await {
        writer.write_all(&frame).await?;
        writer.flush().await?;
    }
    Ok(())
}
