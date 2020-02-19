/*
 * Copyright 2020 Fluence Labs Limited
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use serde::{Deserialize, Serialize};

/// Describes network messages from a peer to current node (client -> server).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action")]
pub enum OutEvent {
    /// Represents a message that should be relayed to given dst node.
    Relay { dst_id: Vec<u8>, data: Vec<u8> },

    /// Requests for the network state.
    /// Currently, gives the whole peers in the network, this behaviour will be refactored in future.
    GetNetworkState,
}

/// Describes network message from current node to a peer (server -> client).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action")]
pub enum InEvent {
    /// Message that should be relayed from src node to chosen dst node.
    Relay { src_id: Vec<u8>, data: Vec<u8> },

    /// Message contains all peers in the network.
    NetworkState { state: Vec<Vec<u8>> },
}

impl Default for InEvent {
    fn default() -> Self {
        InEvent::NetworkState { state: Vec::new() }
    }
}
