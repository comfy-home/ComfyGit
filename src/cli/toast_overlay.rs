// Copyright © 2026 ComfyHome™
// All rights reserved.
//
// Licensed under the ComfyGit SA-PS License
//
// For details, see the LICENSE file in the repository root.

//! Minimal overlay that shows toast notifications for git commands
//! executed from the CLI. The command runs in a scoped background thread
//! while a lightweight render loop on the main thread displays real-time
//! toast updates. No alternate screen or raw mode is used — the action's
//! stdout/stderr flow normally to the terminal, and toasts are rendered on
//! top via cursor save/restore + direct buffer writes.

use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::{
    cursor::{MoveTo, RestorePosition, SavePosition},
    execute,
    style::{ResetColor, SetBackgroundColor, SetForegroundColor},
};
use ratatui::{buffer::Buffer, layout::Rect, widgets::WidgetRef};
use ratatui_comfy_toaster::{
    ToastBuilder, ToastEngine, ToastEngineBuilder, ToastType, ToastUpdate,
};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::git::{GitToastEvent, GitToastEventKind};

/// How often the overlay loop ticks (drains events + renders).
const TICK_INTERVAL: Duration = Duration::from_millis(50);

/// Maximum time to wait after the action completes before force-closing
/// the overlay (gives user time to read success toasts).
const LINGER_DURATION: Duration = Duration::from_secs(3);

/// Write toast cells from `next` to stdout, but only within `region`.
/// Also clears cells that were non-empty in `prev` within `region` but are
/// now empty. Always re-paints all non-empty cells every tick (not just
/// diffs) because the action thread's output scrolls the terminal,
/// invalidating previously written cells at fixed positions.
fn write_buffer_region(prev: &Buffer, next: &Buffer, region: Rect) -> io::Result<()> {
    let mut stdout = io::stdout();
    execute!(stdout, SavePosition)?;
    for y in region.y..region.y + region.height {
        for x in region.x..region.x + region.width {
            let next_cell = &next[(x, y)];
            let prev_cell = &prev[(x, y)];
            // A cell is only truly empty if it has a space symbol AND no
            // background color — toast backgrounds use space+bg cells.
            let next_empty = next_cell.symbol() == " " && next_cell.style().bg.is_none();
            let prev_empty = prev_cell.symbol() == " " && prev_cell.style().bg.is_none();
            // Skip cells that are empty in both buffers.
            if next_empty && prev_empty {
                continue;
            }
            // Always write the cell (even if unchanged) because the
            // terminal may have been scrolled by the action thread.
            execute!(stdout, MoveTo(x, y))?;
            if next_empty {
                write!(stdout, " ")?;
            } else {
                let style = next_cell.style();
                if let Some(fg) = style.fg {
                    execute!(
                        stdout,
                        SetForegroundColor(ratatui_crossterm_color_to_crossterm(fg))
                    )?;
                }
                if let Some(bg) = style.bg {
                    execute!(
                        stdout,
                        SetBackgroundColor(ratatui_crossterm_color_to_crossterm(bg))
                    )?;
                }
                write!(stdout, "{}", next_cell.symbol())?;
                execute!(stdout, ResetColor)?;
            }
        }
    }
    execute!(stdout, RestorePosition)?;
    stdout.flush()
}

