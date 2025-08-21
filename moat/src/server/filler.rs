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

use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use bytes::Bytes;
use foyer::HybridCache;
use futures::{Stream, stream::FuturesUnordered};
use governor::DefaultDirectRateLimiter;
use opendal::Operator;
use pin_project::pin_project;
use tokio::sync::mpsc;

use crate::error::Result;

pub struct RefillTask {
    pub path: String,
}

#[derive(Debug)]
pub struct Filler {
    tx: mpsc::Sender<RefillTask>,
}

impl Filler {
    pub fn submit(&self, task: RefillTask) -> Result<()> {
        todo!()
    }
}

type Task = Pin<Box<dyn Future<Output = Result<()>> + Send>>;

#[pin_project]
struct Runner {
    #[pin]
    inflight: FuturesUnordered<Task>,

    rx: mpsc::Receiver<RefillTask>,
    operator: Operator,
    cache: HybridCache<Arc<String>, Bytes>,

    throttler: DefaultDirectRateLimiter,
    concurrency: usize,
    retry: usize,
    timeout: Duration,
}

impl Stream for Runner {
    type Item = Result<(String, Bytes)>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.as_mut().project();
        let mut wait = false;

        loop {
            if this.inflight.len() < *this.concurrency {
                match this.rx.poll_recv(cx) {
                    Poll::Ready(Some(task)) => {
                        let operator = this.operator.clone();
                        let cache = this.cache.clone();
                        let fut = async move {
                            cache
                                .fetch(Arc::new(task.path.clone()), || async move {
                                    let buffer = operator.read(&task.path).await.map_err(anyhow::Error::from)?;
                                    let bytes = buffer.to_bytes();
                                    Ok(bytes)
                                })
                                .await?;
                            Ok(())
                        };
                        let fut = Box::pin(fut);
                        this.inflight.as_mut().push(fut);
                    }
                    Poll::Ready(None) if wait => return Poll::Ready(None),
                    Poll::Ready(None) => {}
                    Poll::Pending if wait => return Poll::Pending,
                    Poll::Pending => {}
                }
            }

            match this.inflight.as_mut().poll_next(cx) {
                Poll::Ready(Some(res)) => {
                    if let Err(e) = res {
                        tracing::error!("Failed to refill cache: {}", e);
                        continue;
                    }
                }
                Poll::Ready(None) => {
                    wait = true;
                    continue;
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}
