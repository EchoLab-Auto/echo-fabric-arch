//! 生命周期与取消安全回归套件。
//!
//! 覆盖三组缺陷的**行为契约**（不依赖内部结构）：
//! 1. 运输层 `recv` 在帧中途被取消后，流仍然可解码（不丢半帧）；
//! 2. 监督循环因控制消息取消 `recv` 后，连接仍可继续服务；
//! 3. 排空 / 热替换按连接代际结算在途调用（错误明确、不误伤新实例）；
//! 4. 调用方丢弃 `invoke` future 不泄漏在途登记。
//!
//! 对端是 `src/bin/echo-module-test-peer.rs`（`--split-frames` = 逐 3 字节
//! 慢写，1ms/块，用来制造「读到一半」的确定性窗口）。

use std::time::{Duration, Instant};

use echo_module_host::api::*;
use echo_module_host::stdio::StdioTransport;
use echo_module_host::supervisor::{ModuleHandle, ModuleState, ModuleSupervisor};
use echo_module_host::transport::{ModuleSpec, Transport};
use serde_json::json;

/// 测试对端二进制路径（cargo 注入；[[bin]] 见 Cargo.toml）。
const PEER: &str = env!("CARGO_BIN_EXE_echo-module-test-peer");

fn peer() -> StdioTransport {
    StdioTransport::new(PEER)
}

fn spec() -> ModuleSpec {
    ModuleSpec {
        module_id: "recovery-test".to_string(),
        config: json!({"generation": 1}),
    }
}

fn ctx(deadline_ms: u64) -> InvokeContext {
    InvokeContext {
        deadline_ms: Some(deadline_ms),
        ..InvokeContext::default()
    }
}

async fn start_module(transport: StdioTransport) -> ModuleHandle {
    ModuleSupervisor::new()
        .start(spec(), Box::new(transport))
        .await
        .expect("handshake should succeed")
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

/// 1. 运输层直测：`recv` 在帧中途被 timeout 取消，流不得损坏。
///
/// 对端以 3 字节/ms 慢写；5ms 时第一条应答帧（Welcome，约百余字节）必然
/// 只到了一部分。旧实现把已读字节留在 `read_exact` 的局部缓冲里，取消即丢，
/// 下一次 `recv` 会把帧体当成长度前缀（通常直接判超限 / JSON 非法）。
/// 新实现把解码状态留在连接内，取消后从断点续读。
#[tokio::test]
async fn a_recv_cancel_mid_frame_keeps_stream_decodable() {
    let transport = peer().with_args(["--split-frames"]);
    let conn = transport.start(spec()).await.expect("start transport ok");

    conn.send(HostToModule::Hello(Hello {
        protocol: PROTOCOL_NAME.to_string(),
        version: PROTOCOL_VERSION,
        module_id: "recovery-test".to_string(),
        config: json!({}),
    }))
    .await
    .expect("send hello");

    // 5ms：帧只到一小段。
    let cancelled = tokio::time::timeout(Duration::from_millis(5), conn.recv()).await;
    assert!(
        cancelled.is_err(),
        "first recv should still be waiting for the split frame"
    );

    let mut welcomed = false;
    let mut registered = false;
    loop {
        match tokio::time::timeout(Duration::from_secs(5), conn.recv()).await {
            Ok(Ok(ModuleToHost::Welcome(w))) => {
                assert_eq!(w.module_id, "recovery-test");
                welcomed = true;
            }
            Ok(Ok(ModuleToHost::Register(r))) => {
                registered = !r.contributions.is_empty();
            }
            Ok(Ok(ModuleToHost::Ready)) => break,
            Ok(Ok(other)) => panic!("unexpected frame after cancelled recv: {other:?}"),
            Ok(Err(e)) => panic!("stream corrupted after cancelled recv: {e}"),
            Err(_) => panic!("timed out waiting for the rest of the handshake"),
        }
    }
    assert!(welcomed, "Welcome must survive cancellation");
    assert!(registered, "Register must survive cancellation");
}

/// 2. 监督循环：Ready 态收到 `Start` 会被拒绝（控制消息优先），该次 select
///    会取消正在进行的 `recv`。连接必须继续可用，在途调用不得变成崩溃。
#[tokio::test]
async fn b_rejected_start_during_frame_read_keeps_connection_usable() {
    let handle = start_module(peer().with_args(["--split-frames"])).await;

    let invoke = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .invoke("echo", ctx(10_000), json!({"text": "segmented"}))
                .await
        }
    });

    // 让对端先把结果帧写出一小段（3 字节/ms），再连发 Start 取消 recv。
    tokio::time::sleep(Duration::from_millis(10)).await;
    for _ in 0..8 {
        let rejected = handle.start().await;
        assert!(
            rejected.is_err(),
            "Start on a Ready module must be rejected, got {rejected:?}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    let out = tokio::time::timeout(Duration::from_secs(5), invoke)
        .await
        .expect("invoke must settle")
        .expect("invoke task join");
    assert_eq!(
        out,
        InvokeOutcome::ok("segmented"),
        "cancelled recv must not corrupt the stream"
    );
    assert_eq!(handle.state(), ModuleState::Ready);
    assert_eq!(handle.restarts(), 0, "no crash/restart should be observed");
}

/// 3a. `drain` 必须结算在途调用（旧实现要等各自的 600s 默认超时）。
#[tokio::test]
async fn c_drain_settles_in_flight_calls() {
    let handle = start_module(peer()).await;
    let invoke = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .invoke("echo", ctx(60_000), json!({"hang": true}))
                .await
        }
    });
    assert!(
        wait_for(Duration::from_secs(2), || handle.in_flight_calls() == 1).await,
        "invoke should be registered in flight"
    );

    let started = Instant::now();
    handle
        .drain(Duration::from_secs(2))
        .await
        .expect("drain should succeed");
    let out = tokio::time::timeout(Duration::from_secs(3), invoke)
        .await
        .expect("drain must settle the in-flight call")
        .expect("invoke task join");

    assert!(
        matches!(&out, InvokeOutcome::Error { code, .. } if code == "drained"),
        "expected Error{{code:\"drained\"}}, got {out:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "call was settled only by its own timeout: {:?}",
        started.elapsed()
    );
    assert_eq!(handle.in_flight_calls(), 0);
    assert_eq!(handle.state(), ModuleState::Stopped);
}

