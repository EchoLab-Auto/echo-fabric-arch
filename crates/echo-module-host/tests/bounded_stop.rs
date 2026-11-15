//! 「停止有界 / 调用有界」回归套件（第二轮验收发现项）。
//!
//! 上一轮只把 `invoke` 的 `result_rx` 包进了 deadline，并让 `drain_connection`
//! 无界地发 `Drain`、无界地等 `shutdown`。真实的模块（或恶意/挂死的模块）只要
//! **不读 stdin**，宿主侧的 `send` 就会阻塞在「等管道可写」上：
//!
//! - `invoke` 号称 `deadline_ms` 内返回，实际卡在发送阶段，永远不返回；
//! - `drain` / `stop` 号称有界，实际卡在 `Drain` 发送或 `shutdown` 的写锁上，
//!   进程不退出、句柄也不返回——「停止有界」直接破功。
//!
//! 本文件用**确定性**的阻塞源（`--no-stdin-read` 对端 + 4 MiB 大帧 > 64 KiB
//! 管道容量）压这条路径，不靠 sleep 抢时序。每个用例都有**总预算**
//! ([`TOTAL_BUDGET`])，一旦修复回退，用例会以「超预算」失败而不是把 CI 挂死；
//! 用例结束时还会断言**没有残留进程**（pid 文件 + `/proc` 观察）。

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use echo_module_host::api::*;
use echo_module_host::inproc::{InprocIo, InprocModule, InprocTransport};
use echo_module_host::stdio::StdioTransport;
use echo_module_host::supervisor::{ModuleHandle, ModuleState, ModuleSupervisor};
use echo_module_host::transport::{ModuleConnection, ModuleSpec, Transport, TransportError};
use serde_json::{json, Value};

/// 测试对端二进制路径（cargo 注入；[[bin]] 见 Cargo.toml）。
const PEER: &str = env!("CARGO_BIN_EXE_echo-module-test-peer");

/// 单个用例的总预算：超了就是失败（不许把测试挂死）。
const TOTAL_BUDGET: Duration = Duration::from_secs(60);

/// 「有界」的宽松断言阈值。真正的判据是「远小于 60s 总预算」：
/// 修复前这些路径是**永远不返回**，不是慢一点。
const BOUNDED: Duration = Duration::from_secs(10);

fn spec() -> ModuleSpec {
    ModuleSpec {
        module_id: "bounded-test".to_string(),
        config: json!({"generation": 1}),
    }
}

fn ctx(deadline_ms: u64) -> InvokeContext {
    InvokeContext {
        deadline_ms: Some(deadline_ms),
        ..InvokeContext::default()
    }
}

/// 4 MiB 的单帧载荷：远超 Linux 管道容量（64 KiB），对端不读 stdin 时
/// `send` **必然**阻塞（与调度顺序无关，是内核语义）。
fn big_payload() -> Value {
    json!({"text": "x".repeat(4 * 1024 * 1024)})
}

/// 用总预算包住整个用例体。
async fn run_bounded<F: Future>(fut: F) -> F::Output {
    match tokio::time::timeout(TOTAL_BUDGET, fut).await {
        Ok(value) => value,
        Err(_) => {
            panic!("test exceeded its total budget of {TOTAL_BUDGET:?} (bounded stop regressed?)")
        }
    }
}

/// 以 10ms 步进轮询条件，最多 `timeout`。
async fn wait_for(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if cond() {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ── 进程观测 / 清理 ───────────────────────────────────────────────────────

/// 子进程是否还活着（Linux：`/proc/<pid>` 存在；僵尸进程也算「还在」，
/// 因为僵尸意味着**没有收尸**——正是要抓的「资源不释放」）。
fn process_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        false
    }
}

