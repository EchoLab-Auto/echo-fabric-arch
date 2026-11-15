//! stdio 运输层：模块作为独立子进程运行，经 stdin/stdout 交换协议帧。
//!
//! ## 帧编码（本次定稿）
//!
//! 帧 = 4 字节**小端** `u32` 长度前缀 + JSON 帧体；前缀计的是帧体字节数，
//! JSON 形状与 [`HostToModule`] / [`ModuleToHost`] 的 serde 表示一致。
//! 单帧上限 [`MAX_FRAME_BYTES`]（16 MiB）：读侧长度前缀超限、写侧序列化结果
//! 超限均返回 [`TransportError::Protocol`]。
//!
//! 读写状态（读缓冲 / 未写完的帧）**保存在连接内**：`recv` / `send` 的
//! future 被丢弃（`select!` 控制分支、调用方取消）后，下一次调用从断点
//! 续读 / 续写，不会把半帧留在管道里（取消安全契约见
//! [`ModuleConnection::recv`] / [`ModuleConnection::send`]）。stdin 写入端与
//! stdout 读取端各由一把 [`tokio::sync::Mutex`] 串行化，并发 `send` /
//! `recv` 不会交错损坏帧。
//!
//! ## 进程管理
//!
//! - spawn：stdin/stdout/stderr 全部管道化 + `kill_on_drop(true)`；Unix 下
//!   `process_group(0)`（模块独立成组，便于整组清理）。
//! - stderr：后台任务逐行读取，转发 `tracing::warn!`（带 `module = <id>` 字段）。
//! - [`StdioConnection::shutdown`]：**整体有界**——关闭 stdin → 等子进程退出
//!   （带 deadline）→ 超时 `start_kill()` + 有界收尸（类比 SIGKILL 语义）并返回
//!   [`TransportError::Timeout`]；写方向半帧 / 写锁被卡住的 `send` 占着时直接
//!   强杀（不让「对端不读 stdin」把关停挂死）。重复调用安全（幂等）。

use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::api::{HostToModule, ModuleToHost};
use crate::transport::{ModuleConnection, ModuleSpec, Transport, TransportError};

/// 单帧上限（16 MiB）。读侧长度前缀与写侧序列化结果均受此约束。
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// 关停时等写方向锁的**上限**：拿不到就说明有 `send` 卡在管道写入里
/// （对端不读 stdin），此时强杀是唯一有界的出路。
const WRITER_LOCK_GRACE: Duration = Duration::from_millis(50);

/// 强杀之后等收尸的上限。收尸超时**不丢** `Child`——交给后台任务继续等
/// （SIGKILL 已发，进程终将被回收），本函数照常返回。
const REAP_GRACE: Duration = Duration::from_secs(2);

/// 写方向已损坏时，给子进程「自己退出」的观察窗口。
///
/// 取一个**很小**的值而不是 `deadline`：残帧补不齐，对端一般不会自己退，
/// 等满预算只是浪费。但也不能不等——内核先关文件描述符、之后进程才变成可
/// 回收的僵尸，于是「写侧 EPIPE」可能早于「`try_wait` 能看到退出」几个调度
/// 切片；不留窗口就会把「刚刚好自己退出」误判成强杀。
const DAMAGED_EXIT_GRACE: Duration = Duration::from_millis(250);

/// 子进程资源限额（运行时形态；配置侧等价于 `modules.toml` 的
/// `[module.limits]`）。
///
/// 全体 `Option`：**`None` = 不限制**（继承宿主）。只支持五项，每项都是
/// 硬限额（软=硬）——模块没有义务处理 SIGXCPU，两段式限额只会让人误以为
/// 没生效。
///
/// 限额在 `exec` **之前**由子进程自己设置（`setrlimit` 继承给它的后代），
/// 因此模块再派生的子进程同样受限。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResourceLimits {
    /// CPU 时间（秒）。
    pub cpu_seconds: Option<u64>,
    /// 地址空间（字节）。
    pub memory_bytes: Option<u64>,
    /// 单文件写入上限（字节）。
    pub file_size_bytes: Option<u64>,
    /// 打开文件描述符数（含 socket）。
    pub open_files: Option<u64>,
    /// 子进程数。
    pub processes: Option<u64>,
}

