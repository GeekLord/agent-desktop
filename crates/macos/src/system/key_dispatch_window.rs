//! Key delivery to a named window, and the surface close a refused Escape
//! answers.
//!
//! Core's window route needs the named window to hold the exact focus. A menu
//! or a popover a person opened can sit in a window that is not focused, and
//! the key is then refused before anything is sent. The surface still has a
//! semantic close: `AXCancel` on the menu or popover itself. The states, in
//! this order: the key route delivers, unchanged; it refuses for focus and a
//! plain Escape finds an open surface in that window or its application, which
//! is cancelled and read back; otherwise the refusal stays as it was.

use agent_desktop_core::{
    ActionResult, AdapterError, Deadline, DeliverySemantics, InteractionLease, InteractionPolicy,
    KeyCombo, SystemOps, WindowInfo,
};

use super::{CANCEL, delivery_result, semantic_step, simple_key_action, surface_closed};
use crate::system::process::process_identity::ProcessIdentity;
use crate::tree::AXElement;

/// A key for a named window: core's focus-gated delivery, then, for a plain
/// Escape that gate refused, the close of the menu or popover showing in that
/// window or its application.
pub(crate) fn press_for_window<A: SystemOps + ?Sized>(
    adapter: &A,
    window: &WindowInfo,
    combo: &KeyCombo,
    policy: InteractionPolicy,
    lease: &InteractionLease,
) -> Result<ActionResult, AdapterError> {
    match agent_desktop_core::press_key_in_focused_window(adapter, window, combo, policy, lease) {
        Err(refusal) if answers_with_surface_close(combo, &refusal) => {
            answer_with_surface_close(refusal, || close_window_surface(window, lease.deadline()))
        }
        other => other,
    }
}

/// Whether a surface close answers this refusal: the key is a plain Escape,
/// which cancels the open surface, and the window focus gate refused before
/// anything was sent.
fn answers_with_surface_close(combo: &KeyCombo, refusal: &AdapterError) -> bool {
    simple_key_action(combo) == Some(CANCEL) && is_focus_refusal(refusal)
}

fn is_focus_refusal(error: &AdapterError) -> bool {
    let details = error.details.as_ref();
    error.disposition == DeliverySemantics::not_delivered()
        && details.and_then(|details| details["physical_delivery_started"].as_bool()) == Some(false)
        && details.and_then(|details| details["check"].as_str()) == Some("window_active")
}

/// The surface close is the answer when it closed something; with nothing open
/// the refusal stands, and an attempt that failed keeps its own error.
fn answer_with_surface_close(
    refusal: AdapterError,
    close: impl FnOnce() -> Result<Option<ActionResult>, AdapterError>,
) -> Result<ActionResult, AdapterError> {
    match close()? {
        Some(closed) => Ok(closed),
        None => Err(refusal),
    }
}

/// The identity checks and effects a window surface close depends on, split
/// out so its decision flow is exercised without live accessibility windows.
trait WindowSurfaceClose {
    fn expected_identity(&self, window: &WindowInfo) -> Result<ProcessIdentity, AdapterError>;
    fn discover(
        &self,
        identity: ProcessIdentity,
        window: &WindowInfo,
        deadline: Deadline,
    ) -> Result<Option<AXElement>, AdapterError>;
    fn require_generation(&self, identity: ProcessIdentity) -> Result<(), AdapterError>;
    fn surface_pid(&self, surface: &AXElement, deadline: Deadline) -> Result<i32, i32>;
    fn cancel(&self, surface: &AXElement, deadline: Deadline) -> Result<bool, AdapterError>;
    fn closed(&self, surface: &AXElement, deadline: Deadline) -> bool;
}

struct AxWindowSurface;

impl WindowSurfaceClose for AxWindowSurface {
    fn expected_identity(&self, window: &WindowInfo) -> Result<ProcessIdentity, AdapterError> {
        expected_window_identity(window)
    }

    fn discover(
        &self,
        identity: ProcessIdentity,
        window: &WindowInfo,
        deadline: Deadline,
    ) -> Result<Option<AXElement>, AdapterError> {
        crate::tree::surfaces::transient_surface_for_window(identity, window, deadline)
    }

    fn require_generation(&self, identity: ProcessIdentity) -> Result<(), AdapterError> {
        crate::tree::app_menu::require_generation(identity)
    }

    fn surface_pid(&self, surface: &AXElement, deadline: Deadline) -> Result<i32, i32> {
        crate::tree::properties::ax_ipc::pid(surface, deadline)
    }

    fn cancel(&self, surface: &AXElement, deadline: Deadline) -> Result<bool, AdapterError> {
        crate::actions::ax_helpers::try_ax_action_or_err(surface, CANCEL, deadline)
    }

    fn closed(&self, surface: &AXElement, deadline: Deadline) -> bool {
        surface_closed(surface, deadline)
    }
}

/// The process identity a window proves from its captured pid and instance
/// token, so a surface is never searched for under a freshly captured
/// generation.
fn expected_window_identity(window: &WindowInfo) -> Result<ProcessIdentity, AdapterError> {
    let instance = window
        .process_instance
        .as_deref()
        .filter(|instance| !instance.is_empty())
        .ok_or_else(|| {
            AdapterError::stale_ref_because("Window has no process-instance identity")
        })?;
    crate::system::process::process_identity::require_core(
        &agent_desktop_core::ProcessIdentity::new(window.pid, instance),
    )
}

/// Closes the menu or popover the named window or its application shows, and
/// reads that it closed. `Ok(None)` when none is open, or when the surface does
/// not take a cancel action.
fn close_window_surface(
    window: &WindowInfo,
    deadline: Deadline,
) -> Result<Option<ActionResult>, AdapterError> {
    close_window_surface_with(&AxWindowSurface, window, deadline)
}

fn close_window_surface_with(
    source: &impl WindowSurfaceClose,
    window: &WindowInfo,
    deadline: Deadline,
) -> Result<Option<ActionResult>, AdapterError> {
    let identity = source.expected_identity(window).map_err(not_delivered)?;
    let Some(surface) = source
        .discover(identity, window, deadline)
        .map_err(not_delivered)?
    else {
        return Ok(None);
    };
    verify_surface_owner(source, identity, &surface, deadline).map_err(not_delivered)?;
    if !source.cancel(&surface, deadline)? {
        return Ok(None);
    }
    let closed = source.closed(&surface, deadline);
    Ok(Some(delivery_result(
        semantic_step(CANCEL).with_verified(closed),
    )))
}

fn verify_surface_owner(
    source: &impl WindowSurfaceClose,
    identity: ProcessIdentity,
    surface: &AXElement,
    deadline: Deadline,
) -> Result<(), AdapterError> {
    source.require_generation(identity)?;
    match source.surface_pid(surface, deadline) {
        Ok(pid) if pid == identity.pid() => source.require_generation(identity),
        Ok(_) => Err(AdapterError::stale_ref_because(
            "Surface no longer belongs to the target process",
        )),
        Err(error) if error == accessibility_sys::kAXErrorInvalidUIElement => Err(
            AdapterError::stale_ref_because("Surface became invalid before cancel"),
        ),
        Err(error) => Err(crate::tree::surface_read::map_error("AXPid", error)
            .with_disposition(DeliverySemantics::not_delivered())),
    }
}

fn not_delivered(error: AdapterError) -> AdapterError {
    error.with_disposition(DeliverySemantics::not_delivered())
}

#[cfg(test)]
#[path = "key_dispatch_window_tests.rs"]
mod tests;
