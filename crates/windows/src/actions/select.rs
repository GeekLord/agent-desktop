//! Select.
//!
//! A native combo box that stores item names selects by name through its own
//! window messages (`select_combo`); an owner-drawn one, or a name its search
//! misses, falls through to the route below. Any other target selects a named item, itself or one of
//! its realized descendants: LegacyIAccessible `Select(SELFLAG_TAKESELECTION)`
//! has no foreground effect on SysTreeView32, WinForms, WPF and DirectUI items
//! (C8), SelectionItemPattern is the headed rung, and the item's IsSelected
//! verifies either one. A target that is itself the named item selects
//! itself before anything else, so a collapsed tree item never expands. A collapsed container that is not a native combo box
//! (a WPF or Ribbon combo) shows its items only after it expands, and every
//! route there puts the popup over the user's window (C5, C6): headless
//! refuses it, and headed expands it first and collapses it again on failure.

use agent_desktop_core::{ActionStep, AdapterError, Deadline, ErrorCode, InteractionPolicy};
use std::time::Duration;

use crate::tree::element::UIAElement;

pub(crate) const SELECT_LABEL: &str = "SelectionItemPattern.Select";

const VERIFY_TIMEOUT: Duration = Duration::from_millis(600);

const SELECT_SUGGESTION: &str = "Use find or snapshot to inspect the available selection items.";

#[cfg(target_os = "windows")]
mod imp {
    use super::{
        ActionStep, AdapterError, Deadline, ErrorCode, InteractionPolicy, SELECT_LABEL,
        SELECT_SUGGESTION, UIAElement, VERIFY_TIMEOUT,
    };
    use crate::actions::chain::{after_delivery, legacy_select, offered, poll_until, rung};
    use crate::actions::click::SELECT_ITEM_LABEL;
    use crate::actions::disclosure::{EXPAND_LABEL, ExpandKind};
    use crate::actions::mutation::{classify_success, classify_write};
    use crate::actions::scroll::vertical_scroll;
    use crate::actions::select_combo::{COMBO_SELECT_LABEL, native_combo, select_native_combo};
    use crate::actions::select_locate::{Located, RealizeScan, RealizeScrollStep, locate};
    use crate::actions::select_search::{
        find_named_selection_item, name_matches, realize_scroll_step, scroll_to_position,
        selection_item_available,
    };
    use crate::tree::properties::property_ids::TreeProperty;
    use crate::tree::properties::read_one;
    use agent_desktop_core::DeliveryOutcome;
    use agent_desktop_core::DeliverySemantics;
    use uiautomation::patterns::{
        UIExpandCollapsePattern, UIScrollPattern, UISelectionItemPattern,
    };

    pub(crate) fn select_steps(
        element: &UIAElement,
        value: &str,
        policy: InteractionPolicy,
        deadline: Deadline,
    ) -> Result<Vec<ActionStep>, AdapterError> {
        deadline.ensure_remaining()?;
        if let Some(combo) = native_combo(element) {
            if let Some(outcome) = select_native_combo(combo, value, deadline)? {
                return Ok(vec![outcome.step(COMBO_SELECT_LABEL)]);
            }
        }
        if selection_item_available(element) && name_matches(element, value) {
            return select_item(element, policy, deadline);
        }
        let collapsed = is_collapsed_container(element);
        if collapsed && !policy.is_headed() {
            return Err(agent_desktop_core::mechanisms_denied(
                &[EXPAND_LABEL],
                policy,
            ));
        }
        if collapsed {
            expand_container(element)?;
        }
        let selected = find_item(element, value, policy, deadline)
            .and_then(|item| select_item(&item, policy, deadline));
        if collapsed && selected.is_err() {
            collapse_container(element);
        }
        selected
    }

    /// Finds the item, sweeping the scroll range when the policy allows the
    /// scroll that realizes a virtualized list's rows. A second match across
    /// the sweep is `AMBIGUOUS_TARGET`. The row is read from the window that
    /// realized it: a sweep that scrolled past the row returns the list to the
    /// window the row was sighted in and reads it again there, so a row that
    /// left the viewport is still found.
    fn find_item(
        element: &UIAElement,
        value: &str,
        policy: InteractionPolicy,
        deadline: Deadline,
    ) -> Result<UIAElement, AdapterError> {
        let mut source = ContainerScan::new(element, value, deadline);
        match locate(&mut source, policy.allow_focus_steal)? {
            Located::Found(item) => Ok(item),
            Located::ScrollDenied => Err(agent_desktop_core::mechanisms_denied(
                &[crate::actions::scroll::SCROLL_LABEL],
                policy,
            )),
            Located::Absent => Err(element_not_found(value)),
        }
    }

