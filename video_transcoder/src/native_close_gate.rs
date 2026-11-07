//! Native close requests retain the worker until the engine proves local settlement and joins.
//! This gate does not turn remote uncertainty into permission to reuse the remote generation.

use crate::engine::{Controller, ShutdownPoll, WorkerOwner};
use std::sync::{
    Arc, Mutex, TryLockError,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone)]
pub(crate) struct NativeCloseGate {
    controller: Controller,
    owner: Arc<Mutex<WorkerOwner>>,
    requested: Arc<AtomicBool>,
}

impl NativeCloseGate {
    pub(crate) fn new(controller: Controller, owner: WorkerOwner) -> Self {
        Self {
            controller,
            owner: Arc::new(Mutex::new(owner)),
            requested: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn controller(&self) -> Controller {
        self.controller.clone()
    }

    /// Called before the framework's default close behavior, or from the custom Quit menu item.
    pub(crate) fn request_close(&self) {
        self.requested.store(true, Ordering::Release);
        self.controller.request_shutdown();
    }

    pub(crate) fn is_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    /// Never blocks on another observer. The owner and its join capability remain retained.
    /// The returned engine report is inspected only after the mutex guard has been dropped.
    pub(crate) fn poll_shutdown(&self) -> Option<ShutdownPoll> {
        if !self.is_requested() {
            return None;
        }
        Some(match self.owner.try_lock() {
            Ok(mut owner) => owner.poll_shutdown(),
            Err(TryLockError::WouldBlock) => ShutdownPoll::Pending,
            Err(TryLockError::Poisoned(_)) => {
                ShutdownPoll::LocalUncertain("worker_owner_unavailable")
            }
        })
    }
}

#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
pub(crate) const CLOSE_MENU_ID: &str = "gs-transcoder-close";
#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
pub(crate) const QUIT_MENU_ID: &str = "gs-transcoder-quit";

/// Preserve Dioxus 0.7.10's Window/Edit/debug menus, replacing only the native Close and Quit
/// actions. Native predefined Quit can terminate the event loop before a task has settled.
#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
pub(crate) fn gated_menu() -> Result<dioxus::desktop::muda::Menu, dioxus::desktop::muda::Error> {
    use dioxus::desktop::muda::{
        Menu, MenuItem, PredefinedMenuItem, Submenu,
        accelerator::{Accelerator, Code, Modifiers},
    };

    let menu = Menu::new();
    let window = Submenu::new("Window", true);
    #[cfg(target_os = "macos")]
    let close_accelerator = Accelerator::new(Some(Modifiers::META), Code::KeyW);
    #[cfg(not(target_os = "macos"))]
    let close_accelerator = Accelerator::new(Some(Modifiers::ALT), Code::F4);
    #[cfg(target_os = "macos")]
    let quit_accelerator = Some(Accelerator::new(Some(Modifiers::META), Code::KeyQ));
    #[cfg(not(target_os = "macos"))]
    let quit_accelerator = None;
    let close = MenuItem::with_id(
        CLOSE_MENU_ID,
        PredefinedMenuItem::close_window(None).text(),
        true,
        Some(close_accelerator),
    );
    let quit = MenuItem::with_id(
        QUIT_MENU_ID,
        PredefinedMenuItem::quit(None).text(),
        true,
        quit_accelerator,
    );
    window.append_items(&[
        &PredefinedMenuItem::fullscreen(None),
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::hide(None),
        &PredefinedMenuItem::hide_others(None),
        &PredefinedMenuItem::show_all(None),
        &PredefinedMenuItem::maximize(None),
        &PredefinedMenuItem::minimize(None),
        &close,
        &PredefinedMenuItem::separator(),
        &quit,
    ])?;
    let edit = Submenu::new("Edit", true);
    edit.append_items(&[
        &PredefinedMenuItem::undo(None),
        &PredefinedMenuItem::redo(None),
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::cut(None),
        &PredefinedMenuItem::copy(None),
        &PredefinedMenuItem::paste(None),
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::select_all(None),
    ])?;
    menu.append_items(&[&window, &edit])?;
    if cfg!(debug_assertions) {
        let help = Submenu::new("Help", true);
        help.append_items(&[
            &MenuItem::with_id(
                "dioxus-toggle-dev-tools",
                "Toggle Developer Tools",
                true,
                None,
            ),
            &MenuItem::with_id(
                "dioxus-float-top",
                "Float on Top (dev mode only)",
                true,
                None,
            ),
        ])?;
        menu.append_items(&[&help])?;
        #[cfg(target_os = "macos")]
        help.set_as_help_menu_for_nsapp();
    }
    #[cfg(target_os = "macos")]
    window.set_as_windows_menu_for_nsapp();
    Ok(menu)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn native_close_gate_retains_real_owner_and_controller_clones_until_joined() {
        let (controller, owner) = Controller::start_owned();
        let extra_controller = controller.clone();
        let gate = NativeCloseGate::new(controller, owner);
        let event_loop_owner = gate.clone();
        assert!(!gate.is_requested());
        assert!(gate.poll_shutdown().is_none());
        let held_owner = gate.owner.lock().unwrap();
        // A close request does not wait for a UI observer holding the owner mutex.
        event_loop_owner.request_close();
        event_loop_owner.request_close();
        assert!(gate.is_requested());
        assert!(matches!(gate.poll_shutdown(), Some(ShutdownPoll::Pending)));
        drop(held_owner);
        drop(event_loop_owner);
        // Keeping an ordinary Controller sender alive must not prevent irreversible shutdown.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match gate.poll_shutdown().unwrap() {
                ShutdownPoll::Pending => {
                    assert!(Instant::now() < deadline, "real worker did not join");
                    std::thread::sleep(Duration::from_millis(5));
                }
                ShutdownPoll::LocalUncertain(code) => panic!("idle cleanup uncertain: {code}"),
                ShutdownPoll::Joined(report) => {
                    assert!(report.remote_uncertain().is_none());
                    assert!(report.operational_failure().is_none());
                    assert!(report.benchmark_ms().is_none());
                    break;
                }
            }
        }
        assert!(matches!(
            gate.poll_shutdown(),
            Some(ShutdownPoll::Joined(_))
        ));
        drop(extra_controller);
    }
}
