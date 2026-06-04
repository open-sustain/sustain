// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Thin binary entry point. All logic lives in the `xtask` library so the
//! `tests/` suite can drive the commands against fixture workspaces.

fn main() -> std::process::ExitCode {
    xtask::run()
}