/// 3b. 热替换：被换下的旧实例里的在途调用按「旧代」结算，新实例不受影响。
#[tokio::test]
async fn d_upgrade_settles_retired_instance_calls_only() {
    let handle = start_module(peer()).await;
    let old_call = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .invoke("echo", ctx(60_000), json!({"hang": true}))
                .await
        }
    });
    assert!(
        wait_for(Duration::from_secs(2), || handle.in_flight_calls() == 1).await,
        "invoke should be registered in flight"
    );

    handle
        .upgrade(json!({"generation": 2}), Duration::from_secs(2))
        .await
        .expect("upgrade should succeed");

    let out = tokio::time::timeout(Duration::from_secs(3), old_call)
        .await
        .expect("upgrade must settle the retired instance's call")
        .expect("invoke task join");
    assert!(
        matches!(&out, InvokeOutcome::Error { code, .. } if code == "replaced"),
        "expected Error{{code:\"replaced\"}}, got {out:?}"
    );

    // 新实例照常服务；结算没有误伤新一代。
    let fresh = handle
        .invoke("echo", ctx(5_000), json!({"text": "new-instance"}))
        .await;
    assert_eq!(fresh, InvokeOutcome::ok("new-instance"));
    assert_eq!(handle.in_flight_calls(), 0);
}

/// 4. 调用方丢弃 `invoke` future（取消等待）必须撤销在途登记，不泄漏。
#[tokio::test]
async fn e_dropped_invoke_future_does_not_leak_pending() {
    let handle = start_module(peer()).await;
    let task = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .invoke("echo", ctx(60_000), json!({"hang": true}))
                .await
        }
    });
    assert!(
        wait_for(Duration::from_secs(2), || handle.in_flight_calls() == 1).await,
        "invoke should be registered in flight"
    );

    task.abort();
    let _ = task.await;

    assert!(
        wait_for(Duration::from_secs(2), || handle.in_flight_calls() == 0).await,
        "pending call leaked after the caller dropped the invoke future"
    );
    handle
        .drain(Duration::from_secs(2))
        .await
        .expect("drain after caller cancellation");
    assert_eq!(handle.in_flight_calls(), 0);
}

/// 5. 结算错误必须带可读原因（含 call_id 与代际），不能是空壳。
#[tokio::test]
async fn f_settlement_error_message_is_actionable() {
    let handle = start_module(peer()).await;
    let invoke = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .invoke("echo", ctx(60_000), json!({"hang": true}))
                .await
        }
    });
    assert!(wait_for(Duration::from_secs(2), || handle.in_flight_calls() == 1).await);

    handle
        .drain(Duration::from_secs(2))
        .await
        .expect("drain should succeed");
    let out = tokio::time::timeout(Duration::from_secs(3), invoke)
        .await
        .expect("call settled")
        .expect("join");
    let InvokeOutcome::Error { message, .. } = out else {
        panic!("expected Error, got {out:?}");
    };
    assert!(
        message.contains("recovery-test:1"),
        "message should name the call: {message}"
    );
    assert!(
        message.contains("generation"),
        "message should name the connection generation: {message}"
    );
}

