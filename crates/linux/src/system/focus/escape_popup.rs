//! Escape for an application that does not own the active window.
//!
//! The key route sends the key only to the active window, so a key for a
//! background application is refused. A native popup that the application
//! opened stays open in the background on some toolkits, and the key cannot
//! reach it. Web documents are excluded, and a candidate must show an open
//! popup menu child and advertise a dismissal verb (or GTK 3 combo box `Press`,
//! L4-4) so Escape never activates a button's primary handler.
//!
//! Three states, in this order: the key route is allowed and nothing here
//! runs; the key route is refused for focus and a dismissible native popup
//! owner exists in the target window; otherwise the refusal stays as it was.
//! The owner's reply is the answer, so a rejected action delivers nothing and
//! an owner that does not answer leaves the delivery uncertain; the key
//! route's refusal replaces neither. Every stage re-checks that the element
//! still belongs to the original process instance, so a recycled process id
//! never receives the dismissal.

use std::time::Duration;

use agent_desktop_core::{
    Action, ActionResult, AdapterError, ChainRung, Deadline, DeliveryOutcome, DeliverySemantics,
    KeyCombo, ProcessIdentity, StepMechanism, WindowInfo,
};

use crate::actions::action_call::resolve_verb;
use crate::actions::chain::run_semantic_chain;
use crate::actions::process_check::{judge_frame, judge_owner};
use crate::actions::write::mutation::WriteReply;
use crate::system::process::process_facts;
use crate::system::process::process_lookup::Lookup;
use crate::system::window::window_id;
use crate::system::{SharedBus, owner_process_id, within};
use crate::tree::menu::observe_menu::rooted;
use crate::tree::properties::toolkit_trust::ToolkitKind;
use crate::tree::vocabulary::role_numbers::{
    COMBO_BOX, MENU, POPUP_MENU, PUSH_BUTTON, PUSH_BUTTON_MENU, TOGGLE_BUTTON, is_web_document,
};
use crate::tree::vocabulary::state_bits::{HAS_POPUP, SHOWING, StateBits};
use crate::tree::{AtspiElement, ObservedRead, WalkedNode};

#[path = "escape_popup_world.rs"]
mod world;

use world::PopupWorld;

const CLOSE_SETTLE: Duration = Duration::from_millis(1000);
const CLOSE_STEP: Duration = Duration::from_millis(50);
const FOCUS_CHECKS: [&str; 2] = ["window_active", "exact_window_focus"];
const SUGGESTION: &str = "Refresh the snapshot and check whether the popup is still open.";
const CLOSE_LABEL: &str = "Action.DoAction";
const DISMISS_VERBS: [&str; 3] = ["collapse", "cancel", "close"];
const GTK3_COMBO_DISMISS_VERBS: [&str; 4] = ["collapse", "cancel", "close", "press"];

pub(super) fn is_plain_escape(combo: &KeyCombo) -> bool {
    combo.modifiers.is_empty()
        && (combo.key.eq_ignore_ascii_case("escape") || combo.key.eq_ignore_ascii_case("esc"))
}

pub(super) fn is_focus_refusal(error: &AdapterError) -> bool {
    error
        .details
        .as_ref()
        .and_then(|details| details.get("check"))
        .and_then(|check| check.as_str())
        .is_some_and(|check| FOCUS_CHECKS.contains(&check))
}

fn bits(node: &WalkedNode<ObservedRead>) -> Option<StateBits<'_>> {
    node.payload
        .node
        .traits
        .states
        .present()
        .map(|words| StateBits::new(words))
}

fn role(node: &WalkedNode<ObservedRead>) -> Option<u32> {
    node.payload.node.traits.role.present().copied()
}

fn showing_popup(node: &WalkedNode<ObservedRead>) -> bool {
    !node.in_web_document
        && !is_web_document(role(node))
        && matches!(role(node), Some(MENU | POPUP_MENU))
        && bits(node).is_some_and(|bits| bits.has(SHOWING))
}

/// Whether a native node owns a popup that is visibly open now: a combo box,
/// or a button with `HAS_POPUP`, that has a showing `MENU` or `POPUP_MENU`
/// child. Web documents are excluded, and `EXPANDED` alone never qualifies an
/// owner. A tree row, an expander or a table cell is never an owner: Escape
/// does not collapse a tree.
pub(super) fn is_open_owner(node: &WalkedNode<ObservedRead>) -> bool {
    if node.in_web_document || is_web_document(role(node)) {
        return false;
    }
    let Some(bits) = bits(node) else {
        return false;
    };
    let owner = match role(node) {
        Some(COMBO_BOX) => true,
        Some(PUSH_BUTTON | PUSH_BUTTON_MENU | TOGGLE_BUTTON) => bits.has(HAS_POPUP),
        _ => false,
    };
    owner && node.children.iter().any(showing_popup)
}

