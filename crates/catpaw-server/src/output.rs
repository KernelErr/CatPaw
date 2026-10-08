//! What a tool call returns, and how failures are worded.

use catpaw_protocol::wording::ErrorCode;

/// The result of a tool call: text (always), a PNG (screenshots), and
/// whether it is an error.
#[derive(Debug, Clone, Default)]
pub struct ToolOutput {
    pub text: String,
    pub image: Option<Vec<u8>>,
    pub is_error: bool,
    /// What the call started that the policy holds for the user's
    /// approval; the session asks for it.
    pub(crate) held: Option<Held>,
    /// Holds that went unsent during the call (replaced by newer ones, or
    /// gone with their document).
    pub(crate) dropped_holds: Vec<u64>,
    /// The call typed into a password field (journals keep the length).
    pub(crate) secret_input: bool,
}

/// What a call left held: the hold numbers (navigations and requests of
/// the tab's page) and what they would do (`submit → POST https://…`).
#[derive(Debug, Clone)]
pub(crate) struct Held {
    pub ids: Vec<u64>,
    pub what: String,
}

impl ToolOutput {
    pub fn ok(text: String) -> Self {
        Self {
            text,
            ..Self::default()
        }
    }
}

/// A failed call: its code, a one-line message, and lines that follow
/// (advice, a snapshot).
#[derive(Debug, Clone)]
pub struct Failure {
    pub code: ErrorCode,
    pub message: String,
    pub more: Vec<String>,
}

impl Failure {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            more: Vec::new(),
        }
    }

    /// Adds a line under the error line.
    pub fn with(mut self, line: impl Into<String>) -> Self {
        self.more.push(line.into());
        self
    }

    pub fn bad_argument(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::BadArgument, message)
    }

    pub fn render(&self) -> ToolOutput {
        let mut text = format!("error {}", self.code.as_str());
        if !self.message.is_empty() {
            text.push(' ');
            text.push_str(&self.message);
        }
        for line in &self.more {
            text.push('\n');
            text.push_str(line);
        }
        ToolOutput {
            text,
            is_error: true,
            ..ToolOutput::default()
        }
    }
}

pub type CallResult = Result<ToolOutput, Failure>;
