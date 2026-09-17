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

//! Threads driving exclusively owned sessions.

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

pub use crate::storage::QueueBackend;
use crate::storage::{Completion, Disk, QueueOptions, Session};

/// Index of a disk in a node's disk list.
pub type DiskId = usize;

/// How the worker behaves when it has nothing to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollMode {
    /// Spin: the lowest latency, one core per worker.
    Busy,
    /// Sleep after an idle handler iteration. For deployments that cannot
    /// dedicate cores; polling all disks first avoids waiting on a single disk.
    Adaptive {
        /// How long to sleep when neither I/O nor requests are pending.
        idle_sleep: Duration,
    },
}

/// Configuration of one worker.
#[derive(Debug, Clone)]
pub struct WorkerOptions {
    /// Core to pin the thread to; `None` leaves scheduling to the OS.
    pub core: Option<usize>,
    /// Queue depth and pool allocated separately for each owned disk.
    pub queue: QueueOptions,
    /// Queue implementation.
    pub backend: QueueBackend,
    /// Idle behaviour.
    pub poll_mode: PollMode,
}

impl Default for WorkerOptions {
    fn default() -> Self {
        Self {
            core: None,
            queue: QueueOptions::default(),
            backend: QueueBackend::Auto,
            poll_mode: if cfg!(target_os = "linux") {
                PollMode::Busy
            } else {
                PollMode::Adaptive {
                    idle_sleep: Duration::from_millis(1),
                }
            },
        }
    }
}

/// What a [`Handler::run`] call reports back to the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Work was done or is pending; loop again at once.
    Continue,
    /// Nothing to do until I/O completes or new requests arrive.
    Idle,
    /// Shut the worker down after this iteration.
    Stop,
}

/// One disk exclusively owned by this worker.
pub struct DiskSlot {
    /// Stable node-wide disk index.
    pub id: DiskId,
    /// The single-owner engine adapter.
    pub session: Session,
}
/// What a handler sees on each iteration. Only locally owned disks are exposed.
pub struct Context<'a> {
    /// Worker index in the node.
    pub worker: usize,
    /// Owned disks, each with its own engine and queue.
    pub disks: &'a mut [DiskSlot],
    /// Native completions tagged with node-wide disk IDs; drain on each call.
    pub completions: &'a mut Vec<(DiskId, Completion)>,
}
impl Context<'_> {
    /// Whether this worker owns a disk.
    pub fn owns(&self, disk: DiskId) -> bool {
        self.disks.iter().any(|slot| slot.id == disk)
    }
    /// Gets an owned session. Route remote-disk requests to their owner explicitly.
    pub fn disk(&mut self, disk: DiskId) -> Option<&mut Session> {
        self.disks
            .iter_mut()
            .find(|slot| slot.id == disk)
            .map(|slot| &mut slot.session)
    }
}

/// The request source of a worker. See the [module docs](self).
pub trait Handler: Send + 'static {
    /// Called once on the worker thread after its owned sessions recover.
    fn start(&mut self, _cx: &mut Context<'_>) {}

    /// Called once per loop iteration, after completions were collected.
    fn run(&mut self, cx: &mut Context<'_>) -> Step;

    /// Called once on the worker thread after the loop ends, before the
    /// sessions are drained, sealed and released.
    fn stop(&mut self, _cx: &mut Context<'_>) {}
}

/// Errors from a worker thread.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    /// The queue or a pipeline could not be set up, or the queue failed.
    #[error("worker {worker}: {source}")]
    Io {
        /// The worker index.
        worker: usize,
        /// The cause.
        #[source]
        source: io::Error,
    },
    /// An engine call failed during setup or shutdown.
    #[error("worker {worker}: {source}")]
    Engine {
        /// The worker index.
        worker: usize,
        /// The cause.
        #[source]
        source: crate::storage::Error,
    },
    /// The handler panicked; the worker thread is gone.
    #[error("worker {0} panicked")]
    Panicked(usize),
}

/// A running worker thread.
pub struct Worker<H: Handler> {
    index: usize,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<H, WorkerError>>>,
}

impl<H: Handler> Worker<H> {
    /// Starts worker `index`, recovers each assigned disk and runs `handler`.
    /// Each disk gets a separate queue and pool. Startup errors are returned
    /// here; runtime and shutdown failures are returned by `join`.
    pub fn spawn(
        index: usize,
        opts: WorkerOptions,
        disks: Vec<(DiskId, Disk)>,
        handler: H,
    ) -> Result<Self, WorkerError> {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), WorkerError>>();
        let stop_flag = stop.clone();
        let thread = thread::Builder::new()
            .name(format!("moat-worker-{index}"))
            .spawn(move || run_worker(index, opts, disks, handler, stop_flag, ready_tx))
            .map_err(|source| WorkerError::Io { worker: index, source })?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                index,
                stop,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(WorkerError::Panicked(index))
            }
        }
    }

    /// The worker's index.
    pub fn index(&self) -> usize {
        self.index
    }

    /// Asks the worker to stop after its current iteration.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    /// Waits for the worker to finish and returns its handler.
    pub fn join(mut self) -> Result<H, WorkerError> {
        let thread = self.thread.take().expect("joined once");
        thread.join().map_err(|_| WorkerError::Panicked(self.index))?
    }
}

