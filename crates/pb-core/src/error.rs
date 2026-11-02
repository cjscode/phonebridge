use serde::Serialize;

/// Machine-readable error category, so the UI can pick an icon / action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Network,
    NotLoggedIn,
    PhoneNotSetUp,
    PhoneUnreachable,
    PhoneNeedsUsb,
    AdbUnauthorized,
    Adb,
    NoUsbDevice,
    MultipleUsbDevices,
    Scrcpy,
    MissingBinary,
    BadInput,
    Io,
    Internal,
}

/// An error shown to the user: what happened, plus an optional hint on how to fix it.
#[derive(Debug, Clone, Serialize, thiserror::Error)]
#[error("{message}")]
pub struct AppError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl AppError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: None,
        }
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        AppError::new(ErrorCode::Io, e.to_string())
    }
}

pub type Result<T, E = AppError> = std::result::Result<T, E>;
