// Copyright © 2026 ComfyHome™
// All rights reserved.
//
// Licensed under the ComfyGit SA-PS License
//
// For details, see the LICENSE file in the repository root.

//! Minimal TUI overlay that shows toast notifications for git commands
//! executed from the CLI. The command runs in a scoped background thread
//! while a lightweight ratatui render loop on the main thread displays
//! real-time toast updates. After the overlay exits, the terminal is
//! restored to its original state.

use std::{
    io::{self, Write, stdout},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend, layout::Rect, widgets::WidgetRef};
use ratatui_comfy_toaster::{
    ToastBuilder, ToastEngine, ToastEngineBuilder, ToastMouseButton, ToastShortcut, ToastType,
    ToastUpdate,
};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::git::{GitToastEvent, GitToastEventKind};

/// How often the overlay loop ticks (drains events + renders).
const TICK_INTERVAL: Duration = Duration::from_millis(50);

/// Maximum time to wait after the action completes before force-closing
/// the overlay (gives user time to read success toasts).
const LINGER_DURATION: Duration = Duration::from_secs(3);

/// Runs `action` in a scoped background thread while displaying a minimal
/// toast overlay on the current thread. Returns the action's result.
///
/// Uses `thread::scope` so the action closure can borrow from the calling
/// scope without needing `'static`. The overlay enters the alternate screen,
/// drains git toast events, and renders them in real-time. After the action
/// completes, the overlay lingers briefly so the user can read the final
/// toast, then restores the terminal.
pub(crate) fn run_with_toast_overlay<T: Send>(
    mut rx: UnboundedReceiver<GitToastEvent>,
    action: impl FnOnce() -> Result<T> + Send,
) -> Result<T> {
    let result_slot = Arc::new(Mutex::new(None::<Result<T>>));
    let result_slot_clone = result_slot.clone();
    let action_done = Arc::new(Mutex::new(false));
    let action_done_clone = action_done.clone();

    // Set up the terminal for the overlay on the main thread.
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout_handle = stdout();
    execute!(stdout_handle, EnterAlternateScreen).context("failed to enter alternate screen")?;

    let terminal_result = thread::scope(|scope| {
        // Spawn the action in a scoped thread (can borrow from caller).
        scope.spawn(move || {
            let result = action();
            if let Ok(mut done) = action_done_clone.lock() {
                *done = true;
            }
            if let Ok(mut slot) = result_slot_clone.lock() {
                *slot = Some(result);
            }
        });

        // Run the overlay render loop on the main thread.
        (|| {
            let mut terminal = Terminal::new(CrosstermBackend::new(stdout_handle))
                .context("failed to create terminal")?;

            let mut engine: ToastEngine<()> = ToastEngineBuilder::new(Rect::new(0, 0, 120, 40))
                .default_duration(Duration::from_secs(3))
                .default_progress_bar(true)
                .build();

            let mut toast_ids: std::collections::HashMap<u64, u64> =
                std::collections::HashMap::new();
            let mut action_completed_at: Option<Instant> = None;

            loop {
                // Check if the action is done.
                let done = action_done.lock().map(|d| *d).unwrap_or(false);
                if done && action_completed_at.is_none() {
                    action_completed_at = Some(Instant::now());
                }

                // Drain toast events.
                while let Ok(event) = rx.try_recv() {
                    match event.kind {
                        GitToastEventKind::Started { args, timeout_secs } => {
                            let label = format!("git {}", args.join(" "));
                            let builder = ToastBuilder::new(label.into())
                                .toast_type(ToastType::Info)
                                .duration(Duration::from_secs(timeout_secs))
                                .show_progress_bar(true);
                            let toast_id = engine.show_toast_with_id(builder);
                            toast_ids.insert(event.command_id, toast_id);
                        }
                        GitToastEventKind::Finished { success, stderr } => {
                            if let Some(toast_id) = toast_ids.remove(&event.command_id) {
                                if success {
                                    engine.update_toast_by_id(
                                        toast_id,
                                        ToastUpdate::new()
                                            .toast_type(ToastType::Success)
                                            .message("git: SUCCESS")
                                            .duration(Some(Duration::from_secs(2)))
                                            .show_progress_bar(false),
                                    );
                                } else {
                                    let trimmed = stderr.trim();
                                    let msg = if trimmed.is_empty() {
                                        "git: FAILED".to_string()
                                    } else {
                                        format!("git: FAILED\n{}", trimmed)
                                    };
                                    engine.update_toast_by_id(
                                        toast_id,
                                        ToastUpdate::new()
                                            .toast_type(ToastType::Error)
                                            .message(msg)
                                            .keep_on(true)
                                            .show_progress_bar(false),
                                    );
                                }
                            }
                        }
                        GitToastEventKind::TimedOut { timeout_secs } => {
                            if let Some(toast_id) = toast_ids.remove(&event.command_id) {
                                engine.update_toast_by_id(
                                    toast_id,
                                    ToastUpdate::new()
                                        .toast_type(ToastType::Error)
                                        .message(format!("git: TIMED OUT ({}s)", timeout_secs))
                                        .keep_on(true)
                                        .show_progress_bar(false),
                                );
                            }
                        }
                        GitToastEventKind::Cancelled => {
                            if let Some(toast_id) = toast_ids.remove(&event.command_id) {
                                engine.update_toast_by_id(
                                    toast_id,
                                    ToastUpdate::new()
                                        .toast_type(ToastType::Warning)
                                        .message("git: CANCELLED")
                                        .duration(Some(Duration::from_secs(2)))
                                        .show_progress_bar(false),
                                );
                            }
                        }
                    }
                }

                // Tick the toast engine (expire timed toasts).
                engine.tick();

                // Handle input events (non-blocking).
                while event::poll(Duration::from_millis(0)).unwrap_or(false) {
                    if let Ok(Event::Key(key)) = event::read()
                        && key.kind == KeyEventKind::Press
                    {
                        match key.code {
                            KeyCode::Enter
                            | KeyCode::Esc
                            | KeyCode::Char('q')
                            | KeyCode::Char('Q')
                                if engine.has_toast() =>
                            {
                                engine.handle_shortcut(ToastShortcut::Dismiss);
                            }
                            _ => {}
                        }
                    }
                    if let Ok(Event::Mouse(mouse)) = event::read() {
                        match mouse.kind {
                            MouseEventKind::Down(MouseButton::Left) => {
                                engine.handle_click(
                                    mouse.column,
                                    mouse.row,
                                    ToastMouseButton::Left,
                                );
                            }
                            MouseEventKind::Down(MouseButton::Right) => {
                                engine.handle_click(
                                    mouse.column,
                                    mouse.row,
                                    ToastMouseButton::Right,
                                );
                            }
                            _ => {}
                        }
                    }
                }

                // Render.
                terminal.draw(|frame| {
                    let area = frame.area();
                    engine.set_area(area);
                    engine.render_ref(area, frame.buffer_mut());
                })?;

                // Check exit conditions.
                if let Some(completed_at) = action_completed_at {
                    let elapsed = completed_at.elapsed();
                    if !engine.has_toast() || elapsed >= LINGER_DURATION {
                        break;
                    }
                }

                if done && !engine.has_toast() {
                    break;
                }

                thread::sleep(TICK_INTERVAL);
            }

            Ok::<(), anyhow::Error>(())
        })()
    });

    // Restore terminal.
    disable_raw_mode().context("failed to disable raw mode")?;
    let mut stdout_restore = io::stdout();
    execute!(stdout_restore, LeaveAlternateScreen).context("failed to leave alternate screen")?;
    stdout_restore.flush().ok();

    terminal_result?;

    // Get the action result from the shared slot.
    result_slot
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
        .unwrap_or_else(|| Err(anyhow::anyhow!("action thread did not produce a result")))
}
