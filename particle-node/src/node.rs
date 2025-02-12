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

use std::sync::Arc;
use std::{io, net::SocketAddr};

use async_std::task;
use eyre::WrapErr;
use fluence_keypair::KeyPair;
use futures::{
    channel::{mpsc::unbounded, oneshot},
    select,
    stream::StreamExt,
    FutureExt,
};
use libp2p::swarm::SwarmEvent;
use libp2p::{
    core::{muxing::StreamMuxerBox, transport::Boxed, Multiaddr},
    identity::Keypair,
    swarm::AddressScore,
    PeerId, Swarm, TransportError,
};
use libp2p_metrics::{Metrics, Recorder};
use prometheus_client::registry::Registry;

use aquamarine::{
    AquaRuntime, AquamarineApi, AquamarineApiError, AquamarineBackend, RoutingEffects, VmPoolConfig,
};
use builtins_deployer::BuiltinsDeployer;
use config_utils::to_peer_id;
use connection_pool::{ConnectionPoolApi, ConnectionPoolT};
use fluence_libp2p::types::{BackPressuredInlet, Inlet};
use fluence_libp2p::{build_transport, types::OneshotOutlet};
use key_manager::KeyManager;
use particle_builtins::{Builtins, NodeInfo};
use particle_execution::ParticleFunctionStatic;
use particle_protocol::Particle;
use peer_metrics::{
    ConnectionPoolMetrics, ConnectivityMetrics, ParticleExecutorMetrics, ServicesMetrics,
    ServicesMetricsBackend, VmPoolMetrics,
};
use script_storage::{ScriptStorageApi, ScriptStorageBackend, ScriptStorageConfig};
use server_config::{NetworkConfig, ResolvedConfig, ServicesConfig};
use sorcerer::{Sorcerer, SpellBuiltin};
use spell_event_bus::api::{PeerEvent, TriggerEvent};
use spell_event_bus::bus::SpellEventBus;

use crate::dispatcher::Dispatcher;
use crate::effectors::Effectors;
use crate::metrics::start_metrics_endpoint;
use crate::Connectivity;

use super::behaviour::FluenceNetworkBehaviour;
use crate::behaviour::FluenceNetworkBehaviourEvent;

// TODO: documentation
pub struct Node<RT: AquaRuntime> {
    particle_stream: BackPressuredInlet<Particle>,
    effects_stream: Inlet<Result<RoutingEffects, AquamarineApiError>>,
    pub swarm: Swarm<FluenceNetworkBehaviour>,

    pub connectivity: Connectivity,
    pub aquamarine_api: AquamarineApi,
    pub dispatcher: Dispatcher,
    aquavm_pool: AquamarineBackend<RT, Arc<Builtins<Connectivity>>>,
    script_storage: ScriptStorageBackend,
    builtins_deployer: BuiltinsDeployer,
    spell_event_bus: SpellEventBus,
    spell_events_stream: Inlet<TriggerEvent>,
    sorcerer: Sorcerer,

    registry: Option<Registry>,
    services_metrics_backend: ServicesMetricsBackend,

    metrics_listen_addr: SocketAddr,

    pub builtins_management_peer_id: PeerId,

    pub key_manager: KeyManager,
}

