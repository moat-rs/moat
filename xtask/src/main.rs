// Copyright 2025 Moat Project Authors
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

use clap::{Parser, Subcommand};
use colored::Colorize;
use std::{
    ffi::OsStr,
    io::{Write, stdin, stdout},
    process::{Command as StdCommand, Stdio, exit},
};

use crate::dev::Dev;

fn check_and_install(name: &str, check: &str, install: &str, yes: bool) {
    if StdCommand::new("sh")
        .arg("-c")
        .arg(check)
        .stdout(Stdio::null())
        .status()
        .unwrap()
        .success()
    {
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
        let mut input = String::new();
        loop {
            stdin().read_line(&mut input).unwrap();
            match input.trim().to_lowercase().as_str() {
                "y" | "yes" | "" => {
                    break;
                }
                "n" | "no" => {
                    println!("Exit because user canceled the installation.");
                    exit(1);
                }
                _ => continue,
            }
        }
    }
    println!("Installing tool {name}...", name = name.magenta());
    if StdCommand::new("sh").arg("-c").arg(install).status().unwrap().success() {
        println!("Tool {name} installed successfully!", name = name.magenta());
    } else {
        println!(
            "Failed to install tool {name}. Please install it manually.",
            name = name.magenta()
        );
        exit(1);
    }
}

fn run(script: &str) {
    run_with_env::<Vec<(String, String)>, _, _>(script, vec![])
}

fn run_with_env<I, K, V>(script: &str, vars: I)
where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    if !StdCommand::new("sh")
        .arg("-c")
        .arg(script)
        .envs(std::env::vars())
        .envs(vars)
        .current_dir(std::env::current_dir().unwrap())
        .status()
        .unwrap()
        .success()
    {
        println!("Script `{script}` failed.", script = script.red());
        exit(1);
    }
}

fn tools(yes: bool) {
    check_and_install("typos", "which typos", "cargo install typos-cli", yes);
    check_and_install("taplo", "which taplo", "cargo install taplo-cli --locked", yes);
    check_and_install("cargo-sort", "which cargo-sort", "cargo install cargo-sort", yes);
    check_and_install(
        "cargo-machete",
        "which cargo-machete",
        "cargo install cargo-machete",
        yes,
    );
    check_and_install(
        "cargo-nextest",
        "which cargo-nextest",
        "cargo install cargo-nextest --locked",
        yes,
    );
    check_and_install(
        "license-eye",
        "which license-eye",
        &format!(
            r#"
            wget -P /tmp https://github.com/apache/skywalking-eyes/releases/download/v0.7.0/skywalking-license-eye-0.7.0-bin.tgz && \
            tar -xzf /tmp/skywalking-license-eye-0.7.0-bin.tgz -C /tmp && \
            cp /tmp/skywalking-license-eye-0.7.0-bin/bin/{}/license-eye ~/.cargo/bin/license-eye
            "#,
            if cfg!(target_os = "linux") {
                "linux"
            } else if cfg!(target_os = "macos") {
                "darwin"
            } else if cfg!(target_os = "windows") {
                "windows"
            } else {
                println!(
                    "Unsupported OS for {name} installation.",
                    name = "license-eye".magenta()
                );
                exit(1);
            },
        ),
        yes,
    );
}

fn check() {
    run("typos");
    run("cargo sort -w");
    run("taplo fmt");
    run("cargo fmt --all");
    run("cargo +nightly fmt --all");
    run("cargo clippy --all-targets");
}

fn all(yes: bool) {
    tools(yes);
    check();
    test();
    udeps();
    license();
}

fn test() {
    run("cargo nextest run");
}

fn udeps() {
    run("cargo-machete");
}

fn license() {
    run("license-eye header check");
}

mod dev;

#[derive(Debug, Parser)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Automatically answer yes to prompts.
    #[clap(short, long, default_value_t = false)]
    yes: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the default task suit.
    All,
    /// Install necessary tools for development and testing.
    Tools,
    /// Static code analysis and checks.
    Check,
    /// Run unit tests.
    Test,
    /// Find unused dependeicies.
    Udeps,
    /// Check licenses headers.
    License,
    /// Tasks for the development environment.
    ///
    /// The docker environment is needed.
    #[command(subcommand)]
    Dev(Dev),
}

fn main() {
    let cli = Cli::parse();

    let command = cli.command.unwrap_or(Command::All);

    match command {
        Command::All => all(cli.yes),
        Command::Tools => tools(cli.yes),
        Command::Check => check(),
        Command::Test => test(),
        Command::Udeps => udeps(),
        Command::License => license(),
        Command::Dev(dev) => match dev {
            Dev::Up(args) => dev::up(args),
            Dev::Down(args) => dev::down(args),
            Dev::Clean => dev::clean(),
        },
    }

    println!("{}", "Done!".green());
}
