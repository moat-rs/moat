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

use std::fs::create_dir_all;

use clap::{Args, Subcommand};

use crate::{run, run_with_env};

#[derive(Debug, Subcommand)]
pub enum Dev {
    /// Start the development environment.
    Up(Up),
    /// Stop the development environment.
    Down(Down),
    /// Clean the development environment.
    Clean,
}

fn env() -> Vec<(&'static str, String)> {
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    vec![("UID", uid.to_string()), ("GID", gid.to_string())]
}

#[derive(Debug, Args)]
pub struct Up {
    services: Vec<String>,
    #[arg(long, default_value_t = false)]
    release: bool,
}

pub fn up(args: Up) {
    create_dir_all(".moat").unwrap();
    create_dir_all(".moat/log").unwrap();
    create_dir_all(".moat/cache").unwrap();
    create_dir_all(".moat/minio").unwrap();
    create_dir_all(".moat/prometheus").unwrap();
    create_dir_all(".moat/loki").unwrap();

    let cmd = format!(
        r#"docker compose up --build -d {services}"#,
        services = args.services.join(" "),
    );
    let mut env = env();
    let mut build_args = String::new();
    if args.release {
        build_args.push_str("--release ");
    }
    env.push(("BUILD_ARGS", build_args));
    run_with_env(&cmd, env);
}

#[derive(Debug, Args)]
pub struct Down {
    services: Vec<String>,
}

pub fn down(args: Down) {
    let cmd = format!(
        r#"docker compose --profile "*" down {services}"#,
        services = args.services.join(" ")
    );
    let mut env = env();
    env.push(("BUILD_ARGS", String::new()));
    run_with_env(&cmd, env);
}

pub fn clean() {
    let mut env = env();
    env.push(("BUILD_ARGS", String::new()));
    run_with_env(r#"docker compose --profile "*" down --volumes --remove-orphans"#, env);
    run("rm -rf .moat");
}