impl<RT: AquaRuntime> Node<RT> {
    pub fn new(
        config: ResolvedConfig,
        vm_config: RT::Config,
        node_version: &'static str,
    ) -> eyre::Result<Box<Self>> {
        let key_pair: Keypair = config.node_config.root_key_pair.clone().into();
        let transport = config.transport_config.transport;
        let transport = build_transport(
            transport,
            key_pair.clone(),
            config.transport_config.socket_timeout,
        );

        let builtins_peer_id = to_peer_id(&config.builtins_key_pair.clone().into());

        let key_manager = KeyManager::new(
            config.dir_config.keypairs_base_dir.clone(),
            to_peer_id(&key_pair),
        );

        let services_config = ServicesConfig::new(
            key_manager.get_host_peer_id(),
            config.dir_config.services_base_dir.clone(),
            config_utils::particles_vault_dir(&config.dir_config.avm_base_dir),
            config.services_envs.clone(),
            config.management_peer_id,
            builtins_peer_id,
            config.node_config.module_max_heap_size,
            config.node_config.module_default_heap_size,
        )
        .expect("create services config");

        let mut metrics_registry = if config.metrics_config.metrics_enabled {
            Some(Registry::default())
        } else {
            None
        };
        let libp2p_metrics = metrics_registry.as_mut().map(Metrics::new);
        let connectivity_metrics = metrics_registry.as_mut().map(ConnectivityMetrics::new);
        let connection_pool_metrics = metrics_registry.as_mut().map(ConnectionPoolMetrics::new);
        let plumber_metrics = metrics_registry.as_mut().map(ParticleExecutorMetrics::new);
        let vm_pool_metrics = metrics_registry.as_mut().map(VmPoolMetrics::new);

        let network_config = NetworkConfig::new(
            libp2p_metrics,
            connectivity_metrics,
            connection_pool_metrics,
            key_pair,
            &config,
            node_version,
        );

        let (swarm, connectivity, particle_stream) = Self::swarm(
            key_manager.get_host_peer_id(),
            network_config,
            transport,
            config.external_addresses(),
        );

        let (particle_failures_out, particle_failures_in) = unbounded();

        let (script_storage_api, script_storage_backend) = {
            let script_storage_config = ScriptStorageConfig {
                timer_resolution: config.script_storage_timer_resolution,
                max_failures: config.script_storage_max_failures,
                particle_ttl: config.script_storage_particle_ttl,
                peer_id: key_manager.get_host_peer_id(),
            };

            let pool: &ConnectionPoolApi = connectivity.as_ref();
            ScriptStorageBackend::new(pool.clone(), particle_failures_in, script_storage_config)
        };

        let (services_metrics_backend, services_metrics) =
            if let Some(registry) = metrics_registry.as_mut() {
                ServicesMetrics::with_external_backend(
                    config.metrics_config.metrics_timer_resolution,
                    config.metrics_config.max_builtin_metrics_storage_size,
                    registry,
                )
            } else {
                ServicesMetrics::with_simple_backend(
                    config.metrics_config.max_builtin_metrics_storage_size,
                )
            };

        let builtins = Arc::new(Self::builtins(
            connectivity.clone(),
            config.external_addresses(),
            services_config,
            script_storage_api,
            services_metrics,
            config.node_config.root_key_pair.clone(),
        ));

        let (effects_out, effects_in) = unbounded();

        let pool_config =
            VmPoolConfig::new(config.aquavm_pool_size, config.particle_execution_timeout);
        let (aquavm_pool, aquamarine_api) = AquamarineBackend::new(
            pool_config,
            vm_config,
            Arc::clone(&builtins),
            effects_out,
            plumber_metrics,
            vm_pool_metrics,
            key_manager.clone(),
        );
        let effectors = Effectors::new(connectivity.clone());
        let dispatcher = {
            let failures = particle_failures_out;
            let parallelism = config.particle_processor_parallelism;
            Dispatcher::new(
                key_manager.get_host_peer_id(),
                aquamarine_api.clone(),
                effectors,
                failures,
                parallelism,
                metrics_registry.as_mut(),
            )
        };

        let builtins_deployer = BuiltinsDeployer::new(
            builtins_peer_id,
            key_manager.get_host_peer_id(),
            aquamarine_api.clone(),
            config.dir_config.builtins_base_dir.clone(),
            config.node_config.autodeploy_particle_ttl,
            config.node_config.force_builtins_redeploy,
            config.node_config.autodeploy_retry_attempts,
        );

        let recv_connection_pool_events = connectivity.connection_pool.lifecycle_events();
        let sources = vec![recv_connection_pool_events.map(PeerEvent::from).boxed()];

        let (spell_event_bus, spell_event_bus_api, spell_events_stream) =
            SpellEventBus::new(sources);

        let (sorcerer, spell_service_functions) = Sorcerer::new(
            builtins.services.clone(),
            builtins.modules.clone(),
            aquamarine_api.clone(),
            config.clone(),
            spell_event_bus_api,
            key_manager.clone(),
        );

        spell_service_functions.into_iter().for_each(
            |SpellBuiltin {
                 service_id,
                 functions,
                 unhandled,
             }| builtins.extend(service_id, functions, unhandled),
        );

        Ok(Self::with(
            particle_stream,
            effects_in,
            swarm,
            connectivity,
            aquamarine_api,
            dispatcher,
            aquavm_pool,
            script_storage_backend,
            builtins_deployer,
            spell_event_bus,
            spell_events_stream,
            sorcerer,
            metrics_registry,
            services_metrics_backend,
            config.metrics_listen_addr(),
            builtins_peer_id,
            key_manager,
        ))
    }

