// Copyright © 2026 ComfyHome™
// All rights reserved.
//
// Licensed under the ComfyGit SA-PS License
//
// For details, see the LICENSE file in the repository root.

//! Minimal TUI overlay that shows toast notifications for git commands
//! executed from the CLI. The command runs in a scoped background thread
//! while a lightweight ratatui render loop on the main thread displays
//! real-time toast updates. The action's stdout/stderr are captured via
//! fd redirection and replayed to the real terminal after the overlay exits.

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

#[cfg(unix)]
unsafe extern "C" {
    fn c_stdout() -> *mut libc::FILE;
    fn c_stderr() -> *mut libc::FILE;
}

/// Captures stdout/stderr via fd redirection so the action's `println!`
/// output is preserved while the alternate screen overlay is active.
/// After the overlay exits, the captured output is replayed to the real
/// terminal.
#[cfg(unix)]
struct StdoutCapture {
    saved_stdout: i32,
    saved_stderr: i32,
    read_fd: i32,
}

#[cfg(unix)]
impl StdoutCapture {
    fn new() -> Result<Self> {
        unsafe {
            let saved_stdout = libc::dup(libc::STDOUT_FILENO);
            let saved_stderr = libc::dup(libc::STDERR_FILENO);
            if saved_stdout < 0 || saved_stderr < 0 {
                return Err(io::Error::last_os_error().into());
            }

            let mut pipe_fds = [0i32; 2];
            if libc::pipe(pipe_fds.as_mut_ptr()) < 0 {
                libc::close(saved_stdout);
                libc::close(saved_stderr);
                return Err(io::Error::last_os_error().into());
            }
            let read_fd = pipe_fds[0];
            let write_fd = pipe_fds[1];

            // Redirect stdout and stderr to the pipe write end.
            if libc::dup2(write_fd, libc::STDOUT_FILENO) < 0 {
                libc::close(saved_stdout);
                libc::close(saved_stderr);
                libc::close(read_fd);
                libc::close(write_fd);
                return Err(io::Error::last_os_error().into());
            }
            if libc::dup2(write_fd, libc::STDERR_FILENO) < 0 {
                let _ = libc::dup2(saved_stdout, libc::STDOUT_FILENO);
                libc::close(saved_stdout);
                libc::close(saved_stderr);
                libc::close(read_fd);
                libc::close(write_fd);
                return Err(io::Error::last_os_error().into());
            }
            libc::close(write_fd);

            // Make the read end non-blocking so we can drain it in the render loop.
            let flags = libc::fcntl(read_fd, libc::F_GETFL);
            if flags >= 0 {
                libc::fcntl(read_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }

            // Prevent the saved fds from being inherited.
            for fd in [saved_stdout, saved_stderr] {
                let f = libc::fcntl(fd, libc::F_GETFD);
                if f >= 0 {
                    libc::fcntl(fd, libc::F_SETFD, f | libc::FD_CLOEXEC);
                }
            }

            Ok(Self {
                saved_stdout,
                saved_stderr,
                read_fd,
            })
        }
    }

    /// Drain available data from the pipe into `buf`.
    fn drain(&self, buf: &mut Vec<u8>) {
        let mut chunk = [0u8; 4096];
        loop {
            let n = unsafe { libc::read(self.read_fd, chunk.as_mut_ptr() as *mut _, chunk.len()) };
            if n <= 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n as usize]);
        }
    }

    /// Restore the original stdout/stderr and read any remaining pipe data.
    fn finish_and_replay(self) -> Result<()> {
        unsafe {
            // Flush libc buffers before restoring.
            libc::fflush(c_stdout());
            libc::fflush(c_stderr());

            // Restore original fds.
            libc::dup2(self.saved_stdout, libc::STDOUT_FILENO);
            libc::dup2(self.saved_stderr, libc::STDERR_FILENO);
            libc::close(self.saved_stdout);
            libc::close(self.saved_stderr);

            // Make read blocking for final drain.
            let flags = libc::fcntl(self.read_fd, libc::F_GETFL);
            if flags >= 0 {
                libc::fcntl(self.read_fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
            }
        }

        let mut output = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = unsafe { libc::read(self.read_fd, chunk.as_mut_ptr() as *mut _, chunk.len()) };
            if n <= 0 {
                break;
            }
            output.extend_from_slice(&chunk[..n as usize]);
        }
        unsafe { libc::close(self.read_fd) };

        if !output.is_empty() {
            let mut real_stdout = io::stdout();
            real_stdout.write_all(&output)?;
            real_stdout.flush()?;
        }
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for StdoutCapture {
    fn drop(&mut self) {
        unsafe {
            libc::dup2(self.saved_stdout, libc::STDOUT_FILENO);
            libc::dup2(self.saved_stderr, libc::STDERR_FILENO);
            libc::close(self.saved_stdout);
            libc::close(self.saved_stderr);
            libc::close(self.read_fd);
        }
    }
}

/// Runs `action` in a scoped background thread while displaying a minimal
/// toast overlay on the current thread. Returns the action's result.
///
/// Uses `thread::scope` so the action closure can borrow from the calling
/// scope without needing `'static`. The overlay enters the alternate screen,
/// drains git toast events, and renders them in real-time. After the action
/// completes, the overlay lingers briefly so the user can read the final
/// toast, then restores the terminal and replays the action's stdout/stderr.
pub(crate) fn run_with_toast_overlay<T: Send>(
    mut rx: UnboundedReceiver<GitToastEvent>,
    action: impl FnOnce() -> Result<T> + Send,
) -> Result<T> {
    let result_slot = Arc::new(Mutex::new(None::<Result<T>>));
    let result_slot_clone = result_slot.clone();
    let action_done = Arc::new(Mutex::new(false));
    let action_done_clone = action_done.clone();

    // Capture stdout/stderr before entering alternate screen.
    #[cfg(unix)]
    let capture = StdoutCapture::new().context("failed to capture stdout")?;

    // Set up the terminal for the overlay on the main thread.
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout_handle = stdout();
    execute!(stdout_handle, EnterAlternateScreen).context("failed to enter alternate screen")?;

    // Buffer for captured output drained during the render loop.
    #[cfg(unix)]
    let captured_output = Arc::new(Mutex::new(Vec::<u8>::new()));
    #[cfg(unix)]
    let captured_output_clone = captured_output.clone();

    let terminal_result = thread::scope(|scope| {
        // Spawn the action in a scoped thread (can borrow from caller).
        scope.spawn(move || {
            let result = action();
            // Flush libc buffers so all output is in the pipe.
            #[cfg(unix)]
            unsafe {
                libc::fflush(c_stdout());
                libc::fflush(c_stderr());
            }
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

                // Drain captured stdout/stderr from the action thread.
                #[cfg(unix)]
                {
                    let mut buf = captured_output_clone.lock().unwrap();
                    capture.drain(&mut buf);
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

    // Restore stdout/stderr and replay captured output.
    #[cfg(unix)]
    {
        // Final drain to capture any remaining output.
        {
            let mut buf = captured_output.lock().unwrap();
            capture.drain(&mut buf);
        }
        capture.finish_and_replay()?;
    }

    terminal_result?;

    // Get the action result from the shared slot.
    result_slot
        .lock()
        .ok()
        .and_then(|mut slot| slot.take())
        .unwrap_or_else(|| Err(anyhow::anyhow!("action thread did not produce a result")))
}
