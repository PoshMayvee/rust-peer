/*
 * Copyright 2019 Fluence Labs Limited
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

#![deny(
    dead_code,
    nonstandard_style,
    unused_imports,
    unused_mut,
    unused_variables,
    unused_unsafe,
    unreachable_patterns
)]

mod config;
mod error;
mod node_service;
mod peer_service;

use crate::config::{NodeServiceConfig, PeerServiceConfig, WebsocketConfig, ClientType};
use crate::node_service::node_service::{start_node_service, NodeService};
use clap::{App, Arg, ArgMatches};
use ctrlc;
use async_std::task;
use futures::channel::{mpsc, oneshot};
use env_logger;
use exitfailure::ExitFailure;
use failure::_core::str::FromStr;
use log::trace;
use parity_multiaddr::Multiaddr;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const AUTHORS: &str = env!("CARGO_PKG_AUTHORS");
const DESCRIPTION: &str = env!("CARGO_PKG_DESCRIPTION");

const PEER_SERVICE_PORT: &str = "peer-service-port";
const NODE_SERVICE_PORT: &str = "node-service-port";
const CLIENT_TYPE: &str = "client-type";
const BOOTSTRAP_NODE: &str = "bootstrap-node";

fn prepare_args<'a, 'b>() -> [Arg<'a, 'b>; 4] {
    [
        Arg::with_name(PEER_SERVICE_PORT)
            .takes_value(true)
            .short("pp")
            .default_value("9999")
            .help("port that will be used by the peer service"),
        Arg::with_name(CLIENT_TYPE)
            .takes_value(true)
            .short("c")
            .default_value("websocket")
            .help("client's endpoint type: websocket, libp2p"),
        Arg::with_name(NODE_SERVICE_PORT)
            .takes_value(true)
            .short("np")
            .default_value("7777")
            .help("port that will be used by the node service"),
        Arg::with_name(BOOTSTRAP_NODE)
            .takes_value(true)
            .short("b")
            .multiple(true)
            .help("bootstrap nodes of the Fluence network"),
    ]
}

fn make_configs_from_args(
    arg_matches: ArgMatches,
) -> Result<(NodeServiceConfig, PeerServiceConfig, WebsocketConfig), ExitFailure> {
    let mut node_service_config = NodeServiceConfig::default();
    let mut peer_service_config = PeerServiceConfig::default();
    let mut websocket_config = WebsocketConfig::default();

    if let Some(peer_port) = arg_matches.value_of(PEER_SERVICE_PORT) {
        let peer_port: u16 = u16::from_str(peer_port)?;
        peer_service_config.listen_port = peer_port;
        websocket_config.listen_port = peer_port;
    }

    if let Some(node_port) = arg_matches.value_of(NODE_SERVICE_PORT) {
        let node_port: u16 = u16::from_str(node_port)?;
        node_service_config.listen_port = node_port;
    }

    if let Some(client_type) = arg_matches.value_of(CLIENT_TYPE) {
        match client_type {
            "websocket" => node_service_config.client = ClientType::Websocket,
            "libp2p" => node_service_config.client = ClientType::Libp2p,
            _ => return Err(failure::err_msg("client type should be 'websocket' or 'libp2p'").into())
        }

    }

    if let Some(bootstrap_node) = arg_matches.value_of(BOOTSTRAP_NODE) {
        let bootstrap_node = Multiaddr::from_str(bootstrap_node)?;
        node_service_config.bootstrap_nodes.push(bootstrap_node);
    }

    Ok((node_service_config, peer_service_config, websocket_config))
}

async fn start_janus(
    node_service_config: NodeServiceConfig,
    peer_service_config: PeerServiceConfig,
    websocket_config: WebsocketConfig
) -> Result<(oneshot::Sender<()>, oneshot::Sender<()>), std::io::Error> {
    trace!("starting Janus");

    let (out_sender, out_receiver) = mpsc::unbounded();
    let (in_sender, in_receiver) = mpsc::unbounded();

    let exit_sender = match node_service_config.client {
        ClientType::Libp2p => peer_service::libp2p::peer_service::start_peer_service(peer_service_config, out_receiver, in_sender),
        ClientType::Websocket => peer_service::websocket::websocket::start_peer_service(websocket_config, out_receiver, in_sender).await
    };

    let node_service = NodeService::new(node_service_config);
    let node_service_exit = start_node_service(
        node_service,
        in_receiver,
        out_sender,
    );

    Ok((node_service_exit, exit_sender))
}

fn main() -> Result<(), ExitFailure> {
    env_logger::init();

    let arg_matches = App::new("Fluence Janus protocol server")
        .version(VERSION)
        .author(AUTHORS)
        .about(DESCRIPTION)
        .args(&prepare_args())
        .get_matches();

    println!("Janus is starting...");

    let (node_service_config, peer_service_config, websocket_config) = make_configs_from_args(arg_matches)?;
    let (node_service_exit, peer_service_exit) =
        task::block_on(start_janus(node_service_config, peer_service_config, websocket_config))?;

    println!("Janus has been successfully started");

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .expect("Error setting Ctrl-C handler");

    println!("Waiting for Ctrl-C...");
    while running.load(Ordering::SeqCst) {}

    println!("shutdown services");

    node_service_exit.send(()).unwrap();
    peer_service_exit.send(()).unwrap();

    Ok(())
}