impl ResourceLimits {
    /// 是否一项都没设（此时不装 `pre_exec`，spawn 路径与无限制完全一致）。
    pub fn is_empty(&self) -> bool {
        self.cpu_seconds.is_none()
            && self.memory_bytes.is_none()
            && self.file_size_bytes.is_none()
            && self.open_files.is_none()
            && self.processes.is_none()
    }
}

/// stdio 运输层：每次 [`Transport::start`] spawn 一个模块子进程。
///
/// 命令始终按可执行文件路径解析（[`PathBuf`]，不经 shell、无通配 / 变量展开）：
///
/// ```no_run
/// # use echo_module_host::stdio::StdioTransport;
/// let transport = StdioTransport::new("/usr/bin/my-module")
///     .with_args(["serve", "--stdio"])
///     .with_env("MY_MODULE_MODE", "test")
///     .with_cwd("/tmp");
/// ```
#[derive(Debug, Clone)]
pub struct StdioTransport {
    program: PathBuf,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    cwd: Option<PathBuf>,
    limits: ResourceLimits,
}

impl StdioTransport {
    /// 以可执行文件路径构造（相对路径按宿主当前工作目录解析）。
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
            limits: ResourceLimits::default(),
        }
    }

    /// 追加命令行参数（链式；多次调用累积）。
    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// 追加一个子进程环境变量（链式；宿主环境默认不继承，仅透传 `PATH`）。
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// 设置子进程工作目录（链式）。
    pub fn with_cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// 设置子进程资源限额（链式；缺省 = 不限制）。
    ///
    /// 空限额（一项都没设）等价于没调用：不装 `pre_exec`，spawn 路径保持
    /// 无限制时的原样。
    pub fn with_limits(mut self, limits: ResourceLimits) -> Self {
        self.limits = limits;
        self
    }

    /// 组装 spawn 命令（管道 stdio / 最小环境 / 进程组 / 工作目录）。
    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // 最小环境：不继承宿主环境，仅透传 PATH（可执行查找 / 模块自身依赖）；
        // 其余变量经 `with_env` 显式声明，不额外泄漏。
        cmd.env_clear();
        if let Some(path) = std::env::var_os("PATH") {
            cmd.env("PATH", path);
        }
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        #[cfg(unix)]
        cmd.process_group(0); // 独立进程组：shutdown 超时强杀 / 整组清理的前提。
        #[cfg(unix)]
        if !self.limits.is_empty() {
            let limits = self.limits;
            // SAFETY: `pre_exec` 的闭包在 fork 之后、exec 之前运行，只允许
            // 异步信号安全的操作。这里只调 `setrlimit`（POSIX 允许在
            // fork 与 exec 之间调用），不分配内存、不取锁、不碰 Rust 运行时。
            unsafe {
                cmd.pre_exec(move || apply_limits(&limits));
            }
        }
        cmd
    }
}

