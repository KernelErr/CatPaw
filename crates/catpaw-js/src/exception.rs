//! Failures that become script exceptions.

use std::fmt;

use crate::value::Rooted;

/// An exception on its way to script. The backend turns each variant into
/// the corresponding script object (`DOMException`, `TypeError`, ...).
#[derive(Debug, Clone)]
pub enum Exception {
    /// A `DOMException` with the given name (`"NotFoundError"`, ...).
    Dom {
        name: &'static str,
        message: String,
    },
    Type(String),
    Range(String),
    /// A value thrown by script that Rust is propagating unchanged.
    Thrown(Rooted),
}

pub type Fallible<T> = Result<T, Exception>;

macro_rules! dom_ctor {
    ($($fn_name:ident => $name:literal),* $(,)?) => {
        impl Exception {
            $(
                pub fn $fn_name(message: impl Into<String>) -> Self {
                    Exception::Dom { name: $name, message: message.into() }
                }
            )*
        }
    };
}

dom_ctor! {
    index_size => "IndexSizeError",
    hierarchy_request => "HierarchyRequestError",
    wrong_document => "WrongDocumentError",
    invalid_character => "InvalidCharacterError",
    no_modification_allowed => "NoModificationAllowedError",
    not_found => "NotFoundError",
    not_supported => "NotSupportedError",
    in_use_attribute => "InUseAttributeError",
    invalid_state => "InvalidStateError",
    syntax => "SyntaxError",
    invalid_modification => "InvalidModificationError",
    namespace => "NamespaceError",
    invalid_access => "InvalidAccessError",
    security => "SecurityError",
    network => "NetworkError",
    abort => "AbortError",
    url_mismatch => "URLMismatchError",
    quota_exceeded => "QuotaExceededError",
    timeout => "TimeoutError",
    invalid_node_type => "InvalidNodeTypeError",
    data_clone => "DataCloneError",
    not_allowed => "NotAllowedError",
}

impl Exception {
    pub fn dom(name: &'static str, message: impl Into<String>) -> Self {
        Exception::Dom {
            name,
            message: message.into(),
        }
    }

    pub fn type_error(message: impl Into<String>) -> Self {
        Exception::Type(message.into())
    }

    pub fn range_error(message: impl Into<String>) -> Self {
        Exception::Range(message.into())
    }
}

impl fmt::Display for Exception {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Exception::Dom { name, message } => write!(f, "{name}: {message}"),
            Exception::Type(m) => write!(f, "TypeError: {m}"),
            Exception::Range(m) => write!(f, "RangeError: {m}"),
            Exception::Thrown(_) => write!(f, "uncaught script exception"),
        }
    }
}

impl std::error::Error for Exception {}
