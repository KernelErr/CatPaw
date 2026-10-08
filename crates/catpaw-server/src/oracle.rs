//! The page's own style engine and form state, as the agent layer asks
//! for them.

use catpaw_agent::StyleOracle;
use catpaw_dom::{Dom, NodeId};
use catpaw_style::StyleEngine;
use catpaw_web::PageState;
use catpaw_web::agent;

/// Answers visibility and live-state questions from a page whose styles
/// are resolved (see [`catpaw_web::agent::with_styles`]).
pub(crate) struct EngineOracle<'a> {
    pub engine: &'a StyleEngine,
    pub page: &'a PageState,
}

impl StyleOracle for EngineOracle<'_> {
    fn is_display_none(&self, dom: &Dom, id: NodeId) -> bool {
        self.engine.is_display_none(dom, id)
    }

    fn is_visibility_hidden(&self, _dom: &Dom, id: NodeId) -> bool {
        self.engine.is_visibility_hidden(id)
    }

    fn control_value(&self, _dom: &Dom, id: NodeId) -> Option<String> {
        agent::control_value(self.page, id)
    }

    fn is_checked(&self, _dom: &Dom, id: NodeId) -> Option<bool> {
        Some(agent::is_checked(self.page, id))
    }

    fn is_option_selected(&self, _dom: &Dom, id: NodeId) -> Option<bool> {
        Some(agent::is_option_selected(self.page, id))
    }

    fn displayed_options(&self, _dom: &Dom, select: NodeId) -> Option<Vec<NodeId>> {
        Some(agent::selected_options(self.page, select))
    }

    fn is_pointer_cursor(&self, _dom: &Dom, id: NodeId) -> bool {
        self.engine.is_pointer_cursor(id)
    }

    fn is_block_level(&self, _dom: &Dom, id: NodeId) -> Option<bool> {
        self.engine.is_block_level(id)
    }

    fn has_activation_listener(&self, _dom: &Dom, id: NodeId) -> bool {
        agent::has_activation_listener(self.page, id)
    }
}
