//! 内部错误类型：帧 / 协议失败在公开边界折叠为 [`std::io::Error`]。

use std::io;

/// SDK 内部错误（`serve_*` 统一映射为 [`std::io::Error`] 返回）。
#[derive(Debug, thiserror::Error)]
pub(crate) enum SdkError {
    /// 底层 IO 失败（读 / 写管道）。
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    /// 协议违规（帧超限 / JSON 非法 / 非预期消息）。
    #[error("protocol error: {0}")]
    Protocol(String),
}

impl From<SdkError> for io::Error {
    fn from(error: SdkError) -> Self {
        match error {
            SdkError::Io(error) => error,
            SdkError::Protocol(message) => io::Error::new(io::ErrorKind::InvalidData, message),
        }
    }
}
