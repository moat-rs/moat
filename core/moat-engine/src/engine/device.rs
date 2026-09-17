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

//! Cold positional I/O for formatting, recovery, and segment transitions.

use std::{
    fs::File,
    io::{self, Seek, SeekFrom},
    os::unix::fs::FileExt,
};

/// Positional device operations. The asynchronous queue must address this same
/// device. Implementations must report short I/O as errors and honor `sync`.
pub trait Device {
    /// Physical extent available to the engine.
    fn capacity(&self) -> io::Result<u64>;
    /// Fills the entire aligned destination or reports an error.
    fn read_at(&self, bytes: &mut [u8], offset: u64) -> io::Result<()>;
    /// Writes the entire aligned source or reports an error.
    fn write_at(&self, bytes: &[u8], offset: u64) -> io::Result<()>;
    /// Makes preceding successful writes durable.
    fn sync(&self) -> io::Result<()>;
}

impl Device for File {
    fn capacity(&self) -> io::Result<u64> {
        // metadata().len() is zero for Linux block devices. All data I/O is
        // positional, so this cold-path seek does not affect queue operations.
        let mut file = self;
        file.seek(SeekFrom::End(0))
    }
    fn read_at(&self, bytes: &mut [u8], offset: u64) -> io::Result<()> {
        self.read_exact_at(bytes, offset)
    }
    fn write_at(&self, bytes: &[u8], offset: u64) -> io::Result<()> {
        self.write_all_at(bytes, offset)
    }
    fn sync(&self) -> io::Result<()> {
        self.sync_data()
    }
}