    pub fn swarm(
        local_peer_id: PeerId,
        network_config: NetworkConfig,
        transport: Boxed<(PeerId, StreamMuxerBox)>,
        external_addresses: Vec<Multiaddr>,
    ) -> (
        Swarm<FluenceNetworkBehaviour>,
        Connectivity,
        BackPressuredInlet<Particle>,
    ) {
        let (behaviour, connectivity, particle_stream) =
            FluenceNetworkBehaviour::new(network_config);
        let mut swarm = Swarm::new(transport, behaviour, local_peer_id);

        // Add external addresses to Swarm
        external_addresses.iter().cloned().for_each(|addr| {
            Swarm::add_external_address(&mut swarm, addr, AddressScore::Finite(1));
        });

        (swarm, connectivity, particle_stream)
    }

    pub fn builtins(
        connectivity: Connectivity,
        external_addresses: Vec<Multiaddr>,
        services_config: ServicesConfig,
        script_storage_api: ScriptStorageApi,
        services_metrics: ServicesMetrics,
        root_keypair: KeyPair,
    ) -> Builtins<Connectivity> {
        let node_info = NodeInfo {
            external_addresses,
            node_version: env!("CARGO_PKG_VERSION"),
            air_version: air_interpreter_wasm::VERSION,
        };

        Builtins::new(
            connectivity,
            script_storage_api,
            node_info,
            services_config,
            services_metrics,
            root_keypair,
        )
    }
}

impl<RT: AquaRuntime> Node<RT> {
    #[allow(clippy::too_many_arguments)]
    pub fn with(
        particle_stream: BackPressuredInlet<Particle>,
        effects_stream: Inlet<Result<RoutingEffects, AquamarineApiError>>,
        swarm: Swarm<FluenceNetworkBehaviour>,

        connectivity: Connectivity,
        aquamarine_api: AquamarineApi,
        dispatcher: Dispatcher,
        aquavm_pool: AquamarineBackend<RT, Arc<Builtins<Connectivity>>>,
        script_storage: ScriptStorageBackend,
        builtins_deployer: BuiltinsDeployer,
        spell_event_bus: SpellEventBus,
        spell_events_stream: Inlet<TriggerEvent>,
        sorcerer: Sorcerer,

        registry: Option<Registry>,
        services_metrics_backend: ServicesMetricsBackend,
        metrics_listen_addr: SocketAddr,

        builtins_management_peer_id: PeerId,
        key_manager: KeyManager,
    ) -> Box<Self> {
        let node_service = Self {
            particle_stream,
            effects_stream,
            swarm,

            connectivity,
            aquamarine_api,
            dispatcher,
            aquavm_pool,
            script_storage,
            builtins_deployer,
            spell_event_bus,
            spell_events_stream,
            sorcerer,

            registry,
            services_metrics_backend,
            metrics_listen_addr,

            builtins_management_peer_id,
            key_manager,
        };

        Box::new(node_service)
    }

