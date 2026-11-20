//! dylib 运输层：模块编译成独立产物（`.so`），**运行期插入宿主进程**。
//!
//! ## 它解决什么
//!
//! 三种形态各自独占一个维度：
//!
//! | 形态 | 独立产物 | 同进程 |
//! |---|---|---|
//! | builtin | ✗ | ✓ |
//! | stdio | ✓ | ✗ |
//! | **dylib** | **✓** | **✓** |
//!
//! 代价是**没有崩溃隔离**：`.so` 里的 segfault 会杀死宿主。所以形态选择必须
//! 按模块决定——崩溃风险高的（第三方代码 / 平台适配器）用 [`crate::stdio`]，
//! 自己写的稳定模块才用这里。
//!
//! ## 跨边界面只有一个 C 符号
//!
//! 模块继续讲**和 stdio 完全相同**的帧协议，只是载体从 stdin/stdout 换成一条
//! socketpair：
//!
//! ```text
//! 宿主进程（一个地址空间）
//! ├─ DylibTransport
//! │    dlopen(path) → echo_module_run
//! │    socketpair(AF_UNIX, SOCK_STREAM|SOCK_CLOEXEC) → (host_fd, module_fd)
//! │    专用线程：echo_module_run(module_fd)      ← 跨边界就这一次
//! │    host_fd → tokio::net::UnixStream → 帧编解码（与 stdio 逐位一致）
//! └─ module.so
//!      echo_module_run(fd) { serve_fd_blocking(|| MyModule, fd) }
//! ```
//!
//! **没有 Rust 类型过边界**，因此不需要 `abi_stable` 之类的 ABI 稳定层，也不
//! 需要每个模块手写 `extern "C"` 桥（SDK 写一次，见
//! `echo_module_sdk::serve_fd_blocking`）。
//!
//! ## 生命周期语义（与 stdio 的差异）
//!
//! - **没有可杀的对象**：模块跑在同一进程的线程里。`shutdown` 只能关宿主端
//!   写方向（模块读到 EOF 自行退出）然后**等**，等不到就放弃等待并泄漏线程
//!   ——返回 [`TransportError::Timeout`] 与 stdio 同名，但语义是"没等到"，
//!   不是"已强杀"。
//! - **不能卸载**：`dlclose` 一个带线程的 Rust cdylib 不安全，`.so` 映射在整个
//!   进程生命周期内保留。所以 dylib 形态**不会**让"换模块不重启进程"变成可能
//!   ——那件事 stdio 也做不到（要重启的是宿主）。
//! - 模块崩溃（panic）由 `.so` 内的 `catch_unwind` 接住并转成非 0 退出码
//!   （**panic 跨 FFI 是 UB**，必须接在 `.so` 内部）。segfault / abort / OOM
//!   接不住，会带走宿主。

