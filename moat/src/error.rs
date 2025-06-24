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

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("opendal error: {0}")]
    OpenDal(#[from] opendal::Error),
    #[error("pingora error: {0}")]
    Pingora(#[from] pingora::Error),
    #[error("poem error: {0}")]
    Poem(#[from] poem::Error),
    #[error("http header error: {0}")]
    HttpHeader(#[from] headers::Error),
    #[error("other error: {0}")]
    Other(#[from] Box<dyn std::error::Error + Send + Sync + 'static>),
}

impl Error {
    pub fn other<E>(err: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Other(Box::new(err))
    }

    pub fn explain(msg: &str) -> Self {
        Self::Other(msg.into())
    }
}

impl From<Error> for Box<pingora::Error> {
    fn from(e: Error) -> Self {
        pingora::Error::because(pingora::ErrorType::InternalError, "internal error", e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
