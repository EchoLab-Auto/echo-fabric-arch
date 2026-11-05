//! 帧编解码：4 字节小端 `u32` 长度前缀 + JSON 帧体。
//!
//! 与宿主 `echo-module-host::stdio` 的帧实现一致：前缀计的是帧体字节数；
//! 单帧上限 [`MAX_FRAME_BYTES`]；读侧 `read_exact` 容忍「长度先到、帧体后到」
//! 的分段到达。

use tokio::io::{AsyncRead, AsyncReadExt};

use echo_module_api::{HostToModule, ModuleToHost};

use crate::error::SdkError;

/// 单帧上限（16 MiB）；与宿主 `echo-module-host::stdio::MAX_FRAME_BYTES` 同值。
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// 读一帧：长度前缀（4 字节 LE）→ 帧体 → JSON 解码。
///
/// 干净 EOF（帧边界上无数据）返回 `Ok(None)`；EOF 出现在帧中途视为读错误。
pub(crate) async fn read_frame<R>(reader: &mut R) -> Result<Option<HostToModule>, SdkError>
where
    R: AsyncRead + Unpin,
{
    let mut len_bytes = [0u8; 4];
    match reader.read_exact(&mut len_bytes).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(SdkError::Io(error)),
    }
    let len = u32::from_le_bytes(len_bytes) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(SdkError::Protocol(format!(
            "frame length {len} exceeds limit {MAX_FRAME_BYTES}"
        )));
    }

    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| SdkError::Protocol(format!("invalid JSON frame: {error}")))
}

/// 编码一帧（长度前缀 + 帧体）；序列化结果超限 → 协议错误。
pub(crate) fn encode_frame(msg: &ModuleToHost) -> Result<Vec<u8>, SdkError> {
    let body = serde_json::to_vec(msg)
        .map_err(|error| SdkError::Protocol(format!("encode message: {error}")))?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(SdkError::Protocol(format!(
            "frame length {} exceeds limit {MAX_FRAME_BYTES}",
            body.len()
        )));
    }

    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}
