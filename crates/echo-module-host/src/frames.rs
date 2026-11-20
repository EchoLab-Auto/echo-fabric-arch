//! 帧编解码：4 字节小端 `u32` 长度前缀 + JSON 帧体（**宿主侧唯一实现**）。
//!
//! 各运输层（stdio / dylib）的状态机不同（stdio 能强杀子进程，dylib 不能杀
//! 线程），但**线上字节必须逐位一致**。所以这里只放纯函数：编码一帧、从字节
//! 缓冲里切一帧。状态机留在各自的运输层。
//!
//! 语义要点（与模块侧 `echo-module-sdk::frame` 对称）：
//! - 前缀计的是**帧体**字节数，不含前缀自身；
//! - 单帧上限 [`MAX_FRAME_BYTES`]（16 MiB），读侧长度前缀超限即视为**流不可
//!   信**（帧边界丢了，后续字节无法再解释）；
//! - 解码失败**不消费**缓冲（帧边界完整时才能前进），由调用方决定是否继续。

use crate::api::{HostToModule, ModuleToHost};
use crate::transport::TransportError;

/// 单帧上限（16 MiB）。读侧长度前缀与写侧序列化结果均受此约束。
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// JSON 序列化 + 长度前缀 → 一帧完整字节。
pub(crate) fn encode(msg: &HostToModule) -> Result<Vec<u8>, TransportError> {
    let body = serde_json::to_vec(msg)
        .map_err(|e| TransportError::Protocol(format!("encode message: {e}")))?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(TransportError::Protocol(format!(
            "frame length {} exceeds limit {MAX_FRAME_BYTES}",
            body.len()
        )));
    }
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// 从缓冲头部切一帧的结果。
pub(crate) enum Step {
    /// 字节不够，继续读。
    Incomplete,
    /// 长度前缀本身不可信：帧边界已丢失，流不能再解释。带上是多少才越界。
    Fatal { len: usize },
    /// 切出一帧：`consumed` 是从缓冲头部要丢掉的字节数。
    ///
    /// `decoded` 为 `Err` 时**仍要消费** `consumed` 字节——帧边界是完整的，
    /// 只是内容不合法，下一帧还能正常解析。
    Frame {
        consumed: usize,
        decoded: Result<ModuleToHost, TransportError>,
    },
}

/// 尝试从缓冲头部切一帧。
pub(crate) fn step(buf: &[u8]) -> Step {
    if buf.len() < 4 {
        return Step::Incomplete;
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > MAX_FRAME_BYTES {
        return Step::Fatal { len };
    }
    if buf.len() < 4 + len {
        return Step::Incomplete;
    }
    let decoded = serde_json::from_slice(&buf[4..4 + len])
        .map_err(|e| TransportError::Protocol(format!("invalid JSON frame: {e}")));
    Step::Frame {
        consumed: 4 + len,
        decoded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> HostToModule {
        HostToModule::Invoke(crate::api::Invoke {
            call_id: "m:1".into(),
            contribution: "bash".into(),
            ctx: crate::api::InvokeContext::default(),
            payload: json!({"cmd": "ls"}),
        })
    }

    /// 编码 → 切帧 往返：线上字节必须能被自己解回来。
    ///
    /// 两个方向是**不同的枚举**（`HostToModule` / `ModuleToHost`），所以这里
    /// 各验一次，而不是拿 encode 的结果去和同一个值比。
    #[test]
    fn encode_then_step_roundtrips() {
        let frame = encode(&sample()).expect("encode");
        match step(&frame) {
            Step::Frame { consumed, decoded } => {
                assert_eq!(consumed, frame.len());
                // 编码的是 HostToModule，解码成 ModuleToHost 会失败——这本身是
                // 契约的一部分（方向不能混），但要证明**字节是完整的**。
                assert!(decoded.is_err(), "HostToModule 的帧不应被解成 ModuleToHost");
            }
            Step::Incomplete => panic!("完整的帧不应是 Incomplete"),
            Step::Fatal { .. } => panic!("合法的长度前缀不应是 Fatal"),
        }
    }

    /// 反方向：`ModuleToHost` 的帧要能被解回原值。
    #[test]
    fn module_to_host_roundtrips() {
        let message = ModuleToHost::Ready;
        let body = serde_json::to_vec(&message).expect("encode");
        let mut buf = (body.len() as u32).to_le_bytes().to_vec();
        buf.extend_from_slice(&body);
        match step(&buf) {
            Step::Frame { consumed, decoded } => {
                assert_eq!(consumed, buf.len());
                assert_eq!(decoded.expect("decode"), message);
            }
            _ => panic!("应切出一帧"),
        }
    }

    /// 前缀计的是帧体字节数，不含前缀自身。
    #[test]
    fn length_prefix_counts_body_only() {
        let frame = encode(&sample()).expect("encode");
        let declared = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
        assert_eq!(declared, frame.len() - 4);
    }

    /// 逐字节喂入：任何前缀都不应报 Fatal，直到凑满整帧才切出。
    #[test]
    fn partial_input_is_incomplete_not_fatal() {
        let frame = encode(&sample()).expect("encode");
        for cut in 0..frame.len() {
            match step(&frame[..cut]) {
                Step::Incomplete => {}
                other => panic!(
                    "截到 {cut} 字节时应是 Incomplete，实际 {}",
                    match other {
                        Step::Fatal { .. } => "Fatal",
                        Step::Frame { .. } => "Frame",
                        Step::Incomplete => unreachable!(),
                    }
                ),
            }
        }
    }

    /// 长度前缀超限 = 帧边界不可信（不是"再接更多字节就能好"）。
    #[test]
    fn oversized_prefix_is_fatal() {
        let mut buf = ((MAX_FRAME_BYTES as u32) + 1).to_le_bytes().to_vec();
        buf.extend_from_slice(b"{}");
        assert!(matches!(step(&buf), Step::Fatal { .. }));
    }

    /// 内容不合法时仍要消费掉整帧——否则下一帧永远解析不了。
    #[test]
    fn invalid_json_still_consumes_the_frame() {
        let body = b"not json";
        let mut buf = (body.len() as u32).to_le_bytes().to_vec();
        buf.extend_from_slice(body);
        match step(&buf) {
            Step::Frame { consumed, decoded } => {
                assert_eq!(consumed, buf.len());
                assert!(decoded.is_err());
            }
            _ => panic!("帧边界完整，应切出一帧"),
        }
    }

    /// 空长度帧是合法的 JSON 解析输入（会解码失败），不是 Fatal。
    #[test]
    fn empty_body_is_a_frame_not_fatal() {
        let buf = 0u32.to_le_bytes().to_vec();
        match step(&buf) {
            Step::Frame { consumed, decoded } => {
                assert_eq!(consumed, 4);
                assert!(decoded.is_err());
            }
            _ => panic!("0 长度不应是 Fatal / Incomplete"),
        }
    }
}