/// 断言进程已消失（被杀 + 已收尸）；5s 内轮询，超时报错。
async fn assert_process_gone(pid: u32, what: &str) {
    let gone = wait_for(Duration::from_secs(5), || !process_alive(pid)).await;
    assert!(
        gone,
        "{what}: child process {pid} is still present (killed but not reaped?)"
    );
}

/// 兜底清理：用例 panic / 提前返回时也要把对端杀掉，不留残留进程。
///
/// 只在 Linux 且 `/proc/<pid>/cmdline` 确实是对端时动手——避免 pid 复用
/// 误杀无关进程。
struct PidGuard {
    pid: Option<u32>,
}

impl PidGuard {
    fn new(pid: u32) -> Self {
        Self { pid: Some(pid) }
    }

    /// 已经由被测代码收尸：解除兜底，避免 pid 复用误伤。
    fn disarm(&mut self) {
        self.pid = None;
    }
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        let Some(pid) = self.pid else { return };
        #[cfg(target_os = "linux")]
        {
            let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            if !String::from_utf8_lossy(&cmdline).contains("echo-module-test-peer") {
                return;
            }
            let _ = std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .status();
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = pid;
        }
    }
}

/// 起一个「握手之后就不读 stdin」的对端，并拿回它的 pid（写 pid 文件）。
///
/// 返回 `(handle, pid, pid_guard)`：调用方在断言进程已消失后 `disarm` 兜底。
async fn start_pipe_blocking_peer() -> (ModuleHandle, u32, PidGuard) {
    let pid_file = std::env::temp_dir().join(format!(
        "echo-peer-{}-{:?}.pid",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    let _ = std::fs::remove_file(&pid_file);
    let transport = StdioTransport::new(PEER)
        .with_args(["--no-stdin-read"])
        .with_args(["--pid-file".to_string(), pid_file.display().to_string()]);
    let handle = ModuleSupervisor::new()
        .start(spec(), Box::new(transport))
        .await
        .expect("handshake should succeed (the peer only stops reading afterwards)");
    let pid: u32 = std::fs::read_to_string(&pid_file)
        .expect("peer should write its pid file before the handshake")
        .trim()
        .parse()
        .expect("pid file should contain a number");
    let _ = std::fs::remove_file(&pid_file);
    assert!(
        process_alive(pid),
        "peer {pid} should be alive right after start"
    );
    (handle, pid, PidGuard::new(pid))
}

// ── 1. invoke：整体 deadline 覆盖「发送」而不只是「等结果」 ────────────────

/// 对端不读 stdin → 4 MiB 帧写不进去 → `send` 阻塞。
/// `deadline_ms = 300` 必须**在发送阶段**生效：300ms 量级返回
/// `Error{code:"timeout"}`，而不是永远挂着（修复前会挂死）。
#[tokio::test]
async fn a_invoke_deadline_covers_sending() {
    run_bounded(async {
        let (handle, pid, mut guard) = start_pipe_blocking_peer().await;

        let started = Instant::now();
        let out = tokio::time::timeout(BOUNDED, handle.invoke("echo", ctx(300), big_payload()))
            .await
            .expect("invoke must respect its deadline even when the frame cannot be written");
        let elapsed = started.elapsed();

        assert!(
            matches!(&out, InvokeOutcome::Error { code, .. } if code == "timeout"),
            "expected Error{{code:\"timeout\"}} from the send phase, got {out:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "invoke took {elapsed:?}: the send phase is not covered by the deadline"
        );
        assert_eq!(
            handle.in_flight_calls(),
            0,
            "pending must be removed on timeout"
        );
        assert_eq!(
            handle.state(),
            ModuleState::Ready,
            "a stuck write is not a crash"
        );

        // 收尾：排空也必须自己有界（这里对端仍在驻留，只能强杀）。
        let drained = tokio::time::timeout(BOUNDED, handle.drain(Duration::from_millis(500)))
            .await
            .expect("drain must be bounded even with a half-written frame in the pipe");
        assert!(
            matches!(drained, Err(TransportError::Timeout)),
            "drain should report the forced kill, got {drained:?}"
        );
        assert_eq!(handle.state(), ModuleState::Stopped);

        assert_process_gone(pid, "invoke timeout + drain").await;
        guard.disarm();
    })
    .await;
}

// ── 2. drain：`Drain` 发送 + shutdown 都受剩余预算，且真的杀掉进程 ─────────

/// 写方向被一个**卡住的 invoke** 长期占着（持有写锁）：
/// `drain` 必须仍在预算内返回，并且**真的把子进程杀掉收尸**——
/// 不是「timeout 丢 future」了事（那样进程与写锁都还在）。
#[tokio::test]
async fn b_drain_is_bounded_when_the_writer_is_stuck() {
    run_bounded(async {
        let (handle, pid, mut guard) = start_pipe_blocking_peer().await;

        // 后台塞 4 MiB：`send` 卡在管道写入上，写锁被它持有。
        let stuck = tokio::spawn({
            let handle = handle.clone();
            async move { handle.invoke("echo", ctx(60_000), big_payload()).await }
        });
        assert!(
            wait_for(Duration::from_secs(5), || handle.in_flight_calls() == 1).await,
            "the big invoke should be registered in flight"
        );
        // 给它 200ms 去把管道填满并真正卡在 write 上。
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            handle.in_flight_calls(),
            1,
            "the call must still be stuck, not finished"
        );

        let started = Instant::now();
        let drained = tokio::time::timeout(BOUNDED, handle.drain(Duration::from_millis(500)))
            .await
            .expect("drain must be bounded while a send holds the writer lock");
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(5),
            "drain took {elapsed:?}: the Drain send / shutdown is not bounded"
        );
        assert!(
            matches!(drained, Err(TransportError::Timeout)),
            "a peer that never reads stdin can only be killed, got {drained:?}"
        );
        assert_eq!(handle.state(), ModuleState::Stopped);

        // 进程被强杀 + 收尸：没有残留。
        assert_process_gone(pid, "drain with a stuck writer").await;

        // 卡住的调用必须在连接关停后立刻结束（不留 600s 悬挂）。
        let out = tokio::time::timeout(Duration::from_secs(5), stuck)
            .await
            .expect("the in-flight call must settle once the process is killed")
            .expect("invoke task join");
        assert!(
            matches!(out, InvokeOutcome::Error { .. }),
            "a killed connection cannot produce Ok, got {out:?}"
        );
        assert_eq!(handle.in_flight_calls(), 0);
        guard.disarm();
    })
    .await;
}

