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

use std::{collections::BTreeMap, error::Error, fmt::Display, str::FromStr, time::SystemTime};

use clap::ValueEnum;
use itertools::Itertools;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
pub enum Identity {
    Proxy,
    Provider,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Peer {
    pub host: String,
    pub port: u16,
}

impl FromStr for Peer {
    type Err = ParsePeerError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts = s.trim().split(':').collect_vec();
        if parts.len() != 2 {
            return Err(ParsePeerError::InvalidFormat(s.to_string()));
        }
        let host = parts[0].to_string();
        let port = parts[1]
            .parse::<u16>()
            .map_err(|_| ParsePeerError::InvalidPort(parts[1].to_string()))?;
        Ok(Peer { host, port })
    }
}

impl Display for Peer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&format!("{}:{}", self.host, self.port))
    }
}

impl Serialize for Peer {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}
impl<'de> Deserialize<'de> for Peer {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, PartialEq, Clone)]
pub enum ParsePeerError {
    InvalidFormat(String),
    InvalidPort(String),
}

impl Error for ParsePeerError {}

impl Display for ParsePeerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParsePeerError::InvalidFormat(s) => write!(f, "Invalid peer format: {s}"),
            ParsePeerError::InvalidPort(s) => write!(f, "Invalid port in peer: {s}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Membership {
    pub last_seen: SystemTime,
    pub weight: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberList {
    pub providers: BTreeMap<Peer, Membership>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_peer_serde_json() {
        let p1: Peer = "moat-1:23456".parse().unwrap();
        let json = serde_json::to_string(&p1).unwrap();
        assert_eq!(json, r#""moat-1:23456""#);
        let p1d: Peer = serde_json::from_str(&json).unwrap();
        assert_eq!(p1, p1d);
    }

    #[test]
    fn test_member_list_serde_json() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "moat-1:23456".parse().unwrap(),
            Membership {
                last_seen: SystemTime::now(),
                weight: 1,
            },
        );
        providers.insert(
            "moat-2:23457".parse().unwrap(),
            Membership {
                last_seen: SystemTime::now(),
                weight: 1,
            },
        );
        providers.insert(
            "moat-3:23458".parse().unwrap(),
            Membership {
                last_seen: SystemTime::now(),
                weight: 1,
            },
        );
        let m = MemberList { providers };
        let json = serde_json::to_string(&m).unwrap();
        let md = serde_json::from_str::<MemberList>(&json).unwrap();
        assert_eq!(m, md);
    }
}