impl<H: Handler> Drop for Worker<H> {
    fn drop(&mut self) {
        if let Some(t) = self.thread.take() {
            self.stop.store(true, Ordering::Release);
            let _ = t.join();
        }
    }
}

/// Pins the calling thread to `core`.
#[cfg(target_os = "linux")]
pub fn pin_to_core(core: usize) -> io::Result<()> {
    // SAFETY: a zeroed cpu_set_t is a valid empty set; CPU_SET writes within
    // its bounds for any core below CPU_SETSIZE, which we check.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if core >= libc::CPU_SETSIZE as usize {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "core out of range"));
        }
        libc::CPU_SET(core, &mut set);
        if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// CPU pinning is unsupported outside Linux; use `WorkerOptions::core = None`.
#[cfg(not(target_os = "linux"))]
pub fn pin_to_core(_core: usize) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "CPU pinning is only available on Linux",
    ))
}

fn run_worker<H: Handler>(
    index: usize,
    opts: WorkerOptions,
    disks: Vec<(DiskId, Disk)>,
    mut handler: H,
    stop: Arc<AtomicBool>,
    ready: mpsc::Sender<Result<(), WorkerError>>,
) -> Result<H, WorkerError> {
    let io_err = |source| WorkerError::Io { worker: index, source };
    let engine_err = |source| WorkerError::Engine { worker: index, source };

    let setup = || -> Result<Vec<DiskSlot>, WorkerError> {
        if let Some(core) = opts.core {
            pin_to_core(core).map_err(io_err)?;
        }
        disks
            .into_iter()
            .map(|(id, disk)| {
                let session = Session::open(disk, &opts.queue, opts.backend).map_err(engine_err)?;
                Ok(DiskSlot { id, session })
            })
            .collect()
    };
    let mut slots = match setup() {
        Ok(slots) => slots,
        Err(error) => {
            let _ = ready.send(Err(error));
            return Err(WorkerError::Panicked(index));
        }
    };
    let _ = ready.send(Ok(()));
    let mut completions = Vec::new();
    let mut scratch = Vec::new();
    {
        let mut cx = Context {
            worker: index,
            disks: &mut slots,
            completions: &mut completions,
        };
        handler.start(&mut cx);
        loop {
            for slot in cx.disks.iter_mut() {
                slot.session.poll(false, &mut scratch).map_err(engine_err)?;
                cx.completions
                    .extend(scratch.drain(..).map(|completion| (slot.id, completion)));
            }
            let step = handler.run(&mut cx);
            if step == Step::Stop || stop.load(Ordering::Acquire) {
                break;
            }
            if step == Step::Idle
                && let PollMode::Adaptive { idle_sleep } = opts.poll_mode
            {
                thread::sleep(idle_sleep);
            }
        }
        handler.stop(&mut cx);
    }
    // Drain and validate admitted I/O before sealing, including requests accepted by stop.
    for slot in &mut slots {
        while slot.session.in_flight() != 0 {
            slot.session.poll(true, &mut scratch).map_err(engine_err)?;
            for completion in scratch.drain(..) {
                match completion {
                    Completion::Write { result, .. } | Completion::Flush { result, .. } => {
                        result.map_err(crate::storage::Error::from).map_err(engine_err)?
                    }
                    Completion::Read { result, .. } => {
                        result.map_err(crate::storage::Error::from).map_err(engine_err)?;
                    }
                }
            }
        }
        slot.session.seal().map_err(engine_err)?;
    }
    Ok(handler)
}