/// The dismissal verbs Escape may invoke on an open popup owner. Generic
/// click/activate verbs are never used, and `press` is allowed only for a
/// native GTK 3 combo box (L4-4), whose `Press` action pops down its showing
/// menu child rather than firing a button's primary handler.
pub(super) fn close_verbs(role: Option<u32>, toolkit: ToolkitKind) -> &'static [&'static str] {
    match (role, toolkit) {
        (Some(COMBO_BOX), ToolkitKind::Gtk3) => &GTK3_COMBO_DISMISS_VERBS,
        _ => &DISMISS_VERBS,
    }
}

pub(super) fn deepest_open_owner<'a>(
    node: &'a WalkedNode<ObservedRead>,
    depth: usize,
    best: &mut Option<(usize, &'a WalkedNode<ObservedRead>)>,
) {
    if node.in_web_document || is_web_document(role(node)) {
        return;
    }
    if is_open_owner(node) && best.is_none_or(|(deepest, _)| depth > deepest) {
        *best = Some((depth, node));
    }
    for child in &node.children {
        deepest_open_owner(child, depth + 1, best);
    }
}

fn not_delivered(error: AdapterError) -> AdapterError {
    error.with_disposition(DeliverySemantics::not_delivered())
}

/// Refuses a live process table that no longer names the original instance,
/// so a recycled process id never receives the dismissal.
fn verify_live_process(expected: &ProcessIdentity, live: &Lookup) -> Result<(), AdapterError> {
    judge_frame(expected, expected.pid.get(), live).map_err(not_delivered)
}

/// Refuses a window recorded for another process or for another generation of
/// the target process.
fn verify_window_identity(
    expected: &ProcessIdentity,
    window: &WindowInfo,
) -> Result<(), AdapterError> {
    let same_instance = window.process_instance.as_deref() == Some(expected.instance.as_str());
    if window.pid == expected.pid && same_instance {
        return Ok(());
    }
    Err(not_delivered(AdapterError::stale_ref_because(
        "The window no longer belongs to the target process instance, so no popup was closed",
    )))
}

fn verify_element_pid(
    expected: &ProcessIdentity,
    pid: u32,
    live: &Lookup,
) -> Result<(), AdapterError> {
    judge_frame(expected, pid, live).map_err(not_delivered)
}

/// Refuses an element whose frame or address names another process, or whose
/// bus name the bus daemon now reports for another process.
fn verify_element_owner(
    bus: &SharedBus,
    expected: &ProcessIdentity,
    element: &AtspiElement,
    deadline: Deadline,
) -> Result<(), AdapterError> {
    let live = process_facts::lookup(expected.pid.get());
    let frame_pid = element.frame().map_or(element.pid(), AtspiElement::pid);
    verify_element_pid(expected, frame_pid, &live)?;
    verify_element_pid(expected, element.pid(), &live)?;
    let connection = bus.get(deadline).map_err(not_delivered)?;
    let owner = within(deadline, async {
        Ok(owner_process_id(connection.inner(), element.bus_name())
            .await
            .map_err(Some))
    })
    .map_err(not_delivered)?;
    judge_owner(expected.pid.get(), owner).map_err(not_delivered)?;
    verify_live_process(expected, &process_facts::lookup(expected.pid.get()))
}

fn frames_of(
    world: &impl PopupWorld,
    process: &ProcessIdentity,
    window: Option<&WindowInfo>,
    deadline: Deadline,
) -> Result<Vec<AtspiElement>, AdapterError> {
    let windows = match window {
        Some(window) => vec![window.clone()],
        None => {
            let windows = world.windows(process.pid.get(), deadline)?;
            verify_live_process(process, &world.live(process.pid.get()))?;
            for window in &windows {
                verify_window_identity(process, window)?;
            }
            windows
        }
    };
    windows
        .into_iter()
        .map(|window| frame_of(&window, process.pid.get()))
        .collect()
}

fn frame_of(window: &WindowInfo, pid: u32) -> Result<AtspiElement, AdapterError> {
    let (name, path) = window_id::decode(&window.id)?;
    Ok(rooted(&AtspiElement::new(name, path, pid)))
}

