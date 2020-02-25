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

use libp2p::PeerId;

/// Describes inner events from a node service to a peer service
/// (INcoming for peer service, hence InPeerNotification).
#[derive(Clone, Debug, PartialEq)]
pub enum InPeerNotification {
    /// Relay message from a src peer to a dst peer.
    Relay {
        src_id: PeerId,
        dst_id: PeerId,
        data: Vec<u8>,
    },
}

/// Describes inner events from peer service to node service
/// (out of peer service, hence OutPeerNotification).
#[derive(Clone, Debug, PartialEq)]
pub enum OutPeerNotification {
    /// Notifies that new peer that has been connected.
    PeerConnected { peer_id: PeerId },

    /// Notifies that some peer has been disconnected.
    PeerDisconnected { peer_id: PeerId },

    /// Message that should be relayed to other peer.
    Relay {
        src_id: PeerId,
        dst_id: PeerId,
        data: Vec<u8>,
    },
}