/// 6. 运输层 `send` 在帧中途被取消：半帧留在连接内，下一次 `send` 先补完
///    它再写新帧，对端仍能按帧边界解析（不交错、不错位）。
#[tokio::test]
async fn g_send_cancel_mid_frame_keeps_frames_aligned() {
    let conn = peer().start(spec()).await.expect("start transport ok");
    let hello = |config: serde_json::Value| {
        HostToModule::Hello(Hello {
            protocol: PROTOCOL_NAME.to_string(),
            version: PROTOCOL_VERSION,
            module_id: "recovery-test".to_string(),
            config,
        })
    };

    // 2 MiB 配置远超管道容量：5ms 时必然只写出一部分。
    let blob = "x".repeat(2 * 1024 * 1024);
    let cancelled = tokio::time::timeout(
        Duration::from_millis(5),
        conn.send(hello(json!({"blob": blob}))),
    )
    .await;
    assert!(
        cancelled.is_err(),
        "send of a 2 MiB frame should still be blocked on the pipe"
    );

    // 第二帧会先补完半帧；对端因此能完整读到两个 Hello。
    conn.send(hello(json!({})))
        .await
        .expect("second send must complete the half frame first");

    for round in 0..2 {
        let mut welcomed = false;
        loop {
            match tokio::time::timeout(Duration::from_secs(10), conn.recv()).await {
                Ok(Ok(ModuleToHost::Welcome(_))) => welcomed = true,
                Ok(Ok(ModuleToHost::Ready)) => break,
                Ok(Ok(_)) => {}
                Ok(Err(e)) => panic!("frame misaligned in round {round}: {e}"),
                Err(_) => panic!("timed out in round {round}"),
            }
        }
        assert!(welcomed, "round {round} should see Welcome");
    }

    conn.shutdown(Duration::from_secs(2))
        .await
        .expect("shutdown ok");
}

/// 7. 热替换期间的**新代在途调用**不得被旧代结算误伤。
///
/// 旧实例用 `--stay-alive` 拖满排空期限，制造「旧代结算正在进行、新代已有
/// 在途调用」的确定窗口；断言旧调用得到 `replaced`、新调用照常拿到结果。
#[tokio::test]
async fn h_upgrade_settlement_does_not_touch_new_generation() {
    let handle = start_module(peer().with_args(["--stay-alive"])).await;

    let retired = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .invoke("echo", ctx(60_000), json!({"hang": true}))
                .await
        }
    });
    assert!(
        wait_for(Duration::from_secs(2), || handle.in_flight_calls() == 1).await,
        "retired instance call should be in flight"
    );

    // 旧实例无视 Drain，排空要等满 1s 才强杀——足够让新代登记调用。
    let upgrade = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .upgrade(json!({"generation": 2}), Duration::from_secs(1))
                .await
        }
    });
    assert!(
        wait_for(Duration::from_secs(2), || handle.generation() == 2).await,
        "traffic should switch to the replacement generation"
    );

    let fresh = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .invoke("echo", ctx(5_000), json!({"text": "new-generation"}))
                .await
        }
    });
    assert!(
        wait_for(Duration::from_secs(2), || handle.in_flight_calls() == 2).await,
        "the new-generation call must be in flight while the old generation is settling"
    );

    upgrade
        .await
        .expect("upgrade task join")
        .expect("upgrade should succeed");

    let retired_out = tokio::time::timeout(Duration::from_secs(3), retired)
        .await
        .expect("retired call must be settled")
        .expect("retired task join");
    assert!(
        matches!(&retired_out, InvokeOutcome::Error { code, .. } if code == "replaced"),
        "retired call should be replaced, got {retired_out:?}"
    );

    let fresh_out = tokio::time::timeout(Duration::from_secs(3), fresh)
        .await
        .expect("new-generation call must not be settled by the old generation")
        .expect("fresh task join");
    assert_eq!(
        fresh_out,
        InvokeOutcome::ok("new-generation"),
        "new-generation call must be served by the replacement instance"
    );
    assert_eq!(handle.in_flight_calls(), 0);
}