/// 在子进程里落地限额（`fork` 后、`exec` 前；只做 `setrlimit`）。
///
/// 返回值是 `io::Error`：`pre_exec` 的契约要求如此，任一限额设置失败即让
/// spawn 失败——**静默忽略等于假装限额生效**，那比不设更危险。
#[cfg(unix)]
fn apply_limits(limits: &ResourceLimits) -> std::io::Result<()> {
    // `setrlimit` 在 linux 绑定里第一个形参是 `__rlimit_resource_t`（= u32），
    // 而 `RLIMIT_*` 常量也是 u32；直接沿用绑定类型，避免跨平台猜宽度。
    fn set(resource: libc::__rlimit_resource_t, value: Option<u64>) -> std::io::Result<()> {
        let Some(value) = value else {
            return Ok(());
        };
        // rlim_t 在不同平台上宽度不同：u64 → libc::rlim_t 显式转换，
        // 溢出即按"上限"处理（不做静默截断）。
        let limit = libc::rlim_t::try_from(value).unwrap_or(libc::rlim_t::MAX);
        let rlimit = libc::rlimit {
            rlim_cur: limit,
            rlim_max: limit,
        };
        // SAFETY: 传的是本函数栈上的有效 rlimit 指针；setrlimit 不保留它。
        let rc = unsafe { libc::setrlimit(resource, &rlimit) };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
    set(libc::RLIMIT_CPU, limits.cpu_seconds)?;
    set(libc::RLIMIT_AS, limits.memory_bytes)?;
    set(libc::RLIMIT_FSIZE, limits.file_size_bytes)?;
    set(libc::RLIMIT_NOFILE, limits.open_files)?;
    // RLIMIT_NPROC 限的是**该 uid 的**总进程数，不是本进程树的——多模块
    // 同 uid 部署时它会把彼此算进配额（可能误伤）。仍然提供，但值要给得
    // 足够宽（文档已注明）。
    set(libc::RLIMIT_NPROC, limits.processes)?;
    Ok(())
}

#[async_trait]
impl Transport for StdioTransport {
    async fn start(&self, spec: ModuleSpec) -> Result<Box<dyn ModuleConnection>, TransportError> {
        let mut cmd = self.command();
        let mut child = cmd.spawn().map_err(|e| {
            TransportError::Io(format!(
                "failed to spawn module process {:?}: {e}",
                self.program
            ))
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| TransportError::Io("module stdin pipe unavailable".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| TransportError::Io("module stdout pipe unavailable".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| TransportError::Io("module stderr pipe unavailable".to_string()))?;

        spawn_stderr_forwarder(&spec.module_id, stderr);

        Ok(Box::new(StdioConnection {
            writer: Mutex::new(WriteState {
                stdin: Some(stdin),
                frame: Vec::new(),
                offset: 0,
                broken: false,
            }),
            reader: Mutex::new(ReadState {
                stdout,
                buf: Vec::new(),
                poisoned: false,
            }),
            child: Mutex::new(Some(child)),
        }))
    }
}

/// 宿主侧 stdio 连接：读写各一把锁 + 各自的**持久化帧状态**。
///
/// 帧状态放在连接里而不是调用栈上的局部缓冲，是取消安全的前提：
/// `recv` / `send` 的 future 可能在任意 `await` 处被丢弃（`select!` 的控制
/// 分支、调用方取消），下一次调用必须能从断点继续，而不是把半帧留在管道里。
pub struct StdioConnection {
    /// 宿主 → 模块方向的写状态（stdin + 未写完的帧）。
    writer: Mutex<WriteState>,
    /// 模块 → 宿主方向的读状态（stdout + 未消费的字节）。
    reader: Mutex<ReadState>,
    /// 子进程句柄；`None` = 已收割（shutdown 完成或已强杀）。
    child: Mutex<Option<Child>>,
}

/// 写状态：`frame[..offset]` 已交给管道，`frame[offset..]` 尚未写出。
struct WriteState {
    /// `None` = shutdown 已关闭该方向。
    stdin: Option<ChildStdin>,
    /// 当前帧（4 字节 LE 长度前缀 + JSON 帧体）；空 = 没有半帧残留。
    frame: Vec<u8>,
    /// 已写出的字节数。
    offset: usize,
    /// **写方向已损坏**：某一帧写到一半就失败了（对端消失 / 管道破裂）。
    ///
    /// 此时管道里留着一个帧前缀 + 半截帧体，后续任何新帧都会被对端当成
    /// 这半帧的续体——那正是「误执行」。置位后所有 `send` 直接 `Closed`，
    /// 由关停路径强杀收场（见 [`StdioConnection::shutdown`]）。
    broken: bool,
}

/// 读状态：`buf` 是已从管道读出、尚未消费的字节。
struct ReadState {
    stdout: ChildStdout,
    /// 帧解码的持久缓冲：跨 `recv` 调用保留，取消不会丢字节。
    buf: Vec<u8>,
    /// 长度前缀超限后帧边界不可信；置位后所有 `recv` 直接报协议错误。
    poisoned: bool,
}

/// 单次 `read` 的暂存大小（读到的字节在同一 poll 内并入 `buf`）。
const READ_CHUNK: usize = 8 * 1024;

impl StdioConnection {
    /// 读一帧：长度前缀（4 字节 LE）→ 帧体 → JSON 解码。
    ///
    /// **取消安全**：`buf` 是连接内状态；`AsyncReadExt::read` 是 tokio 的
    /// 取消安全原语。在读满一帧之前丢弃 future，已读字节仍留在 `buf`，
    /// 下一次 `recv` 从断点续读——`select!` 用控制消息取消 `recv` 不会
    /// 损坏流。EOF（对端退出）= [`TransportError::Closed`]。
    async fn read_frame(&self) -> Result<ModuleToHost, TransportError> {
        let mut guard = self.reader.lock().await;
        let ReadState {
            stdout,
            buf,
            poisoned,
        } = &mut *guard;
        if *poisoned {
            return Err(TransportError::Protocol(
                "frame stream is unusable after a protocol error".to_string(),
            ));
        }

        loop {
            if buf.len() >= 4 {
                let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
                if len > MAX_FRAME_BYTES {
                    // 长度前缀本身不可信 → 帧边界丢失，后续字节无法再解释。
                    *poisoned = true;
                    buf.clear();
                    return Err(TransportError::Protocol(format!(
                        "frame length {len} exceeds limit {MAX_FRAME_BYTES}"
                    )));
                }
                if buf.len() >= 4 + len {
                    let decoded = serde_json::from_slice(&buf[4..4 + len]);
                    // 帧边界完整才消费缓冲；解码失败不破坏边界。
                    buf.drain(..4 + len);
                    return decoded
                        .map_err(|e| TransportError::Protocol(format!("invalid JSON frame: {e}")));
                }
            }

            let mut chunk = [0u8; READ_CHUNK];
            let read = match stdout.read(&mut chunk).await {
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

    /// 写一帧：JSON 编码 → 4 字节 LE 长度前缀 + 帧体。
    ///
    /// **取消安全**：先把本帧登记为 `frame`，再按 `offset` 续写；future 被
    /// 丢弃时进度留在连接内，后续 `send` 先补完半帧再写新帧，不会交错。
    /// 代价：被取消那次 `send` 的帧仍可能（由后续 `send` 补完）送达——
    /// 调用方不应把「取消」理解为「一定没发送」。
    ///
    /// **失败即损坏**：写到一半失败（对端消失 / 管道破裂）会把写方向置为
    /// `broken`，之后所有 `send` 直接 `Closed`——绝不把新帧追加到半帧后面
    /// 让对端误解析。
    async fn write_frame(&self, msg: &HostToModule) -> Result<(), TransportError> {
        let body = serde_json::to_vec(msg)
            .map_err(|e| TransportError::Protocol(format!("encode message: {e}")))?;
        if body.len() > MAX_FRAME_BYTES {
            return Err(TransportError::Protocol(format!(
                "frame length {} exceeds limit {MAX_FRAME_BYTES}",
                body.len()
            )));
        }

        let mut guard = self.writer.lock().await;
        if guard.broken || guard.stdin.is_none() {
            return Err(TransportError::Closed);
        }

        // 先补完上次被取消时残留的半帧（线序不能乱）。
        write_pending(&mut guard).await?;
        guard.frame.clear();
        guard.offset = 0;

        // 本帧：先登记再写；取消后由下一次 send 续写。
        guard.frame.reserve(4 + body.len());
        guard
            .frame
            .extend_from_slice(&(body.len() as u32).to_le_bytes());
        guard.frame.extend_from_slice(&body);
        write_pending(&mut guard).await?;
        guard.frame.clear();
        guard.offset = 0;

        match guard.stdin.as_mut() {
            Some(pipe) => pipe.flush().await.map_err(map_write_error),
            None => Err(TransportError::Closed),
        }
    }
}

/// 把 `state.frame[state.offset..]` 全部写进管道（`Interrupted` 重试）。
///
/// 中途失败 → 置 `broken`：管道里留下「帧前缀 + 半截帧体」，后续帧若继续
/// 追加就会被对端当成半帧的续体（误执行）。失败时**不清空** `frame`/`offset`
/// ——关停路径据此判定「必须强杀」（见 [`StdioConnection::shutdown`]）。
async fn write_pending(state: &mut WriteState) -> Result<(), TransportError> {
    let WriteState {
        stdin,
        frame,
        offset,
        broken,
    } = state;
    let Some(pipe) = stdin.as_mut() else {
        return Err(TransportError::Closed);
    };
    while *offset < frame.len() {
        match pipe.write(&frame[*offset..]).await {
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
impl ModuleConnection for StdioConnection {
    async fn send(&self, msg: HostToModule) -> Result<(), TransportError> {
        self.write_frame(&msg).await
    }

    async fn recv(&self) -> Result<ModuleToHost, TransportError> {
        self.read_frame().await
    }

    /// 关停语义（类比 SIGTERM → SIGKILL），**整体有界**：
    ///
    /// 1. 关闭宿主 → 模块方向（stdin EOF），让模块自行退出；
    /// 2. 等子进程退出，最多 `deadline`（已用掉的时间要扣掉）；
    /// 3. 没能优雅退出 → `start_kill()` 强杀 + **有界收尸**，返回
    ///    [`TransportError::Timeout`]。
    ///
    /// `deadline` 是**本函数总预算**，不是「每一步各自一份」。有界性靠两处
    /// 硬约束（旧实现会永久挂住，与「停止有界」的承诺冲突）：
    ///
    /// - **写方向锁可能被卡住的 `send` 长期占着**（对端不读 stdin → 管道写满，
    ///   `write` 一直等可写）。取写锁最多等
    ///   `min(deadline, WRITER_LOCK_GRACE)`，拿不到就不再尝试「优雅关 stdin」，
    ///   直接强杀：被杀的子进程关掉管道读端，卡住的写立刻以 `EPIPE` 失败，
    ///   写锁随之释放。**以「丢弃 timeout future」了事是错的**——那只让本函数
    ///   早退，子进程与写锁都还在，下一次关停照样挂、进程照样泄漏。
    /// - **半帧**（写方向有帧只送出去一部分：`send` 被取消 / 超时 / 写失败）：
    ///   此时管道里留着「帧前缀 + 半截帧体」，若只是清空缓冲并关 stdin，对端
    ///   可能把已到的字节当成完整帧的一部分而**误执行**。因此半帧 / `broken`
    ///   一律**强杀**，不给对端继续解析的机会。
    ///
    /// 重复调用：子进程已收割时直接 `Ok(())`（幂等）。
    async fn shutdown(&self, deadline: Duration) -> Result<(), TransportError> {
        let started = Instant::now();

        // 1) 有界地关掉写方向。返回值 = 「写方向留有残帧 / 写锁被卡住」，
        //    此时不能走「优雅 EOF」，只能强杀。
        let close_budget = deadline.min(WRITER_LOCK_GRACE);
        let damaged = match tokio::time::timeout(close_budget, self.writer.lock()).await {
            Ok(mut writer) => {
                // 半帧或已损坏：管道里留着残帧，绝不清空缓冲（清空 = 假装
                // 没发过，对端仍会读到被截断的帧）。保持原样，靠强杀收场。
                let damaged =
                    writer.broken || (writer.offset > 0 && writer.offset < writer.frame.len());
                if damaged {
                    // 残帧留在管道里（不清空 = 不假装没发过），并且**封死写方向**：
                    // 之后任何 `send` 直接 `Closed`，绝不把新帧追加到半帧后面
                    // 让对端把两帧连起来解析（那就是「误执行」）。
                    writer.broken = true;
                } else {
                    writer.stdin.take(); // EOF：让模块自己退出
                    writer.frame.clear();
                    writer.offset = 0;
                }
                damaged
            }
            // 拿不到写锁 = 有 send 卡在管道写入上。强杀是唯一有界的出路。
            Err(_) => {
                tracing::warn!(
                    "module stdin writer is stuck (peer is not reading); killing the process"
                );
                true
            }
        };

        // 2) 子进程已经退出（对端自己关的 / 崩溃）：直接收尸，不制造 Timeout。
        let mut guard = self.child.lock().await;
        let Some(mut child) = guard.take() else {
            return Ok(()); // 已关停 / 已收割：幂等
        };
        match child.try_wait() {
            Ok(Some(_status)) => return Ok(()),
            Ok(None) => {}
            Err(e) => return Err(TransportError::Io(format!("probe module exit: {e}"))),
        }

        // 3) 等它自己退出：未损坏 → 用剩余预算（优雅路径）；已损坏 → 只留一个
        //    很短的观察窗口（残帧补不齐，对端基本不会自己退，强杀才是出路）。
        let budget = if damaged {
            deadline
                .saturating_sub(started.elapsed())
                .min(DAMAGED_EXIT_GRACE)
        } else {
            deadline.saturating_sub(started.elapsed())
        };
        match tokio::time::timeout(budget, child.wait()).await {
            Ok(Ok(_status)) => return Ok(()),
            Ok(Err(e)) => return Err(TransportError::Io(format!("wait for module exit: {e}"))),
            Err(_elapsed) => {
                if !damaged {
                    tracing::warn!("module did not exit within the drain deadline; killing it");
                }
            }
        }

        // 4) 强杀 + 有界收尸。收尸超时也**不丢** `Child`：交给后台任务继续等，
        //    SIGKILL 已发，进程终将被回收——不留僵尸，也不让调用方无限等。
        let _ = child.start_kill();
        if tokio::time::timeout(REAP_GRACE, child.wait())
            .await
            .is_err()
        {
            tracing::warn!("module did not reap in time after SIGKILL; reaping in background");
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
        Err(TransportError::Timeout)
    }
}

/// stderr 转发：后台任务逐行读取，带模块 id 前缀转 `tracing::warn!`。
fn spawn_stderr_forwarder(module_id: &str, stderr: tokio::process::ChildStderr) {
    let module_id = module_id.to_string();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => tracing::warn!(module = %module_id, "[stderr] {}", line),
                Ok(None) => break, // EOF：子进程退出（或关闭了 stderr）
                Err(e) => {
                    tracing::debug!(module = %module_id, error = %e, "stderr read ended");
                    break;
                }
            }
        }
    });
}

/// 读错误映射：EOF / 管道破裂 = 对端退出（`Closed`）；其余 = IO 错误。
fn map_read_error(what: &str, e: std::io::Error) -> TransportError {
    match e.kind() {
        ErrorKind::UnexpectedEof | ErrorKind::BrokenPipe | ErrorKind::ConnectionReset => {
            TransportError::Closed
        }
        _ => TransportError::Io(format!("{what}: {e}")),
    }
}

/// 写错误映射：管道破裂 = 对端退出（`Closed`）；其余 = IO 错误。
fn map_write_error(e: std::io::Error) -> TransportError {
    match e.kind() {
        ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::UnexpectedEof => {
            TransportError::Closed
        }
        _ => TransportError::Io(format!("write frame: {e}")),
    }
}
