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

#[macro_use]
extern crate fstrings;

use std::thread::sleep;

use eyre::WrapErr;
use maplit::hashmap;
use serde_json::{json, Value};

use connected_client::ConnectedClient;
use created_swarm::make_swarms;
use network::join::join_stream;
use test_constants::KAD_TIMEOUT;

pub mod network {
    pub mod join;
}

#[test]
fn identity() {
    let swarms = make_swarms(3);
    sleep(KAD_TIMEOUT);

    let mut a = ConnectedClient::connect_to(swarms[0].multiaddr.clone())
        .wrap_err("connect client")
        .unwrap();
    let mut b = ConnectedClient::connect_to(swarms[1].multiaddr.clone())
        .wrap_err("connect client")
        .unwrap();

    a.send_particle(
        r#"
        (seq
            (call node_a ("op" "noop") [])
            (seq
                (call node_b ("op" "noop") [])
                (seq
                    (call node_c ("op" "noop") [])
                    (seq
                        (call node_b ("op" "noop") [])
                        (call client_b ("op" "noop") [])
                    )
                )
            )
        )
        "#,
        hashmap! {
            "node_a" => json!(swarms[0].peer_id.to_string()),
            "node_b" => json!(swarms[1].peer_id.to_string()),
            "node_c" => json!(swarms[2].peer_id.to_string()),
            "client_b" => json!(b.peer_id.to_string()),
        },
    );

    b.receive().wrap_err("receive").unwrap();
}

#[test]
fn init_peer_id() {
    let swarms = make_swarms(3);
    let mut client = ConnectedClient::connect_to(swarms[0].multiaddr.clone())
        .wrap_err("connect client")
        .unwrap();

    client.send_particle(
        r#"
        (seq
            (call relay ("kad" "neighborhood") [client] peers)
            (seq
                (call relay ("op" "noop") [])
                (call %init_peer_id% ("event" "peers_discovered") [relay peers])
            )
        )
        "#,
        hashmap! {
            "relay" => json!(client.node.to_string()),
            "client" => json!(client.peer_id.to_string()),
        },
    );

    client.receive().wrap_err("receive").unwrap();
}

#[test]
fn join() {
    let swarms = make_swarms(3);

    let mut client = ConnectedClient::connect_to(swarms[0].multiaddr.clone())
        .wrap_err("connect client")
        .unwrap();

    client.send_particle(
        format!(
            r#"
        (seq
            (seq
                (call relay ("op" "noop") [])
                (fold nodes n
                    (par
                        (seq
                            (call n ("op" "identity") [n] $results)
                            (seq
                                (call relay ("op" "noop") [])
                                (call %init_peer_id% ("op" "noop") [])
                            )
                        )
                        (next n)
                    )
                )
            )
            (seq
                {}
                (call %init_peer_id% ("op" "return") [#results])
            )
        )
        "#,
            join_stream("results", "%init_peer_id%", "len", "results"),
        ),
        hashmap! {
            "nodes" => json!(swarms.iter().map(|s| s.peer_id.to_base58()).collect::<Vec<_>>()),
            "client" => json!(client.peer_id.to_string()),
            "relay" => json!(client.node.to_string()),
            "len" => json!(swarms.len()),
        },
    );

    let received = client.listen_for_n(4, |peer_ids| {
        match peer_ids.as_ref().map(|v| v.as_slice()) {
            Ok(&[Value::Array(ref arr)]) => {
                assert_eq!(arr.len(), swarms.len());
                true
            }
            other => panic!(
                "expected array of {} elements, got {:?}",
                swarms.len(),
                other
            ),
        }
    });
    assert!(received);
}