    /// The rows one container has realized, read as a scroll window over them.
    struct ContainerScan<'a> {
        element: &'a UIAElement,
        value: &'a str,
        deadline: Deadline,
        scroll: Option<UIScrollPattern>,
    }

    impl<'a> ContainerScan<'a> {
        fn new(element: &'a UIAElement, value: &'a str, deadline: Deadline) -> Self {
            Self {
                element,
                value,
                deadline,
                scroll: vertical_scroll(element),
            }
        }
    }

    impl RealizeScan for ContainerScan<'_> {
        type Item = UIAElement;

        fn scan(&mut self, prior: Option<&UIAElement>) -> Result<Option<UIAElement>, AdapterError> {
            find_named_selection_item(self.element, self.value, self.deadline, prior)
        }

        fn hides_rows_behind_a_scroll(&self) -> bool {
            self.scroll.is_some()
        }

        fn position(&self) -> Option<f64> {
            self.scroll
                .as_ref()
                .and_then(|pattern| pattern.get_vertical_scroll_percent().ok())
        }

        fn scroll_step(&mut self) -> Result<RealizeScrollStep, AdapterError> {
            match &self.scroll {
                Some(pattern) => realize_scroll_step(pattern, self.deadline),
                None => Ok(RealizeScrollStep::Stop),
            }
        }

        fn scroll_to(&mut self, position: f64) -> Result<bool, AdapterError> {
            match &self.scroll {
                Some(pattern) => scroll_to_position(pattern, position, self.deadline),
                None => Ok(false),
            }
        }
    }

    fn element_not_found(value: &str) -> AdapterError {
        AdapterError::new(
            ErrorCode::ElementNotFound,
            format!(
                "No selection item matched the requested value ({} chars)",
                value.chars().count()
            ),
        )
        .with_disposition(DeliverySemantics::not_delivered())
        .with_suggestion(SELECT_SUGGESTION)
    }

    fn select_item(
        item: &UIAElement,
        policy: InteractionPolicy,
        deadline: Deadline,
    ) -> Result<Vec<ActionStep>, AdapterError> {
        let observe = |delivered: bool| -> Result<DeliveryOutcome, AdapterError> {
            if !delivered {
                return Ok(DeliveryOutcome::NotDelivered);
            }
            let selected = poll_until(deadline, VERIFY_TIMEOUT, || {
                read_one(item, TreeProperty::SelectionItemIsSelected).flag() == Some(true)
            })
            .map_err(after_delivery)?;
            Ok(DeliveryOutcome::from_delivery(true, selected))
        };
        let mut legacy = || observe(legacy_select(item)?);
        let mut pattern = || observe(pattern_select(item)?);
        agent_desktop_core::execute_chain(
            &mut offered([
                rung(SELECT_ITEM_LABEL, true, false, &mut legacy),
                rung(
                    SELECT_LABEL,
                    selection_item_available(item),
                    true,
                    &mut pattern,
                ),
            ]),
            policy,
            deadline,
            SELECT_SUGGESTION,
        )
    }

    fn pattern_select(item: &UIAElement) -> Result<bool, AdapterError> {
        match item.0.get_pattern::<UISelectionItemPattern>() {
            Ok(pattern) => match pattern.select() {
                Ok(()) => classify_success(),
                Err(error) => classify_write("Select", SELECT_LABEL, &error),
            },
            Err(error) => classify_write("get_pattern", SELECT_LABEL, &error),
        }
    }

    fn expand_container(element: &UIAElement) -> Result<(), AdapterError> {
        let expanded = match element.0.get_pattern::<UIExpandCollapsePattern>() {
            Ok(pattern) => match pattern.expand() {
                Ok(()) => classify_success()?,
                Err(error) => classify_write("Expand", EXPAND_LABEL, &error)?,
            },
            Err(error) => classify_write("get_pattern", EXPAND_LABEL, &error)?,
        };
        if expanded {
            return Ok(());
        }
        Err(
            AdapterError::new(ErrorCode::ActionFailed, "The container did not expand")
                .with_disposition(DeliverySemantics::not_delivered())
                .with_suggestion(SELECT_SUGGESTION),
        )
    }

    fn collapse_container(element: &UIAElement) {
        if let Ok(pattern) = element.0.get_pattern::<UIExpandCollapsePattern>() {
            let _ = pattern.collapse();
        }
    }

    fn is_collapsed_container(element: &UIAElement) -> bool {
        read_one(element, TreeProperty::ExpandCollapseAvailable).flag() == Some(true)
            && matches!(
                read_one(element, TreeProperty::ExpandCollapseState)
                    .number()
                    .and_then(ExpandKind::from_i32),
                Some(ExpandKind::Collapsed)
            )
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    use super::{ActionStep, AdapterError, Deadline, InteractionPolicy, UIAElement};

    pub(crate) fn select_steps(
        _element: &UIAElement,
        _value: &str,
        _policy: InteractionPolicy,
        _deadline: Deadline,
    ) -> Result<Vec<ActionStep>, AdapterError> {
        Err(AdapterError::not_supported("Select"))
    }
}

pub(crate) use imp::select_steps;
