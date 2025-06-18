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

mod logging;
mod meter;

use crate::{config::MoatConfig, error::Result};

pub fn init(config: &MoatConfig) -> Result<Box<dyn Send + Sync + 'static>> {
    let mut guards = vec![];

    let guard = logging::init(config)?;
    guards.push(guard);

    let guard = meter::init(&config.telemetry)?;
    guards.push(guard);

    Ok(Box::new(guards))
}
