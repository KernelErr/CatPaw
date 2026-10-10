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
    /// More than one element matches the target.
    AmbiguousTarget,
    /// The element cannot take the action (disabled, hidden, not editable).
    NotActionable,
    /// Something else is on top of the element.
    Occluded,
    /// The page could not be loaded.
    NavigationFailed,
    /// The script threw or its promise was rejected.
    ScriptError,
    /// What was waited for did not happen in time.
    Timeout,
    /// Not something this version can do.
    Unsupported,
    /// The tab is with the user (a hand-off) until they give it back.
    Busy,
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
            ErrorCode::AmbiguousTarget => "AmbiguousTarget",
            ErrorCode::NotActionable => "NotActionable",
            ErrorCode::Occluded => "Occluded",
            ErrorCode::NavigationFailed => "NavigationFailed",
            ErrorCode::ScriptError => "ScriptError",
            ErrorCode::Timeout => "Timeout",
            ErrorCode::Unsupported => "Unsupported",
            ErrorCode::Busy => "Busy",
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
    pub const TARGET_SYNTAX: &str = "advice: a target is a ref (e12), text:<visible text>, role \"name\" (button \"Sign in\"), css:<selector> or xy:<x>,<y>";
    pub const AMBIGUOUS: &str =
        "advice: pick one by its ref, or narrow it with role \"name\" (button \"Sign in\")";
    pub const FULL_NAME: &str = "advice: use the ref, or the name in full as the snapshot shows it";
    pub const OTHER_ROLE: &str = "advice: use the ref, or the role the snapshot shows";
    pub const NO_TAB: &str = "advice: navigate to a URL first";
    pub const HANDED_OVER: &str = "advice: the user finishes on that page or gives the tab back there (unfinished, if need be); wait({\"for\":\"handoff\"}) until then, or use another tab";
    pub const HANDOFF: &str = "advice: the user still has the tab; wait({\"for\":\"handoff\"}) again once they say they are done";
    pub const HANDOFF_LAPSED: &str = "advice: the tab is back as the user left it; take a snapshot, or call handoff again if they still need it";
    pub const NOT_EDITABLE: &str =
        "advice: type into a text field or editable element; click the one that takes the text";
    pub const OCCLUDED: &str =
        "advice: dismiss what covers it (a banner or dialog), or scroll; then retry";
    pub const NOT_VISIBLE: &str =
        "advice: it has no size on screen; open the menu or section that holds it first";
    pub const OUT_OF_REACH: &str = "advice: it sits outside what the page can scroll to; scroll the box that holds it (act scroll on that box), or click with force:true";
    pub const DISABLED: &str = "advice: something on the page has to enable it first";
    pub const MOVING: &str =
        "advice: it is animating; wait({\"for\":\"settled\"}), or click with force:true";
    pub const NOTHING_FOCUSED: &str = "advice: pass target, or click the field first";
    pub const NOT_CHECKABLE: &str =
        "advice: check and uncheck work on checkboxes, radio buttons and switches; click others";
    pub const NOT_A_SELECT: &str =
        "advice: select works on <select>; for custom dropdowns click the option instead";
    pub const SNAPSHOT_TRUNCATED: &str = "advice: snapshot({\"root\":\"eN\"}) opens a [collapsed] part, and with \"after\":\"eM\" the rest of a list; filter \"interactive\" shows controls only";
    pub const READ_CONTINUES: &str = "continues";
    pub const WAIT: &str =
        "advice: wait({\"for\":\"settled\"}) gives it more time, or wait for the text you expect";
    pub const IDLE: &str =
        "advice: it will not come on its own: act on the page, or wait for something else";
}

/// The first words of results that are neither `ok` nor `error`, and
/// what follows them.
pub mod outcome {
    /// The user must approve before the call can go on.
    pub const NEEDS_CONFIRMATION: &str = "needs_confirmation";
    /// Not allowed; retrying will not help.
    pub const BLOCKED: &str = "blocked";
    /// How to get a confirmation approved, before its URL.
    pub const ASK_USER: &str = "ask the user to approve at";
    /// The same, when its page was opened in the user's browser.
    pub const OPENED_FOR_USER: &str = "opened in the user's browser to approve at";
    /// A confirmation the user has not answered yet.
    pub const STILL_PENDING: &str = "(still pending)";
    /// The reason a policy gives for asking.
    pub const POLICY: &str = "policy";
    /// The reason when the user said no.
    pub const DECLINED: &str = "user: declined";
    /// The reason when the confirmation ran out.
    pub const EXPIRED: &str = "expired";
    /// The reason when the page no longer holds what was approved.
    pub const SUPERSEDED: &str = "superseded";
    /// The reason when the host's user dismissed the question.
    pub const CANCELLED: &str = "user: cancelled";
}

/// Words that start consequence lines (`! <word> ...`), in the order the
/// lines appear.
pub mod consequence {
    pub const NAVIGATION_FAILED: &str = "navigation-failed";
    pub const POPUP: &str = "popup";
    pub const DOWNLOAD: &str = "download";
    pub const TAB_CLOSED: &str = "tab-closed";
    pub const DIALOG: &str = "dialog";
    pub const NETWORK: &str = "network";
    pub const CONSOLE: &str = "console";
    pub const BLOCKED: &str = "blocked";
    pub const NOT_SETTLED: &str = "not-settled";

    /// The order consequence lines come in, whatever the order things
    /// happened in.
    pub const ORDER: &[&str] = &[
        NAVIGATION_FAILED,
        BLOCKED,
        POPUP,
        TAB_CLOSED,
        DOWNLOAD,
        DIALOG,
        NETWORK,
        CONSOLE,
        NOT_SETTLED,
    ];
}
