use std::io::Write;
use std::sync::OnceLock;
use std::time::Instant;

static GIT_DEBUG_ENABLED: OnceLock<bool> = OnceLock::new();
static TUI_DEBUG_ENABLED: OnceLock<bool> = OnceLock::new();
static DEEP_DEBUG_ENABLED: OnceLock<bool> = OnceLock::new();
static DEBUG_FILE: OnceLock<String> = OnceLock::new();

const DEFAULT_LOG_PATH: &str = "./comfygit-debug.log";

pub(crate) fn init() {
    let git_enabled = std::env::var("GIT_DEBUG")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let tui_enabled = std::env::var("TUI_DEBUG")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let deep_enabled = std::env::var("DEEP_DEBUG")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let _ = GIT_DEBUG_ENABLED.set(git_enabled);
    let _ = TUI_DEBUG_ENABLED.set(tui_enabled);
    let _ = DEEP_DEBUG_ENABLED.set(deep_enabled);

    let file_path = std::env::var("COMFYGIT_DEBUG_FILE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_LOG_PATH.to_string());
    let _ = DEBUG_FILE.set(file_path);
}

pub(crate) fn git_debug_enabled() -> bool {
    *GIT_DEBUG_ENABLED.get_or_init(|| false)
}

pub(crate) fn tui_debug_enabled() -> bool {
    *TUI_DEBUG_ENABLED.get_or_init(|| false)
}

pub(crate) fn deep_debug_enabled() -> bool {
    *DEEP_DEBUG_ENABLED.get_or_init(|| false)
}

pub(crate) fn any_debug_enabled() -> bool {
    git_debug_enabled() || tui_debug_enabled() || deep_debug_enabled()
}

pub(crate) fn log(category: &str, message: &str) {
    let timestamp = chrono::Local::now().format("%H:%M:%S%.3f");
    let line = format!("[{timestamp}] {category}: {message}\n");

    let path = DEBUG_FILE.get_or_init(|| DEFAULT_LOG_PATH.to_string());
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = file.write_all(line.as_bytes());
        return;
    }

    let _ = std::io::stderr().write_all(line.as_bytes());
}

pub(crate) fn log_git_start(repo_root: &str, args: &[&str]) -> Instant {
    let now = Instant::now();
    if git_debug_enabled() {
        log("git", &format!("start {} {:?}", repo_root, args));
    }
    now
}

pub(crate) fn log_git_end(repo_root: &str, args: &[&str], started: Instant, success: bool) {
    if git_debug_enabled() {
        let elapsed = started.elapsed();
        log(
            "git",
            &format!(
                "end   {} {:?} {}ms {}",
                repo_root,
                args,
                elapsed.as_millis(),
                if success { "ok" } else { "FAIL" }
            ),
        );
    }
}

pub(crate) fn log_git_timeout(repo_root: &str, args: &[&str], timeout_secs: u64) {
    if git_debug_enabled() {
        log(
            "git",
            &format!("TIMEOUT {} {:?} after {}s", repo_root, args, timeout_secs),
        );
    }
}

pub(crate) fn log_cmd_start(program: &str, repo_root: &str, args: &[String]) -> Instant {
    let now = Instant::now();
    if git_debug_enabled() {
        log("cmd", &format!("start {program} in {repo_root} {args:?}"));
    }
    now
}

pub(crate) fn log_cmd_end(
    program: &str,
    repo_root: &str,
    args: &[String],
    started: Instant,
    success: bool,
) {
    if git_debug_enabled() {
        let elapsed = started.elapsed();
        log(
            "cmd",
            &format!(
                "end   {program} in {repo_root} {args:?} {}ms {}",
                elapsed.as_millis(),
                if success { "ok" } else { "FAIL" }
            ),
        );
    }
}

pub(crate) fn log_cmd_timeout(program: &str, repo_root: &str, args: &[String], timeout_secs: u64) {
    if git_debug_enabled() {
        log(
            "cmd",
            &format!(
                "TIMEOUT {program} in {repo_root} {args:?} after {}s",
                timeout_secs
            ),
        );
    }
}

pub(crate) fn git_default_timeout() -> std::time::Duration {
    std::env::var("COMFYGIT_GIT_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(std::time::Duration::from_secs)
        .unwrap_or(std::time::Duration::from_secs(10))
}

pub(crate) fn log_tui(category: &str, message: &str) {
    if tui_debug_enabled() {
        log(category, message);
    }
}

pub(crate) fn log_tui_key(key_code: &str, modifiers: &str) {
    if tui_debug_enabled() {
        log("tui/key", &format!("{key_code} mods={modifiers}"));
    }
}

pub(crate) fn log_tui_mouse(kind: &str, column: u16, row: u16) {
    if tui_debug_enabled() {
        log("tui/mouse", &format!("{kind} at ({column}, {row})"));
    }
}

pub(crate) fn log_tui_mouse_deep(kind: &str, column: u16, row: u16) {
    if deep_debug_enabled() {
        log("tui/mouse", &format!("{kind} at ({column}, {row})"));
    }
}

pub(crate) fn log_tui_scope_select(scope_index: usize, project_name: &str) {
    if tui_debug_enabled() {
        log(
            "tui/scope",
            &format!("select scope={scope_index} project='{project_name}'"),
        );
    }
}

pub(crate) fn log_tui_draw_start() -> Instant {
    let now = Instant::now();
    if deep_debug_enabled() {
        log("tui/draw", "start");
    }
    now
}

pub(crate) fn log_tui_draw_end(started: Instant) {
    if deep_debug_enabled() {
        let elapsed = started.elapsed();
        log("tui/draw", &format!("end {}ms", elapsed.as_millis()));
    }
}

pub(crate) fn log_tui_loop(message: &str) {
    if tui_debug_enabled() {
        log("tui/loop", message);
    }
}
