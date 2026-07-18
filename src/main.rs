// Copyright © 2026 ComfyHome™
// All rights reserved.
//
// Licensed under the ComfyGit License v1.2
//
// For details, see the LICENSE file in the repository root.

fn main() {
    if let Err(error) = comfygit::run_entrypoint() {
        eprintln!("\x1b[1;31mError: {error:#}\x1b[0m");
        std::process::exit(1);
    }
}
