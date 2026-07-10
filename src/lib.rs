// Copyright © 2026 ComfyHome™
// All rights reserved.
//
// Licensed under the ComfyGit SA-PS License
//
// For details, see the LICENSE file in the repository root.

mod app;
mod changelog;
mod cli;
mod config;
mod debug;
mod forge;
mod ghub;
mod git;
mod glab;
mod snif;
mod tui;
mod workflow;

pub fn run_entrypoint() -> anyhow::Result<()> {
    debug::init();
    if debug::any_debug_enabled() {
        debug::log("init", &format!(
            "debug enabled: GIT_DEBUG={} TUI_DEBUG={}",
            debug::git_debug_enabled(),
            debug::tui_debug_enabled(),
        ));
    }
    match cli::dispatch()? {
        cli::StartupMode::Handled => Ok(()),
        cli::StartupMode::LaunchTui => app::run(),
    }
}
