//! 操作不能を表すエラー。提供者へ表示してよいメッセージを運ぶ。

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
#[error("{0}")]
pub struct AgentError(pub String);

impl AgentError {
    pub fn new(message: impl Into<String>) -> Self {
        AgentError(message.into())
    }
}

pub type Result<T> = std::result::Result<T, AgentError>;