    /// Starts node service
    #[allow(clippy::boxed_local)] // Mike said it should be boxed
    pub fn start(self: Box<Self>) -> eyre::Result<OneshotOutlet<()>> {
        let (exit_outlet, exit_inlet) = oneshot::channel();
        let mut exit_inlet = exit_inlet.into_stream().fuse();

        let particle_stream = self.particle_stream;
        let effects_stream = self.effects_stream;
        let mut swarm = self.swarm;
        let connectivity = self.connectivity;
        let dispatcher = self.dispatcher;
        let aquavm_pool = self.aquavm_pool;
        let script_storage = self.script_storage;
        let spell_event_bus = self.spell_event_bus;
        let spell_events_stream = self.spell_events_stream;
        let sorcerer = self.sorcerer;
        let registry = self.registry;
        let services_metrics_backend = self.services_metrics_backend;
        let metrics_listen_addr = self.metrics_listen_addr;

        task::spawn(async move {
            let (metrics_fut, libp2p_metrics) = if let Some(mut registry) = registry {
                let libp2p_metrics = Metrics::new(&mut registry);
                let fut = start_metrics_endpoint(registry, metrics_listen_addr);
                (fut, Some(libp2p_metrics))
            } else {
                (futures::future::ready(Ok(())).boxed(), None)
            };
            let mut metrics_fut = metrics_fut.fuse();

            let services_metrics_backend = services_metrics_backend.start();
            let script_storage = script_storage.start();
            let spell_event_bus = spell_event_bus.start();
            let sorcerer = sorcerer.start(spell_events_stream);
            let pool = aquavm_pool.start();
            let mut connectivity = connectivity.start();
            let mut dispatcher = dispatcher.start(particle_stream, effects_stream);

            loop {
                select!(
                    e = swarm.select_next_some() => {
                        if let Some(m) = libp2p_metrics.as_ref() { m.record(&e) }
                        if let SwarmEvent::Behaviour(FluenceNetworkBehaviourEvent::Identify(i)) = e {
                            swarm.behaviour_mut().inject_identify_event(i, true)
                        }
                    },
                    e = metrics_fut => {
                        if let Err(err) = e {
                            log::warn!("Metrics returned error: {}", err)
                        }
                    },
                    _ = connectivity => {},
                    _ = dispatcher => {},
                    event = exit_inlet.next() => {
                        // Ignore Err and None – if exit_outlet is dropped, we'll run forever!
                        if let Some(Ok(_)) = event {
                            break
                        }
                    }
                )
            }

            log::info!("Stopping node");
            services_metrics_backend.cancel().await;
            script_storage.cancel().await;
            spell_event_bus.cancel().await;
            sorcerer.cancel().await;
            dispatcher.cancel().await;
            connectivity.cancel().await;
            pool.cancel().await;
        });

        let mut builtins_deployer = self.builtins_deployer;
        builtins_deployer
            .deploy_builtin_services()
            .wrap_err("builtins deploy failed")?;

        Ok(exit_outlet)
    }

    /// Starts node service listener.
    #[inline]
    pub fn listen(
        &mut self,
        addrs: impl Into<Vec<Multiaddr>>,
    ) -> Result<(), TransportError<io::Error>> {
        let addrs = addrs.into();
        log::info!("Fluence listening on {:?}", addrs);

        for addr in addrs {
            Swarm::listen_on(&mut self.swarm, addr)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use eyre::WrapErr;
    use libp2p::core::Multiaddr;
    use maplit::hashmap;
    use serde_json::json;
    use std::path::PathBuf;

    use air_interpreter_fs::{air_interpreter_path, write_default_air_interpreter};
    use aquamarine::{VmConfig, AVM};
    use config_utils::to_peer_id;
    use connected_client::ConnectedClient;
    use fs_utils::to_abs_path;
    use server_config::{builtins_base_dir, default_base_dir, resolve_config};

    use crate::Node;

    #[test]
    fn run_node() {
        let base_dir = default_base_dir();
        fs_utils::create_dir(&base_dir).unwrap();
        fs_utils::create_dir(builtins_base_dir(&base_dir)).unwrap();
        write_default_air_interpreter(&air_interpreter_path(&base_dir)).unwrap();

        let mut config = resolve_config(&<_>::default(), &[]).expect("deserialize config");
        config.aquavm_pool_size = 1;
        config.dir_config.spell_base_dir = to_abs_path(PathBuf::from("spell"));
        let vm_config = VmConfig::new(
            to_peer_id(&config.root_key_pair.clone().into()),
            config.dir_config.avm_base_dir.clone(),
            config.dir_config.air_interpreter_path.clone(),
            None,
        );
        let mut node: Box<Node<AVM<_>>> =
            Node::new(config, vm_config, "some version").expect("create node");

        let listening_address: Multiaddr = "/ip4/127.0.0.1/tcp/7777".parse().unwrap();
        node.listen(vec![listening_address.clone()]).unwrap();
        node.start().expect("start node");

        let mut client = ConnectedClient::connect_to(listening_address).expect("connect client");
        let data = hashmap! {
            "name" => json!("folex"),
            "client" => json!(client.peer_id.to_string()),
            "relay" => json!(client.node.to_string()),
        };
        client.send_particle(
            r#"
                (seq
                    (call relay ("op" "identity") [])
                    (call client ("return" "") [name])
                )
            "#,
            data.clone(),
        );
        client.receive_args().wrap_err("receive args").unwrap();
    }
}