use std::ffi::c_int;
use std::io::ErrorKind;
use std::os::unix::io::{FromRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::api::{HostToModule, ModuleToHost};
use crate::frames::{self, MAX_FRAME_BYTES};
use crate::transport::{ModuleConnection, ModuleSpec, Transport, TransportError};

/// 模块 `.so` 必须导出的入口符号（C 调用约定）。
///
/// 签名 `int echo_module_run(int fd)`：`fd` 是一条已连接的流式 socket，模块在
/// 其上跑帧协议；返回值是模块的退出码（0 = 正常排空 / 收到 `Dispose`）。
pub const ENTRY_SYMBOL: &[u8] = b"echo_module_run\0";

/// [`ENTRY_SYMBOL`] 的可读名（日志 / 错误消息用）。
pub const ENTRY_NAME: &str = "echo_module_run";

/// 入口函数指针形状（与 [`ENTRY_SYMBOL`] 的 C 签名一致）。
pub type ModuleRunFn = unsafe extern "C" fn(c_int) -> c_int;

/// 写方向锁的等待上限（`shutdown` 用）：拿不到说明有 `send` 卡在对端不读上。
const WRITER_LOCK_GRACE: Duration = Duration::from_millis(50);

/// 关停时等模块线程退出的**额外**宽限：关掉写方向后，模块要读完缓冲里的
/// 剩余帧才看到 EOF。
const INFLIGHT_EXIT_GRACE: Duration = Duration::from_millis(250);

/// 单次 `read` 的暂存大小。
const READ_CHUNK: usize = 8 * 1024;

/// 关停时轮询模块线程是否结束的间隔（只影响关停的响应粒度，不影响正确性）。
const JOIN_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// dylib 运输层：每次 [`Transport::start`] 载入（或复用已载入的）`.so` 并
/// 建立一条新的 socketpair 连接。
///
/// ```no_run
/// # use echo_module_host::dylib::DylibTransport;
/// let transport = DylibTransport::new("/usr/lib/echo/modules/checklist.so");
/// ```
#[derive(Debug, Clone)]
pub struct DylibTransport {
    path: PathBuf,
}

impl DylibTransport {
    /// 以 `.so` 路径构造（相对路径按宿主当前工作目录解析）。
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// `.so` 路径（诊断用）。
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// 模板块：`.so` 句柄 + 入口。句柄必须活到模块线程结束。
struct Loaded {
    /// `dlopen` 的句柄。**永不 `dlclose`**（见模块文档）。
    _library: libloading::Library,
    entry: ModuleRunFn,
}

impl Loaded {
    fn open(path: &Path) -> Result<Self, TransportError> {
        if !path.is_file() {
            return Err(TransportError::Io(format!(
                "dylib module not found: {}",
                path.display()
            )));
        }
        // SAFETY: dlopen 只是把库映射进来；构造函数副作用的风险由"模块是受信
        // 代码"这一前提承担（与 builtin 模块同一个信任边界——这正是 dylib
        // 没有崩溃隔离、也不该被当作安全沙箱的原因）。
        let library = unsafe { libloading::Library::new(path) }
            .map_err(|e| TransportError::Io(format!("dlopen {}: {e}", path.display())))?;
        // SAFETY: 符号按 C ABI 声明；形状不符是本文件顶部文档明写的契约，
        // 错配与"模块写错了"同类，不是宿主能恢复的状态。
        let entry: ModuleRunFn = unsafe {
            let symbol: libloading::Symbol<ModuleRunFn> =
                library.get(ENTRY_SYMBOL).map_err(|e| {
                    TransportError::Io(format!(
                        "{} does not export `{ENTRY_NAME}`: {e}",
                        path.display()
                    ))
                })?;
            // 立刻把函数指针拷出来：`Symbol` 借着 `library`，而下面要把库一起
            // 存进结构体（NLL 下拷出来即可结束借用）。
            *symbol
        };
        Ok(Self {
            _library: library,
            entry,
        })
    }
}

/// 模块线程的名字（诊断 / 测试定位用）。
///
/// 截到 15 个字符：Linux 的 `comm` 只有 16 字节（含结尾 NUL），更长的名字会被
/// 内核**静默截断**——按完整名字去 `/proc` 里找会得到"线程不存在"的假阴性。
/// 这里是唯一真值，测试也用它。
pub fn worker_thread_name(module_id: &str) -> String {
    format!("module-{module_id}").chars().take(15).collect()
}

/// 一对已连接的流式 socket（`(host, module)`），两个 fd 都带 `FD_CLOEXEC`。
fn socketpair() -> Result<(RawFd, RawFd), TransportError> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: fds 是长度为 2 的可写数组；SOCK_CLOEXEC 避免这两个 fd 泄漏进
    // 宿主随后 spawn 的任何子进程（那会让一个无关进程持有模块的通道）。
    let rc = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
    if rc != 0 {
        return Err(TransportError::Io(format!(
            "socketpair: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok((fds[0], fds[1]))
}

#[async_trait]
impl Transport for DylibTransport {
    async fn start(&self, spec: ModuleSpec) -> Result<Box<dyn ModuleConnection>, TransportError> {
        let loaded = Loaded::open(&self.path)?;
        let (host_fd, module_fd) = socketpair()?;

        let (read, write) = match host_stream(host_fd) {
            Ok(parts) => parts,
            Err(error) => {
                // 宿主端接不上就没人收 module_fd，必须还回去，否则泄漏。
                // SAFETY: module_fd 由上面的 socketpair 创建、尚未交给任何人。
                unsafe { libc::close(module_fd) };
                return Err(error);
            }
        };

        // 模块线程：独占 module_fd，退出时关闭它。用专用 std 线程而不是 tokio
        // 任务——模块自己会建 runtime（见 `serve_fd_blocking`），嵌套 runtime
        // 会 panic。
        let entry = loaded.entry;
        let module_id = spec.module_id.clone();
        let worker = std::thread::Builder::new()
            .name(worker_thread_name(&module_id))
            .spawn(move || {
                // SAFETY: 调用的是模块声明的 C 入口；契约见 ENTRY_SYMBOL。
                let code = unsafe { entry(module_fd) };
                // SAFETY: module_fd 归本线程所有，正好关闭一次。
                unsafe { libc::close(module_fd) };
                code
            })
            .map_err(|e| TransportError::Io(format!("spawn module thread: {e}")))?;

        Ok(Box::new(DylibConnection {
            writer: Mutex::new(WriteState {
                stream: Some(write),
                frame: Vec::new(),
                offset: 0,
                broken: false,
            }),
            reader: Mutex::new(ReadState {
                stream: read,
                buf: Vec::new(),
                poisoned: false,
            }),
            worker: Mutex::new(Some(worker)),
            _library: loaded._library,
        }))
    }
}

/// 宿主端 fd → tokio 读写两半（非阻塞化 + 注册 reactor）。
fn host_stream(fd: RawFd) -> Result<(OwnedReadHalf, OwnedWriteHalf), TransportError> {
    // SAFETY: fd 由 socketpair 创建、尚未被任何人接管。
    let std_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
    std_stream
        .set_nonblocking(true)
        .map_err(|e| TransportError::Io(format!("set_nonblocking: {e}")))?;
    let stream = tokio::net::UnixStream::from_std(std_stream)
        .map_err(|e| TransportError::Io(format!("register module socket: {e}")))?;
    Ok(stream.into_split())
}

/// 宿主侧 dylib 连接：读写各一把锁 + 各自的持久化帧状态（与 stdio 同构）。
pub struct DylibConnection {
    writer: Mutex<WriteState>,
    reader: Mutex<ReadState>,
    /// 模块线程句柄；`None` = 已 join 或已放弃等待。
    worker: Mutex<Option<std::thread::JoinHandle<i32>>>,
    /// `.so` 句柄：模块线程还活着时必须保持映射（`dlclose` 会把它抽掉）。
    _library: libloading::Library,
}

/// 写状态：与 stdio 同构，只是对端是 socket 而不是子进程管道。
struct WriteState {
    /// `None` = shutdown 已关闭该方向。
    stream: Option<OwnedWriteHalf>,
    /// 当前帧（4 字节 LE 长度前缀 + JSON 帧体）；空 = 没有半帧残留。
    frame: Vec<u8>,
    /// 已写出的字节数。
    offset: usize,
    /// 写方向已损坏（某帧写到一半失败）：之后所有 `send` 直接 `Closed`。
    broken: bool,
}

/// 读状态。
struct ReadState {
    stream: OwnedReadHalf,
    /// 帧解码的持久缓冲：跨 `recv` 调用保留，取消不会丢字节。
    buf: Vec<u8>,
    /// 长度前缀超限后帧边界不可信；置位后所有 `recv` 直接报协议错误。
    poisoned: bool,
}

impl DylibConnection {
    /// 读一帧（**取消安全**：进度留在 `buf` 里，future 被丢弃不损坏流）。
    async fn read_frame(&self) -> Result<ModuleToHost, TransportError> {
        let mut guard = self.reader.lock().await;
        let ReadState {
            stream,
            buf,
            poisoned,
        } = &mut *guard;
        if *poisoned {
            return Err(TransportError::Protocol(
                "frame stream is unusable after a protocol error".to_string(),
            ));
        }

        loop {
            match frames::step(buf) {
                frames::Step::Incomplete => {}
                frames::Step::Fatal { len } => {
                    *poisoned = true;
                    buf.clear();
                    return Err(TransportError::Protocol(format!(
                        "frame length {len} exceeds limit {MAX_FRAME_BYTES}"
                    )));
                }
                frames::Step::Frame { consumed, decoded } => {
                    buf.drain(..consumed);
                    return decoded;
                }
            }

            let mut chunk = [0u8; READ_CHUNK];
            let read = match stream.read(&mut chunk).await {
                Ok(read) => read,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(map_read_error("read frame", e)),
            };
            if read == 0 {
                return Err(TransportError::Closed);
            }
            buf.extend_from_slice(&chunk[..read]);
        }
    }

    /// 写一帧（**取消安全**：先补完上次残留的半帧再写新帧）。
    async fn write_frame(&self, msg: &HostToModule) -> Result<(), TransportError> {
        let frame = frames::encode(msg)?;

        let mut guard = self.writer.lock().await;
        if guard.broken || guard.stream.is_none() {
            return Err(TransportError::Closed);
        }

        write_pending(&mut guard).await?;
        guard.frame = frame;
        guard.offset = 0;
        write_pending(&mut guard).await?;
        guard.frame.clear();
        guard.offset = 0;

        match guard.stream.as_mut() {
            Some(stream) => stream.flush().await.map_err(map_write_error),
            None => Err(TransportError::Closed),
        }
    }
}

/// 把 `state.frame[state.offset..]` 全部写进 socket（`Interrupted` 重试）。
async fn write_pending(state: &mut WriteState) -> Result<(), TransportError> {
    let WriteState {
        stream,
        frame,
        offset,
        broken,
    } = state;
    let Some(socket) = stream.as_mut() else {
        return Err(TransportError::Closed);
    };
    while *offset < frame.len() {
        match socket.write(&frame[*offset..]).await {
            Ok(0) => {
                *broken = true;
                return Err(TransportError::Closed);
            }
            Ok(n) => *offset += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => {
                *broken = true;
                return Err(map_write_error(e));
            }
        }
    }
    Ok(())
}

#[async_trait]
impl ModuleConnection for DylibConnection {
    async fn send(&self, msg: HostToModule) -> Result<(), TransportError> {
        self.write_frame(&msg).await
    }

    async fn recv(&self) -> Result<ModuleToHost, TransportError> {
        self.read_frame().await
    }

    /// 关停（**没有可杀的对象**，见模块文档）：
    ///
    /// 1. 有界地关掉写方向（`SHUT_WR`）→ 模块读完剩余帧后见到 EOF 自行退出；
    /// 2. 等模块线程结束，最多 `deadline + INFLIGHT_EXIT_GRACE`；
    /// 3. 等不到 → 放弃等待并**泄漏线程**（它仍持有 fd 与库映射），返回
    ///    [`TransportError::Timeout`]。
    ///
    /// 与 stdio 的关键差别：第 3 步之后**进程里确实还多着一个线程**。调用方
    /// 若据此重启模块，旧实例会与新实例并存直到它自己退出——这是 dylib 形态
    /// 固有的代价，不是实现缺陷。
    ///
    /// 重复调用：已 join 过则直接 `Ok(())`（幂等）。
    async fn shutdown(&self, deadline: Duration) -> Result<(), TransportError> {
        let started = Instant::now();

        // 1) 有界地关写方向。拿不到锁 = 有 send 卡在对端不读上，那就别等了。
        let closed = match tokio::time::timeout(deadline.min(WRITER_LOCK_GRACE), self.writer.lock())
            .await
        {
            Ok(mut writer) => {
                let damaged =
                    writer.broken || (writer.offset > 0 && writer.offset < writer.frame.len());
                if damaged {
                    writer.broken = true;
                } else {
                    if let Some(mut stream) = writer.stream.take() {
                        // SHUT_WR：模块读到 EOF 后自行退出。失败不影响后续等待。
                        let _ = tokio::time::timeout(deadline, stream.shutdown()).await;
                    }
                    writer.frame.clear();
                    writer.offset = 0;
                }
                !damaged
            }
            Err(_) => {
                tracing::warn!(
                    "module socket writer is stuck (peer is not reading); skipping the graceful close"
                );
                false
            }
        };

        // 2) 等模块线程退出。
        //
        // **不能用 `spawn_blocking(join)`**：那个阻塞任务在线程不退出时永远不
        // 结束，而 tokio 的 `Runtime` 在 drop 时**会等所有 blocking 任务收尾**
        // ——宿主的一次有序关停会被一个不肯退出的 dylib 模块整整挂住。2026-11
        // 实测：`#[tokio::test]` 的运行时在测试结束后等待它，单个用例挂 60s+。
        //
        // 改为轮询 `is_finished()`：不占任何池线程，超时立刻返回；线程仍在跑
        // 就是仍在跑（dylib 没有可杀的对象，这是形态本身的代价）。
        let Some(worker) = self.worker.lock().await.take() else {
            return Ok(()); // 已关停：幂等
        };
        let budget = if closed {
            deadline
                .saturating_sub(started.elapsed())
                .saturating_add(INFLIGHT_EXIT_GRACE)
        } else {
            deadline.saturating_sub(started.elapsed())
        };
        let give_up_at = Instant::now() + budget;
        while !worker.is_finished() {
            if Instant::now() >= give_up_at {
                tracing::warn!(
                    "dylib module did not exit within the drain deadline; \
                     its thread is left running (a dylib cannot be killed)"
                );
                return Err(TransportError::Timeout);
            }
            tokio::time::sleep(JOIN_POLL_INTERVAL).await;
        }
        // 已结束：`join` 立即返回（顺带把线程的返回值吃掉，避免遗留资源）。
        let _ = worker.join();
        Ok(())
    }
}

impl Drop for DylibConnection {
    fn drop(&mut self) {
        // 不主动 join：drop 是同步的，等一个不合作的模块线程会挂住调用方。
        // 关闭 fd 由两个半句柄的 Drop 完成，模块随即见到 EOF 自行退出。
        if let Ok(mut worker) = self.worker.try_lock() {
            if let Some(handle) = worker.take() {
                // 只 detach（丢弃句柄，线程继续跑到自然结束）。
                drop(handle);
            }
        }
    }
}

/// 读错误映射：EOF / 对端关闭 = `Closed`；其余 = IO 错误。
fn map_read_error(what: &str, e: std::io::Error) -> TransportError {
    match e.kind() {
        ErrorKind::UnexpectedEof | ErrorKind::BrokenPipe | ErrorKind::ConnectionReset => {
            TransportError::Closed
        }
        _ => TransportError::Io(format!("{what}: {e}")),
    }
}

/// 写错误映射：对端消失 = `Closed`；其余 = IO 错误。
fn map_write_error(e: std::io::Error) -> TransportError {
    match e.kind() {
        ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::UnexpectedEof => {
            TransportError::Closed
        }
        _ => TransportError::Io(format!("write frame: {e}")),
    }
}