/// Convert a ratatui `Color` to a crossterm `Color`.
fn ratatui_crossterm_color_to_crossterm(c: ratatui::style::Color) -> crossterm::style::Color {
    use ratatui::style::Color;
    match c {
        Color::Reset => crossterm::style::Color::Reset,
        Color::Black => crossterm::style::Color::Black,
        Color::Red => crossterm::style::Color::DarkRed,
        Color::Green => crossterm::style::Color::DarkGreen,
        Color::Yellow => crossterm::style::Color::DarkYellow,
        Color::Blue => crossterm::style::Color::DarkBlue,
        Color::Magenta => crossterm::style::Color::DarkMagenta,
        Color::Cyan => crossterm::style::Color::DarkCyan,
        Color::Gray => crossterm::style::Color::Grey,
        Color::DarkGray => crossterm::style::Color::DarkGrey,
        Color::LightRed => crossterm::style::Color::Red,
        Color::LightGreen => crossterm::style::Color::Green,
        Color::LightYellow => crossterm::style::Color::Yellow,
        Color::LightBlue => crossterm::style::Color::Blue,
        Color::LightMagenta => crossterm::style::Color::Magenta,
        Color::LightCyan => crossterm::style::Color::Cyan,
        Color::White => crossterm::style::Color::White,
        Color::Indexed(i) => crossterm::style::Color::AnsiValue(i),
        Color::Rgb(r, g, b) => crossterm::style::Color::Rgb { r, g, b },
    }
}

/// Clear a rectangular region of the terminal by overwriting with spaces.
fn clear_region(area: Rect) -> io::Result<()> {
    let mut stdout = io::stdout();
    execute!(stdout, SavePosition)?;
    for y in area.y..area.y + area.height {
        execute!(stdout, MoveTo(area.x, y))?;
        for _ in 0..area.width {
            write!(stdout, " ")?;
        }
    }
    execute!(stdout, RestorePosition)?;
    stdout.flush()
}

/// Runs `action` in a scoped background thread while displaying toast
/// notifications as a true overlay on the current terminal. Returns the
/// action's result.
///
/// Unlike a full-screen TUI, this does NOT use the alternate screen. The
/// action's stdout/stderr flow normally to the terminal, and toasts are
/// rendered on top using cursor save/restore + direct buffer writes.
/// This means interactive prompts and command output remain visible.
pub(crate) fn run_with_toast_overlay<T: Send>(
    mut rx: UnboundedReceiver<GitToastEvent>,
    action: impl FnOnce() -> Result<T> + Send,
) -> Result<T> {
    let result_slot = Arc::new(Mutex::new(None::<Result<T>>));
    let result_slot_clone = result_slot.clone();
    let action_done = Arc::new(Mutex::new(false));
    let action_done_clone = action_done.clone();

    let overlay_result = thread::scope(|scope| {
        // Spawn the action in a scoped thread.
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
            let (w, h) = crossterm::terminal::size().unwrap_or((120, 40));
            let area = Rect::new(0, 0, w, h);

            let mut engine: ToastEngine<()> = ToastEngineBuilder::new(area)
                .default_duration(Duration::from_secs(3))
                .default_progress_bar(true)
                .build();

            let mut toast_ids: std::collections::HashMap<u64, u64> =
                std::collections::HashMap::new();
            let mut action_completed_at: Option<Instant> = None;
            let mut prev_buffer = Buffer::empty(area);
            let mut last_toast_areas: Option<Vec<Rect>> = None;

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

                // Render toasts to a buffer.
                let mut next_buffer = Buffer::empty(area);
                engine.set_area(area);
                engine.render_ref(area, &mut next_buffer);

                // Get all toast areas from the engine (each toast has its
                // own area, properly stacked by set_area_avoiding).
                let current_areas = engine.toast_areas();

                // Clear any old toast areas that are no longer present.
                if let Some(old_areas) = &last_toast_areas {
                    for old in old_areas {
                        if !current_areas.contains(old) {
                            clear_region(*old)?;
                        }
                    }
                }

                // Write each toast area individually — no giant bounding
                // box that would wipe out terminal content between toasts.
                for &toast_area in &current_areas {
                    write_buffer_region(&prev_buffer, &next_buffer, toast_area)?;
                }

                prev_buffer = next_buffer;
                last_toast_areas = Some(current_areas);

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

    overlay_result?;

    // Get the action result from the shared slot.
    result_slot
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
        .unwrap_or_else(|| Err(anyhow::anyhow!("action thread did not produce a result")))
}
