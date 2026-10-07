//! The wording of results, in one table: the same thing is always said the
//! same way, which keeps results byte-stable (prompt caches) and gives one
//! place to tune what models read.

/// What went wrong, as the first word after `error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// The arguments do not fit the tool.
    BadArgument,
    /// No tab is open, or the one named is not.
    NoTab,
    /// The ref's element left the page.
    StaleRef,
    /// Nothing matches the target.
    NotFound,
    /// The element cannot take the action (disabled, hidden, not editable).
    NotActionable,
    /// Something else is on top of the element.
    Occluded,
    /// The page could not be loaded.
    NavigationFailed,
    /// The script threw or its promise was rejected.
    ScriptError,
    /// Not something this version can do.
    Unsupported,
    /// The engine failed while running the call; the tab is gone.
    Crashed,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::BadArgument => "BadArgument",
            ErrorCode::NoTab => "NoTab",
            ErrorCode::StaleRef => "StaleRef",
            ErrorCode::NotFound => "NotFound",
            ErrorCode::NotActionable => "NotActionable",
            ErrorCode::Occluded => "Occluded",
            ErrorCode::NavigationFailed => "NavigationFailed",
            ErrorCode::ScriptError => "ScriptError",
            ErrorCode::Unsupported => "Unsupported",
            ErrorCode::Crashed => "Crashed",
        }
    }
}

/// Advice lines: what to try next.
pub mod advice {
    pub const STALE: &str = "advice: use the suggested ref, or take a snapshot";
    pub const STALE_GONE: &str =
        "advice: the page changed since that ref was shown; use the latest snapshot";
    pub const UNKNOWN_REF: &str = "advice: refs come from this tab's snapshots; take one";
    pub const TARGET_SYNTAX: &str = "advice: a target is a ref (e12), css:<selector> or xy:<x>,<y>";
    pub const NO_TAB: &str = "advice: navigate to a URL first";
    pub const NOT_EDITABLE: &str =
        "advice: type into a text field or editable element; click the one that takes the text";
    pub const OCCLUDED: &str =
        "advice: dismiss what covers it (a banner or dialog), or scroll; then retry";
    pub const NOT_VISIBLE: &str =
        "advice: it has no size on screen; open the menu or section that holds it first";
    pub const DISABLED: &str = "advice: something on the page has to enable it first";
    pub const NOTHING_FOCUSED: &str = "advice: pass target, or click the field first";
    pub const NOT_CHECKABLE: &str =
        "advice: check and uncheck work on checkboxes, radio buttons and switches; click others";
    pub const NOT_A_SELECT: &str =
        "advice: select works on <select>; for custom dropdowns click the option instead";
    pub const SNAPSHOT_TRUNCATED: &str = "advice: snapshot({root:\"eN\"}) shows one part; filter:\"interactive\" shows controls only";
    pub const READ_CONTINUES: &str = "continues";
}

/// Words that start consequence lines (`! <word> ...`), in the order the
/// lines appear.
pub mod consequence {
    pub const NAVIGATED: &str = "navigated";
    pub const NAVIGATION_FAILED: &str = "navigation-failed";
    pub const POPUP: &str = "popup";
    pub const TAB_CLOSED: &str = "tab-closed";
    pub const DIALOG: &str = "dialog";
    pub const CONSOLE: &str = "console";
}
