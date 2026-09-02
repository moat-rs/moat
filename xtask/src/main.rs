// Copyright 2026- Moat Project Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Development tasks for the moat workspace.
//!
//! Invoked through the `cargo x` alias defined in `.cargo/config.toml`, following
//! the [cargo-xtask](https://github.com/matklad/cargo-xtask) convention. Every
//! task is a thin wrapper over shell commands so that the exact commands CI runs
//! are visible and reproducible locally.

use std::{
    io::{Write, stdin, stdout},
    process::{Command as StdCommand, Stdio, exit},
};

use clap::{Parser, Subcommand};
use colored::Colorize;

#[derive(Debug, Parser)]
#[command(about = "Development tasks for the moat workspace.")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Automatically answer yes to prompts.
    #[arg(short, long, default_value_t = false)]
    yes: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the default task suite: tools, check, test, udeps, license, doc.
    All,
    /// Install the tools the other tasks depend on.
    Tools,
    /// Formatting, spelling and static analysis (fixes formatting in place).
    Check,
    /// Run all tests.
    Test,
    /// Find unused dependencies.
    Udeps,
    /// Check license headers.
    License,
    /// Build the documentation with warnings denied.
    Doc,
}

fn main() {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::All) {
        Command::All => {
            tools(cli.yes);
            check();
            test();
            udeps();
            license();
            doc();
        }
        Command::Tools => tools(cli.yes),
        Command::Check => check(),
        Command::Test => test(),
        Command::Udeps => udeps(),
        Command::License => license(),
        Command::Doc => doc(),
    }
    println!("{}", "Done!".green());
}

fn tools(yes: bool) {
    check_and_install("typos", "typos --version", "cargo install typos-cli --locked", yes);
    check_and_install("taplo", "taplo --version", "cargo install taplo-cli --locked", yes);
    check_and_install(
        "cargo-sort",
        "cargo sort --version",
        "cargo install cargo-sort --locked",
        yes,
    );
    // Invoked as a plain binary: when run underneath `cargo run`, cargo-machete
    // does not strip the `machete` subcommand argument cargo passes to it and
    // treats it as a path.
    check_and_install(
        "cargo-machete",
        "cargo-machete --version",
        "cargo install cargo-machete --locked",
        yes,
    );
    check_and_install(
        "cargo-nextest",
        "cargo nextest --version",
        "cargo install cargo-nextest --locked",
        yes,
    );
    check_and_install(
        "license-eye",
        "license-eye --version",
        &format!(
            "tmp_dir=$(mktemp -d) \
             && trap 'rm -rf \"$tmp_dir\"' 0 \
             && cargo_home=\"${{CARGO_HOME:-$HOME/.cargo}}\" \
             && mkdir -p \"$cargo_home/bin\" \
             && wget -O \"$tmp_dir/license-eye.tgz\" https://github.com/apache/skywalking-eyes/releases/download/v0.7.0/skywalking-license-eye-0.7.0-bin.tgz \
             && tar -xzf \"$tmp_dir/license-eye.tgz\" -C \"$tmp_dir\" \
             && cp \"$tmp_dir/skywalking-license-eye-0.7.0-bin/bin/{os}/license-eye\" \"$cargo_home/bin/license-eye\"",
            os = license_eye_os(),
        ),
        yes,
    );
}

fn license_eye_os() -> &'static str {
    if cfg!(target_os = "linux") {
        "linux"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        println!("{}", "Unsupported OS for license-eye installation.".red());
        exit(1);
    }
}

fn check() {
    run("typos");
    run("cargo sort --workspace");
    run("taplo fmt");
    run("cargo fmt --all");
    if has_nightly() {
        // Unstable rustfmt options (import grouping, comment wrapping) live in a
        // separate config so stable rustfmt keeps working without them.
        run("cargo +nightly fmt --all -- --config-path rustfmt.nightly.toml");
    } else {
        println!(
            "{}",
            "Skipping nightly rustfmt (no nightly toolchain installed).".yellow()
        );
    }
    run("cargo clippy --workspace --all-targets -- -D warnings");
}

fn test() {
    run("cargo nextest run --workspace");
    // nextest does not run doctests.
    run("cargo test --workspace --doc");
}

fn udeps() {
    run("cargo-machete");
}

fn license() {
    run("license-eye header check");
}

fn doc() {
    run_with_env("cargo doc --workspace --no-deps", [("RUSTDOCFLAGS", "-D warnings")]);
}

fn has_nightly() -> bool {
    StdCommand::new("cargo")
        .args(["+nightly", "--version"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn check_and_install(name: &str, check: &str, install: &str, yes: bool) {
    let installed = StdCommand::new("sh")
        .arg("-c")
        .arg(check)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if installed {
        return;
    }
    if !yes {
        print!(
            "Tool {name} is not installed, install it? [{y}/{n}]: ",
            name = name.magenta(),
            y = "Y".green(),
            n = "n".red()
        );
        stdout().flush().unwrap();
        loop {
            let mut input = String::new();
            stdin().read_line(&mut input).unwrap();
            match input.trim().to_lowercase().as_str() {
                "y" | "yes" | "" => break,
                "n" | "no" => {
                    println!("Exit because the installation was declined.");
                    exit(1);
                }
                _ => continue,
            }
        }
    }
    println!("Installing tool {name}...", name = name.magenta());
    run(install);
    println!("Tool {name} installed.", name = name.magenta());
}

fn run(script: &str) {
    run_with_env::<[(&str, &str); 0]>(script, []);
}

fn run_with_env<I>(script: &str, vars: I)
where
    I: IntoIterator<Item = (&'static str, &'static str)>,
{
    println!("{} {script}", "$".dimmed());
    let ok = StdCommand::new("sh")
        .arg("-c")
        .arg(script)
        .envs(vars)
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        println!("Script `{script}` failed.", script = script.red());
        exit(1);
    }
}
