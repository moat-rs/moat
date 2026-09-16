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

//! Deliver a poll's completions on application workers, amortizing remote wakes.

use super::{Data, Reply};
use anyhow::Result;
use tokio::sync::mpsc;

pub(super) enum Ready {
    Write(Vec<Reply<()>>),
    Read(Reply<Data>, Result<Data>),
    Close(Reply<()>, Result<()>),
}
impl Ready {
    fn finish(self) {
        match self {
            Self::Write(replies) => {
                for reply in replies {
                    let _ = reply.send(Ok(()));
                }
            }
            Self::Read(reply, result) => {
                let _ = reply.send(result);
            }
            Self::Close(reply, result) => {
                let _ = reply.send(result);
            }
        }
    }
}

pub(super) struct Delivery {
    sender: Option<mpsc::UnboundedSender<Vec<Ready>>>,
    pending: Vec<Ready>,
}
impl Delivery {
    pub fn new(batched: bool) -> Self {
        let sender = batched.then(|| {
            let (sender, mut receiver) = mpsc::unbounded_channel::<Vec<Ready>>();
            // One lightweight driver per disk on the existing runtime. Waking
            // request tasks here uses Tokio's local scheduling path instead of
            // having every I/O completion contend on the shared injection queue.
            tokio::spawn(async move {
                while let Some(batch) = receiver.recv().await {
                    for ready in batch {
                        ready.finish();
                    }
                }
            });
            sender
        });
        Self {
            sender,
            pending: Vec::new(),
        }
    }
    pub fn push(&mut self, ready: Ready) {
        if self.sender.is_some() {
            self.pending.push(ready);
        } else {
            ready.finish();
        }
    }
    pub fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        if let Err(rejected) = self
            .sender
            .as_ref()
            .expect("batched delivery")
            .send(std::mem::take(&mut self.pending))
        {
            // Runtime shutdown must not strand this batch's reply channels.
            for ready in rejected.0 {
                ready.finish();
            }
        }
    }
}
impl Drop for Delivery {
    fn drop(&mut self) {
        self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn close_follows_all_accepted_completions_and_drop_flushes() {
        let mut delivery = Delivery::new(true);
        let mut waits = Vec::new();
        for _ in 0..300 {
            let (reply, wait) = oneshot::channel();
            waits.push(wait);
            delivery.push(Ready::Write(vec![reply]));
        }
        delivery.flush();
        let (reply, close) = oneshot::channel();
        delivery.push(Ready::Close(reply, Ok(())));
        drop(delivery);
        close.await.unwrap().unwrap();
        for mut wait in waits {
            wait.try_recv().unwrap().unwrap();
        }
    }

    #[test]
    fn closed_driver_falls_back_without_losing_replies() {
        let (sender, receiver) = mpsc::unbounded_channel();
        drop(receiver);
        let mut delivery = Delivery {
            sender: Some(sender),
            pending: Vec::new(),
        };
        let (reply, mut wait) = oneshot::channel();
        delivery.push(Ready::Close(reply, Ok(())));
        drop(delivery);
        wait.try_recv().unwrap().unwrap();
    }
}