/// Whether the owner's popup is open, read after a dismissal. `None` when the
/// owner's states, a child's role or a popup child's states could not be
/// read, or the walk was cut short: a popup that could not be read never
/// counts as closed.
pub(super) fn popup_open(owner: &WalkedNode<ObservedRead>) -> Option<bool> {
    if owner.children.iter().any(showing_popup) {
        return Some(true);
    }
    let unread = bits(owner).is_none()
        || owner.truncated
        || owner.children.iter().any(|child| match role(child) {
            Some(MENU | POPUP_MENU) => bits(child).is_none(),
            Some(_) => false,
            None => true,
        });
    (!unread).then_some(false)
}

/// What the owner's reply settles. A reply of false delivered nothing, so the
/// popup is not read and cannot turn that reply into a delivery.
fn popup_outcome(reply: WriteReply, read_closed: impl FnOnce() -> Option<bool>) -> DeliveryOutcome {
    match reply {
        WriteReply::Absent => DeliveryOutcome::NotDelivered,
        WriteReply::Done => DeliveryOutcome::observed(read_closed()),
    }
}

/// The answer for a refused Escape: the popup attempt's own result, or its own
/// error, which already says whether that attempt may have landed. The refusal
/// stands only when the target window shows no dismissible open popup owner.
pub(super) fn popup_answer(
    refusal: AdapterError,
    popup: Result<Option<ActionResult>, AdapterError>,
) -> Result<ActionResult, AdapterError> {
    match popup {
        Ok(Some(settled)) => Ok(settled),
        Ok(None) => Err(refusal),
        Err(attempt) => Err(attempt),
    }
}

/// The refused Escape of a background application, answered by closing the
/// open native popup its target window shows when the owner advertises a safe
/// dismissal verb. `Ok(None)` leaves the refusal as it was; every other
/// result, the popup attempt's own failure included, is the answer.
pub(super) fn close_open_popup(
    bus: &SharedBus,
    process: &ProcessIdentity,
    window: Option<&WindowInfo>,
    combo: &KeyCombo,
    deadline: Deadline,
) -> Result<Option<ActionResult>, AdapterError> {
    close_popup_with(bus, process, window, combo, deadline)
}

fn close_popup_with(
    world: &impl PopupWorld,
    process: &ProcessIdentity,
    window: Option<&WindowInfo>,
    combo: &KeyCombo,
    deadline: Deadline,
) -> Result<Option<ActionResult>, AdapterError> {
    verify_live_process(process, &world.live(process.pid.get()))?;
    if let Some(window) = window {
        verify_window_identity(process, window)?;
    }
    let mut best: Option<(usize, AtspiElement, Option<u32>)> = None;
    for frame in frames_of(world, process, window, deadline).map_err(not_delivered)? {
        world
            .verify_owner(process, &frame, deadline)
            .map_err(not_delivered)?;
        let walk = world.walk(&frame, deadline).map_err(not_delivered)?;
        let mut found = None;
        deepest_open_owner(&walk, 0, &mut found);
        if let Some((depth, node)) = found
            && best.as_ref().is_none_or(|(deepest, _, _)| depth > *deepest)
        {
            best = Some((depth, node.payload.node.element.clone(), role(node)));
        }
    }
    let Some((_, owner, owner_role)) = best else {
        return Ok(None);
    };
    world
        .verify_owner(process, &owner, deadline)
        .map_err(not_delivered)?;
    let (names, verbs) = world
        .verbs(&owner, owner_role, deadline)
        .map_err(not_delivered)?;
    if resolve_verb(&names, verbs).is_none() {
        return Ok(None);
    }
    let run = || {
        world
            .verify_owner(process, &owner, deadline)
            .map_err(not_delivered)?;
        let reply = world.dismiss(&owner, &names, verbs, deadline)?;
        Ok(popup_outcome(reply, || {
            deadline.poll_until(
                CLOSE_SETTLE,
                CLOSE_STEP,
                || world.popup_still_open(&owner, deadline).map(|open| !open),
                |closed| *closed == Some(true),
            )
        }))
    };
    let mut rungs = [ChainRung {
        label: CLOSE_LABEL,
        effects: &[],
        mechanism: StepMechanism::SemanticApi,
        run: Box::new(run),
    }];
    run_semantic_chain(
        &Action::PressKey(combo.clone()),
        &mut rungs,
        deadline,
        SUGGESTION,
    )
    .map(Some)
}

#[cfg(test)]
#[path = "escape_popup_stage_tests.rs"]
mod stage_tests;

#[cfg(test)]
#[path = "escape_popup_tests.rs"]
mod tests;
