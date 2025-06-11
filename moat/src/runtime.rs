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

// Copyright 2025 foyer Project Authors
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

use std::{
    fmt::Debug,
    mem::ManuallyDrop,
    ops::{Deref, DerefMut},
    sync::Arc,
};

use tokio::runtime::{Handle as TokioHandle, Runtime as TokioRuntime};

/// A wrapper around [`tokio::runtime::Runtime`] that shuts down the runtime in the background when dropped.
///
/// This is necessary because directly dropping a nested runtime is not allowed in a parent runtime.s
#[derive(Clone)]
pub struct Runtime(Arc<RuntimeInner>);

impl Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Runtime").finish()
    }
}

impl Deref for Runtime {
    type Target = TokioRuntime;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<TokioRuntime> for Runtime {
    fn from(runtime: TokioRuntime) -> Self {
        Self::new(runtime)
    }
}

impl Runtime {
    pub fn new(inner: TokioRuntime) -> Self {
        Self(Arc::new(RuntimeInner(ManuallyDrop::new(inner))))
    }

    #[expect(unused)]
    pub fn handle(&self) -> Handle {
        Handle {
            inner: self.0.handle().clone(),
            _runtime: self.clone(),
        }
    }
}

struct RuntimeInner(ManuallyDrop<TokioRuntime>);

impl Drop for RuntimeInner {
    fn drop(&mut self) {
        // Safety: The runtime is only dropped once here.
        let runtime = unsafe { ManuallyDrop::take(&mut self.0) };
        runtime.shutdown_background();
    }
}

impl Deref for RuntimeInner {
    type Target = TokioRuntime;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for RuntimeInner {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

pub struct Handle {
    inner: TokioHandle,
    _runtime: Runtime,
}

impl Deref for Handle {
    type Target = TokioHandle;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for Handle {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}