// ── 3. stop（可逆停止）同样有界，且之后还能重新拉起 ───────────────────────

#[tokio::test]
async fn c_reversible_stop_is_bounded_and_module_can_be_restarted() {
    run_bounded(async {
        let (handle, pid, mut guard) = start_pipe_blocking_peer().await;

        // 与 b 同样先卡住写方向：`stop` 走的是另一条控制分支
        // （`Control::Stop`），也要有界。
        let stuck = tokio::spawn({
            let handle = handle.clone();
            async move { handle.invoke("echo", ctx(60_000), big_payload()).await }
        });
        assert!(
            wait_for(Duration::from_secs(5), || handle.in_flight_calls() == 1).await,
            "the big invoke should be registered in flight"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;

        let started = Instant::now();
        let stopped = tokio::time::timeout(BOUNDED, handle.stop(Duration::from_millis(500)))
            .await
            .expect("stop must be bounded");
        assert!(
            matches!(stopped, Err(TransportError::Timeout)),
            "stop should report the forced kill, got {stopped:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "stop took {:?}",
            started.elapsed()
        );
        assert_eq!(handle.state(), ModuleState::Stopped);
        assert_process_gone(pid, "stop").await;
        // 卡住的调用随连接关停立刻结束。
        let out = tokio::time::timeout(Duration::from_secs(5), stuck)
            .await
            .expect("the stuck call must settle once the process is killed")
            .expect("invoke task join");
        assert!(
            matches!(out, InvokeOutcome::Error { .. }),
            "a killed connection cannot produce Ok, got {out:?}"
        );
        guard.disarm();

        // 可逆：监督任务还活着，能再拉起一个新实例（同一个 --no-stdin-read 对端）。
        let restarted = tokio::time::timeout(BOUNDED, handle.start())
            .await
            .expect("start after stop must be bounded");
        restarted.expect("module should restart after a bounded stop");
        assert_eq!(handle.state(), ModuleState::Ready);

        // 收尾（新实例的 pid 不再跟踪：drain 的有界性已由上面覆盖）。
        let _ = tokio::time::timeout(BOUNDED, handle.drain(Duration::from_millis(300))).await;
    })
    .await;
}

// ── 4. 运输层直测：半帧 + 不可回收的写 → 强制关闭，且之后不再追加新帧 ─────

/// `send` 被取消后留下**半帧**（写方向状态在连接内，取消安全契约）。
/// 此时 `shutdown` 不得「清空缓冲 + 关 stdin」了事（对端会读到被截断的帧），
/// 而要**强杀**；强杀后写方向封死，后续 `send` 一律 `Closed`
/// ——绝不把新帧追加到半帧后面让对端连起来解析（误执行）。
#[tokio::test]
async fn d_half_frame_shutdown_force_closes_and_seals_the_stream() {
    run_bounded(async {
        let pid_file = std::env::temp_dir().join(format!(
            "echo-peer-half-{}-{:?}.pid",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        let _ = std::fs::remove_file(&pid_file);
        let transport = StdioTransport::new(PEER)
            .with_args(["--no-stdin-read"])
            .with_args(["--pid-file".to_string(), pid_file.display().to_string()]);
        let conn = transport.start(spec()).await.expect("start transport ok");

        // 直接做握手（对端在 Hello 之后就不读 stdin 了）。
        conn.send(HostToModule::Hello(Hello {
            protocol: PROTOCOL_NAME.to_string(),
            version: PROTOCOL_VERSION,
            module_id: "bounded-test".to_string(),
            config: json!({}),
        }))
        .await
        .expect("send hello");
        let mut ready = false;
        for _ in 0..3 {
            match tokio::time::timeout(Duration::from_secs(5), conn.recv()).await {
                Ok(Ok(ModuleToHost::Ready)) => {
                    ready = true;
                    break;
                }
                Ok(Ok(_)) => continue,
                Ok(Err(e)) => panic!("handshake failed: {e}"),
                Err(_) => panic!("handshake timed out"),
            }
        }
        assert!(ready, "peer should complete the handshake");

        let pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("peer should write its pid file")
            .trim()
            .parse()
            .expect("pid file should contain a number");
        let mut guard = PidGuard::new(pid);

        // 4 MiB 帧：200ms 后取消 → 管道里留下半帧（写方向状态留在连接内）。
        let big = HostToModule::Invoke(Invoke {
            call_id: "bounded-test:half-frame".to_string(),
            contribution: "echo".to_string(),
            ctx: ctx(60_000),
            payload: big_payload(),
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(200), conn.send(big))
                .await
                .is_err(),
            "the 4 MiB frame cannot fit into a 64 KiB pipe the peer never reads"
        );

        // 期限给 5s：有半帧时**不该**白等这 5s 的「优雅退出」（对端永远补不齐
        // 这一帧），而应立即强杀——所以断言 1s 内返回（旧实现会等满 5s）。
        let started = Instant::now();
        let res = tokio::time::timeout(BOUNDED, conn.shutdown(Duration::from_secs(5)))
            .await
            .expect("shutdown must be bounded with a half frame in the pipe");
        let elapsed = started.elapsed();
        assert!(
            matches!(res, Err(TransportError::Timeout)),
            "a half frame forces a kill, got {res:?}"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "shutdown waited {elapsed:?} for a graceful exit that cannot happen; \
             a half frame must be force-closed right away"
        );
        assert_process_gone(pid, "half-frame shutdown").await;

        // 写方向已封死：新帧不会被追加到半帧后面。
        assert!(
            matches!(
                conn.send(HostToModule::Dispose).await,
                Err(TransportError::Closed)
            ),
            "sends must be rejected after a half frame was abandoned"
        );
        // 幂等：再次 shutdown 直接 Ok。
        conn.shutdown(Duration::from_secs(1))
            .await
            .expect("second shutdown should be ok");
        guard.disarm();
    })
    .await;
}

// ── 5. inproc 轨道同一缺陷：模块不读通道时 shutdown 也必须有序收场 ─────────

/// in-proc 模块**永不读通道**：容量 64 的通道写满后，宿主侧第 65 次 `send`
/// 会卡在 `Sender::send` 上，并一直占着 `to_module` 锁。旧实现里
/// `shutdown` 无条件 `lock().await` —— 关停被永久挂住。
#[tokio::test]
async fn e_inproc_shutdown_is_bounded_when_the_module_stops_reading() {
    run_bounded(async {
        struct DeafModule {
            dropped: Arc<AtomicBool>,
        }
        impl Drop for DeafModule {
            fn drop(&mut self) {
                self.dropped.store(true, Ordering::SeqCst);
            }
        }
        impl InprocModule for DeafModule {
            fn run(self: Box<Self>, io: InprocIo) -> Pin<Box<dyn Future<Output = ()> + Send>> {
                // 永不 recv，但**持有**接收端（丢掉 io 就等于关闭通道）：
                // 通道写满后宿主的写方向就堵死了。
                Box::pin(async move {
                    let _keep = (self, io);
                    std::future::pending::<()>().await
                })
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let transport = InprocTransport::new({
            let dropped = Arc::clone(&dropped);
            move |_spec: ModuleSpec| -> Box<dyn InprocModule> {
                Box::new(DeafModule {
                    dropped: Arc::clone(&dropped),
                })
            }
        });
        let conn: Arc<dyn ModuleConnection> =
            Arc::from(transport.start(spec()).await.expect("start"));

        // 填满通道：前 CAPACITY 次成功，第 CAPACITY+1 次阻塞（并占着锁）。
        for _ in 0..echo_module_host::inproc::CHANNEL_CAPACITY {
            conn.send(HostToModule::Dispose)
                .await
                .expect("buffer has room");
        }
        let blocked = tokio::spawn({
            let conn = Arc::clone(&conn);
            async move { conn.send(HostToModule::Dispose).await }
        });
        // 给这个 send 一点时间去真正卡在 `Sender::send` 上（拿到 to_module 锁）。
        tokio::time::sleep(Duration::from_millis(100)).await;

        let started = Instant::now();
        let res = tokio::time::timeout(BOUNDED, conn.shutdown(Duration::from_millis(300)))
            .await
            .expect("inproc shutdown must be bounded while a send holds the write lock");
        assert!(
            matches!(res, Err(TransportError::Timeout)),
            "a module that never reads can only be aborted, got {res:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "inproc shutdown took {:?}",
            started.elapsed()
        );

        // 卡住的 send 随 abort 立刻失败（接收端被释放）。
        let stuck = tokio::time::timeout(Duration::from_secs(3), blocked)
            .await
            .expect("the blocked send must settle once the module task is aborted")
            .expect("send task join");
        assert!(
            matches!(stuck, Err(TransportError::Closed)),
            "expected Closed, got {stuck:?}"
        );
        // 模块任务真的被撤销并析构（没有后台残骸）。
        assert!(
            dropped.load(Ordering::SeqCst),
            "the module task must be aborted and dropped by shutdown"
        );
    })
    .await;
}
