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

use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::ops::Try;
use std::path;
use std::str::FromStr;
use std::time::{Duration, Instant};

use derivative::Derivative;
use fluence_keypair::{KeyPair, Signature};
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use humantime_serde::re::humantime::format_duration as pretty;
use libp2p::{core::Multiaddr, kad::kbucket::Key, kad::K_VALUE, PeerId};
use multihash::{Code, MultihashDigest, MultihashGeneric};
use parking_lot::{Mutex, RwLock};
use serde::Deserialize;
use serde_json::{json, Value as JValue};
use JValue::Array;

use connection_pool::{ConnectionPoolApi, ConnectionPoolT};
use kademlia::{KademliaApi, KademliaApiT};
use now_millis::{now_ms, now_sec};
use particle_args::{from_base58, Args, ArgsError, JError};
use particle_execution::{FunctionOutcome, ParticleParams, ServiceFunction};
use particle_modules::{
    AddBlueprint, ModuleConfig, ModuleRepository, NamedModuleConfig, WASIConfig,
};
use particle_protocol::Contact;
use particle_services::{ParticleAppServices, VIRTUAL_PARTICLE_VAULT_PREFIX};
use peer_metrics::ServicesMetrics;
use script_storage::ScriptStorageApi;
use server_config::ServicesConfig;

use crate::debug::fmt_custom_services;
use crate::error::HostClosureCallError;
use crate::error::HostClosureCallError::{DecodeBase58, DecodeUTF8};
use crate::func::{binary, unary};
use crate::identify::NodeInfo;
use crate::outcome::{ok, wrap, wrap_unit};
use crate::{json, math};

pub struct CustomService {
    /// (function_name -> service function)
    pub functions: HashMap<String, Mutex<ServiceFunction>>,
    /// if set, all `function_name` mismatches with `custom_service.functions` will be routed to `unhandled`
    pub unhandled: Option<Mutex<ServiceFunction>>,
}

#[derive(Derivative)]
#[derivative(Debug)]
pub struct Builtins<C> {
    pub connectivity: C,
    pub script_storage: ScriptStorageApi,

    pub management_peer_id: PeerId,
    pub builtins_management_peer_id: PeerId,
    pub local_peer_id: PeerId,
    #[derivative(Debug = "ignore")]
    pub root_keypair: KeyPair,

    pub modules: ModuleRepository,
    pub services: ParticleAppServices,
    pub node_info: NodeInfo,

    #[derivative(Debug(format_with = "fmt_custom_services"))]
    pub custom_services: RwLock<HashMap<String, CustomService>>,

    particles_vault_dir: path::PathBuf,
}

impl<C> Builtins<C>
where
    C: Clone + Send + Sync + 'static + AsRef<KademliaApi> + AsRef<ConnectionPoolApi>,
{
    pub fn new(
        connectivity: C,
        script_storage: ScriptStorageApi,
        node_info: NodeInfo,
        config: ServicesConfig,
        services_metrics: ServicesMetrics,
        root_keypair: KeyPair,
    ) -> Self {
        let modules_dir = &config.modules_dir;
        let blueprint_dir = &config.blueprint_dir;
        let vault_dir = &config.particles_vault_dir;
        let modules = ModuleRepository::new(
            modules_dir,
            blueprint_dir,
            vault_dir,
            config.max_heap_size,
            config.default_heap_size,
        );
        let particles_vault_dir = vault_dir.to_path_buf();
        let management_peer_id = config.management_peer_id;
        let builtins_management_peer_id = config.builtins_management_peer_id;
        let local_peer_id = config.local_peer_id;
        let services = ParticleAppServices::new(config, modules.clone(), Some(services_metrics));

        Self {
            connectivity,
            script_storage,
            management_peer_id,
            builtins_management_peer_id,
            local_peer_id,
            root_keypair,
            modules,
            services,
            node_info,
            particles_vault_dir,
            custom_services: <_>::default(),
        }
    }

    pub async fn call(&self, args: Args, particle: ParticleParams) -> FunctionOutcome {
        let start = Instant::now();
        let result = self.builtins_call(args, particle).await;
        let end = start.elapsed().as_secs();
        match result {
            FunctionOutcome::NotDefined { args, params } => self
                .custom_service_call(args, params)
                .or_else(|args, params| self.call_service(args, params)),
            result => {
                if let Some(metrics) = self.services.metrics.as_ref() {
                    metrics.observe_builtins(result.not_err(), end as f64);
                }
                result
            }
        }
    }

    pub fn custom_service_call(&self, args: Args, particle: ParticleParams) -> FunctionOutcome {
        if let Some(function) = self
            .custom_services
            .read()
            .get(&args.service_id)
            .and_then(|fs| {
                fs.functions
                    .get(&args.function_name)
                    .or(fs.unhandled.as_ref())
            })
        {
            let mut function = function.lock();
            async_std::task::block_on(function(args, particle))
        } else {
            FunctionOutcome::NotDefined {
                args,
                params: particle,
            }
        }
    }

    // TODO: get rid of all blocking methods (std::fs and such)
    pub async fn builtins_call(&self, args: Args, particle: ParticleParams) -> FunctionOutcome {
        use Result as R;
        #[rustfmt::skip]
        match (args.service_id.as_str(), args.function_name.as_str()) {
            ("peer", "identify")              => ok(json!(self.node_info)),
            ("peer", "timestamp_ms")          => ok(json!(now_ms() as u64)),
            ("peer", "timestamp_sec")         => ok(json!(now_sec())),
            ("peer", "is_connected")          => wrap(self.is_connected(args).await),
            ("peer", "connect")               => wrap(self.connect(args).await),
            ("peer", "get_contact")           => self.get_contact(args).await,
            ("peer", "timeout")               => self.timeout(args).await,

            ("kad", "neighborhood")           => wrap(self.neighborhood(args).await),
            ("kad", "neigh_with_addrs")       => wrap(self.neighborhood_with_addresses(args).await),
            ("kad", "merge")                  => wrap(self.kad_merge(args.function_args)),

            ("srv", "list")                   => ok(self.list_services()),
            ("srv", "create")                 => wrap(self.create_service(args, particle)),
            ("srv", "get_interface")          => wrap(self.get_interface(args)),
            ("srv", "resolve_alias")          => wrap(self.resolve_alias(args)),
            ("srv", "add_alias")              => wrap_unit(self.add_alias(args, particle)),
            ("srv", "remove")                 => wrap_unit(self.remove_service(args, particle)),

            ("dist", "add_module_from_vault") => wrap(self.add_module_from_vault(args, particle)),
            ("dist", "add_module")            => wrap(self.add_module(args)),
            ("dist", "add_blueprint")         => wrap(self.add_blueprint(args)),
            ("dist", "make_module_config")    => wrap(make_module_config(args)),
            ("dist", "load_module_config")    => wrap(self.load_module_config_from_vault(args, particle)),
            ("dist", "default_module_config") => wrap(self.default_module_config(args)),
            ("dist", "make_blueprint")        => wrap(self.make_blueprint(args)),
            ("dist", "load_blueprint")        => wrap(self.load_blueprint_from_vault(args, particle)),
            ("dist", "list_modules")          => wrap(self.list_modules()),
            ("dist", "get_module_interface")  => wrap(self.get_module_interface(args)),
            ("dist", "list_blueprints")       => wrap(self.get_blueprints()),

            ("script", "add")                 => wrap(self.add_script_from_arg(args, particle)),
            ("script", "add_from_vault")      => wrap(self.add_script_from_vault(args, particle)),
            ("script", "remove")              => wrap(self.remove_script(args, particle).await),
            ("script", "list")                => wrap(self.list_scripts().await),

            ("op", "noop")                    => FunctionOutcome::Empty,
            ("op", "array")                   => ok(Array(args.function_args)),
            ("op", "array_length")            => wrap(self.array_length(args.function_args)),
            ("op", "concat")                  => wrap(self.concat(args.function_args)),
            ("op", "string_to_b58")           => wrap(self.string_to_b58(args.function_args)),
            ("op", "string_from_b58")         => wrap(self.string_from_b58(args.function_args)),
            ("op", "bytes_from_b58")          => wrap(self.bytes_from_b58(args.function_args)),
            ("op", "bytes_to_b58")            => wrap(self.bytes_to_b58(args.function_args)),
            ("op", "sha256_string")           => wrap(self.sha256_string(args.function_args)),
            ("op", "concat_strings")          => wrap(self.concat_strings(args.function_args)),
            ("op", "identity")                => self.identity(args.function_args),

            ("debug", "stringify")            => self.stringify(args.function_args),

            ("stat", "service_memory") => unary(args, |id: String| -> R<Vec<JValue>, _> { self.services.get_service_mem_stats(id) }),
            ("stat", "service_stat")   => wrap(self.service_stat(args)),

            ("math", "add")        => binary(args, |x: i64, y: i64| -> R<i64, _> { math::add(x, y) }),
            ("math", "sub")        => binary(args, |x: i64, y: i64| -> R<i64, _> { math::sub(x, y) }),
            ("math", "mul")        => binary(args, |x: i64, y: i64| -> R<i64, _> { math::mul(x, y) }),
            ("math", "fmul")       => binary(args, |x: f64, y: f64| -> R<i64, _> { math::fmul_floor(x, y) }),
            ("math", "div")        => binary(args, |x: i64, y: i64| -> R<i64, _> { math::div(x, y) }),
            ("math", "rem")        => binary(args, |x: i64, y: i64| -> R<i64, _> { math::rem(x, y) }),
            ("math", "pow")        => binary(args, |x: i64, y: u32| -> R<i64, _> { math::pow(x, y) }),
            ("math", "log")        => binary(args, |x: i64, y: i64| -> R<u32, _> { math::log(x, y) }),

            ("cmp", "gt")          => binary(args, |x: i64, y: i64| -> R<bool, _> { math::gt(x, y) }),
            ("cmp", "gte")         => binary(args, |x: i64, y: i64| -> R<bool, _> { math::gte(x, y) }),
            ("cmp", "lt")          => binary(args, |x: i64, y: i64| -> R<bool, _> { math::lt(x, y) }),
            ("cmp", "lte")         => binary(args, |x: i64, y: i64| -> R<bool, _> { math::lte(x, y) }),
            ("cmp", "cmp")         => binary(args, |x: i64, y: i64| -> R<i8, _> { math::cmp(x, y) }),

            ("array", "sum")       => unary(args, |xs: Vec<i64> | -> R<i64, _> { math::array_sum(xs) }),
            ("array", "dedup")     => unary(args, |xs: Vec<String>| -> R<Vec<String>, _> { math::dedup(xs) }),
            ("array", "intersect") => binary(args, |xs: HashSet<String>, ys: HashSet<String>| -> R<Vec<String>, _> { math::intersect(xs, ys) }),
            ("array", "diff")      => binary(args, |xs: HashSet<String>, ys: HashSet<String>| -> R<Vec<String>, _> { math::diff(xs, ys) }),
            ("array", "sdiff")     => binary(args, |xs: HashSet<String>, ys: HashSet<String>| -> R<Vec<String>, _> { math::sdiff(xs, ys) }),
            ("array", "slice")     => wrap(self.array_slice(args.function_args)),
            ("array", "length")    => wrap(self.array_length(args.function_args)),

            ("sig", "sign")        => wrap(self.sign(args)),
            ("sig", "verify")      => wrap(self.verify(args)),
            ("sig", "get_peer_id") => wrap(self.get_peer_id()),

            ("json", "obj")        => wrap(json::obj(args)),
            ("json", "put")        => wrap(json::put(args)),
            ("json", "puts")       => wrap(json::puts(args)),
            ("json", "parse")      => unary(args, |s: String| -> R<JValue, _> { json::parse(&s) }),
            ("json", "stringify")  => unary(args, |v: JValue| -> R<String, _> { Ok(json::stringify(v)) }),

            _                      => FunctionOutcome::NotDefined { args, params: particle },
        }
    }

    async fn neighbor_peers(&self, args: Args) -> Result<Vec<PeerId>, JError> {
        let mut args = args.function_args.into_iter();
        let key = from_base58("key", &mut args)?;
        let already_hashed: Option<bool> = Args::next_opt("already_hashed", &mut args)?;
        let count: Option<usize> = Args::next_opt("count", &mut args)?;
        let count = count.unwrap_or_else(|| K_VALUE.get());

        let key = if already_hashed == Some(true) {
            MultihashGeneric::from_bytes(&key)?
        } else {
            Code::Sha2_256.digest(&key)
        };
        let neighbors = self.kademlia().neighborhood(key, count).await?;

        Ok(neighbors)
    }

    async fn neighborhood(&self, args: Args) -> Result<JValue, JError> {
        let neighbors = self.neighbor_peers(args).await?;
        let neighbors = json!(neighbors
            .into_iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>());

        Ok(neighbors)
    }

    async fn neighborhood_with_addresses(&self, args: Args) -> Result<JValue, JError> {
        let neighbors = self.neighbor_peers(args).await?;
        let neighbors = neighbors
            .into_iter()
            .map(|peer| async move {
                let contact = self.connection_pool().get_contact(peer).await;
                (peer, contact)
            })
            .collect::<FuturesUnordered<_>>()
            .map(|(peer_id, contact)| {
                json!({
                    "peer_id": peer_id.to_string(),
                    "addresses": contact.map(|c| c.addresses).unwrap_or_default()
                })
            })
            .collect::<Vec<_>>()
            .await;
        let neighbors = json!(neighbors);

        Ok(neighbors)
    }

    async fn is_connected(&self, args: Args) -> Result<JValue, JError> {
        let peer: String = Args::next("peer_id", &mut args.function_args.into_iter())?;
        let peer = PeerId::from_str(peer.as_str())?;
        let ok = self.connection_pool().is_connected(peer).await;
        Ok(json!(ok))
    }

    async fn connect(&self, args: Args) -> Result<JValue, JError> {
        let mut args = args.function_args.into_iter();

        let peer_id: String = Args::next("peer_id", &mut args)?;
        let peer_id = PeerId::from_str(peer_id.as_str())?;
        let addrs: Vec<Multiaddr> = Args::next_opt("addresses", &mut args)?.unwrap_or_default();

        let contact = Contact::new(peer_id, addrs);

        let ok = self.connection_pool().connect(contact).await;
        Ok(json!(ok))
    }

    async fn get_contact(&self, args: Args) -> FunctionOutcome {
        let peer: String = Args::next("peer_id", &mut args.function_args.into_iter())?;
        let peer = PeerId::from_str(peer.as_str())?;
        let contact = self.connection_pool().get_contact(peer).await;
        match contact {
            Some(c) => FunctionOutcome::Ok(json!(c)),
            None => FunctionOutcome::Empty,
        }
    }

    fn add_script_from_arg(&self, args: Args, params: ParticleParams) -> Result<JValue, JError> {
        let mut args = args.function_args.into_iter();
        let script: String = Args::next("script", &mut args)?;
        self.add_script(args, params, script)
    }

    fn add_script_from_vault(&self, args: Args, params: ParticleParams) -> Result<JValue, JError> {
        let mut args = args.function_args.into_iter();

        let path: String = Args::next("path", &mut args)?;
        let script = self.read_script_from_vault(path::Path::new(&path), &params.id)?;
        self.add_script(args, params, script)
    }

    fn add_script(
        &self,
        mut args: std::vec::IntoIter<JValue>,
        params: ParticleParams,
        script: String,
    ) -> Result<JValue, JError> {
        let interval = parse_from_str("interval_sec", &mut args)?;
        let interval = interval.map(Duration::from_secs);

        let delay = parse_from_str("delay_sec", &mut args)?;
        let delay = delay.map(Duration::from_secs);
        let delay = get_delay(delay, interval);

        let creator = params.init_peer_id;

        let id = self
            .script_storage
            .add_script(script, interval, delay, creator)?;

        Ok(json!(id))
    }

    fn read_script_from_vault(
        &self,
        path: &path::Path,
        particle_id: &str,
    ) -> Result<String, JError> {
        let resolved_path = resolve_vault_path(&self.particles_vault_dir, path, particle_id)?;
        std::fs::read_to_string(resolved_path)
            .map_err(|_| JError::new(format!("Error reading script file `{}`", path.display())))
    }

    async fn remove_script(&self, args: Args, params: ParticleParams) -> Result<JValue, JError> {
        let mut args = args.function_args.into_iter();

        let force = params.init_peer_id == self.management_peer_id;

        let uuid: String = Args::next("uuid", &mut args)?;
        let actor = params.init_peer_id;

        let ok = self
            .script_storage
            .remove_script(uuid, actor, force)
            .await?;

        Ok(json!(ok))
    }

    async fn list_scripts(&self) -> Result<JValue, JError> {
        let scripts = self.script_storage.list_scripts().await?;

        Ok(JValue::Array(
            scripts
                .into_iter()
                .map(|(id, script)| {
                    let id: &String = id.borrow();
                    json!({
                        "id": id,
                        "src": script.src,
                        "failures": script.failures,
                        "interval": script.interval.map(|i| pretty(i).to_string()),
                        "owner": script.creator.to_string(),
                    })
                })
                .collect(),
        ))
    }

    async fn timeout(&self, args: Args) -> FunctionOutcome {
        use async_std::future;
        use std::future::pending;

        let mut args = args.function_args.into_iter();

        let dur_field = "duration_ms";
        let duration = parse_from_str(dur_field, &mut args)?;
        let duration = duration.ok_or(ArgsError::MissingField(dur_field))?;
        let duration = Duration::from_millis(duration);

        let message = Args::next_opt("message", &mut args)?;

        // sleep for `duration`
        future::timeout(duration, pending::<()>()).await.ok();

        message
            .map(|msg: String| FunctionOutcome::Ok(msg.into()))
            .unwrap_or(FunctionOutcome::Empty)
    }

    fn string_to_b58(&self, args: Vec<serde_json::Value>) -> Result<JValue, JError> {
        let mut args = args.into_iter();
        let string: String = Args::next("string", &mut args)?;
        let b58 = bs58::encode(string).into_string();
        Ok(JValue::String(b58))
    }

    /// Attempts to decode UTF8 string from a given base58 string
    /// May fail at base58 decoding and on UTF8 decoding
    fn string_from_b58(&self, args: Vec<serde_json::Value>) -> Result<JValue, JError> {
        let mut args = args.into_iter();
        let string: String = Args::next("b58_string", &mut args)?;
        let vec = bs58::decode(string).into_vec().map_err(DecodeBase58)?;
        let string = String::from_utf8(vec).map_err(DecodeUTF8)?;
        Ok(JValue::String(string))
    }

    fn bytes_from_b58(&self, args: Vec<serde_json::Value>) -> Result<JValue, JError> {
        let mut args = args.into_iter();
        let string: String = Args::next("b58_string", &mut args)?;
        let vec = bs58::decode(string).into_vec().map_err(DecodeBase58)?;
        Ok(json!(vec))
    }

    fn bytes_to_b58(&self, args: Vec<serde_json::Value>) -> Result<JValue, JError> {
        let mut args = args.into_iter();
        let bytes: Vec<u8> = Args::next("bytes", &mut args)?;
        let string = bs58::encode(bytes).into_string();
        Ok(JValue::String(string))
    }

    /// Returns SHA256 of the passed string
    /// Accepts 3 arguments:
    /// `string` – string to hash
    /// `digest_only` boolean – if set to true, return only SHA256 digest, otherwise (by default) – full multihash
    /// `as_bytes` boolean - if set to true, return result as array of bytes, otherwise (by default) – as base58 string
    fn sha256_string(&self, args: Vec<serde_json::Value>) -> Result<JValue, JError> {
        let mut args = args.into_iter();
        let string: String = Args::next("string", &mut args)?;
        let digest_only: Option<bool> = Args::next_opt("digest_only", &mut args)?;
        let as_bytes: Option<bool> = Args::next_opt("as_bytes", &mut args)?;
        let multihash = Code::Sha2_256.digest(string.as_bytes());

        let result = if digest_only == Some(true) {
            multihash.digest().to_vec()
        } else {
            multihash.to_bytes()
        };

        if as_bytes == Some(true) {
            Ok(json!(result))
        } else {
            let b58 = bs58::encode(result).into_string();
            Ok(JValue::String(b58))
        }
    }

    /// Merge, sort by distance to first key, return top K
    /// K is optional. If not passed, all elements are returned.
    fn kad_merge(&self, args: Vec<serde_json::Value>) -> Result<JValue, JError> {
        let mut args = args.into_iter();
        let target: String = Args::next("target", &mut args)?;
        let left: Vec<String> = Args::next("left", &mut args)?;
        let right: Vec<String> = Args::next("right", &mut args)?;
        let count: Option<usize> = Args::next_opt("count", &mut args)?;
        let count = count.unwrap_or_else(|| K_VALUE.get());

        let target = bs58::decode(target).into_vec().map_err(DecodeBase58)?;
        let target = Key::from(target);
        let left = left.into_iter();
        let right = right.into_iter();

        let mut keys: Vec<Key<_>> = left
            .chain(right)
            .map(|b58_str| {
                Ok(Key::from(
                    bs58::decode(b58_str).into_vec().map_err(DecodeBase58)?,
                ))
            })
            .collect::<Result<Vec<_>, HostClosureCallError>>()?;
        keys.sort_by_cached_key(|k| target.distance(k.as_ref()));
        keys.dedup();

        let keys = keys
            .into_iter()
            .map(|k| bs58::encode(k.into_preimage()).into_string());

        let keys: Vec<_> = keys.take(count).collect();

        Ok(json!(keys))
    }

    fn identity(&self, args: Vec<serde_json::Value>) -> FunctionOutcome {
        if args.len() > 1 {
            FunctionOutcome::Err(JError::new(format!(
                "identity accepts up to 1 arguments, received {} arguments",
                args.len()
            )))
        } else {
            Try::from_output(args.into_iter().next())
        }
    }

    fn stringify(&self, args: Vec<serde_json::Value>) -> FunctionOutcome {
        let debug = if args.is_empty() {
            // return valid JSON string
            r#""<empty argument list>""#.to_string()
        } else if args.len() == 1 {
            args[0].to_string()
        } else {
            JValue::Array(args).to_string()
        };

        FunctionOutcome::Ok(JValue::String(debug))
    }

    /// Flattens an array of arrays
    fn concat(&self, args: Vec<serde_json::Value>) -> Result<JValue, JError> {
        let flattened: Vec<JValue> =
            args.into_iter()
                .enumerate()
                .try_fold(vec![], |mut acc, (i, v)| match v {
                    JValue::Array(mut array) => {
                        acc.append(&mut array);
                        Ok(acc)
                    }
                    _ => Err(JError::new(format!(
                        "all arguments of 'concat' must be arrays: argument #{i} is not"
                    ))),
                })?;

        Ok(JValue::Array(flattened))
    }

    /// Concatenates an array of arrays
    fn concat_strings(&self, args: Vec<serde_json::Value>) -> Result<JValue, JError> {
        let string: String =
            args.into_iter()
                .enumerate()
                .try_fold(String::new(), |mut acc, (i, v)| match v {
                    JValue::String(s) => {
                        acc.push_str(&s);
                        Ok(acc)
                    }
                    _ => Err(JError::new(format!(
                        "all arguments of 'concat_strings' must be strings: argument #{i} is not"
                    ))),
                })?;

        Ok(JValue::String(string))
    }

    fn array_length(&self, args: Vec<serde_json::Value>) -> Result<JValue, JError> {
        match &args[..] {
            [JValue::Array(array)] => Ok(json!(array.len())),
            [_] => Err(JError::new("op array_length's argument must be an array")),
            arr => Err(JError::new(format!(
                "op array_length accepts exactly 1 argument: {} found",
                arr.len()
            ))),
        }
    }

    /// takes a range of values from an array
    /// slice(array: []JValue, start: usize, end: usize) -> []JValue
    fn array_slice(&self, args: Vec<serde_json::Value>) -> Result<JValue, JError> {
        let (array, start, end) = if let [array, start, end] = &args[..] {
            (array, start, end)
        } else {
            return Err(JError::new(
                "invalid number of parameters. need array, start index and end index",
            ));
        };

        let array = match array {
            JValue::Array(arr) if arr.is_empty() => return Ok(json!([])),
            JValue::Array(arr) => arr,
            e => {
                return Err(JError::new(format!(
                    "first argument must be an array, was {e}"
                )));
            }
        };

        let start = match start.as_u64() {
            Some(n) => n as usize,
            _ => {
                return Err(JError::new(format!(
                    "second argument (start index) must be an unsigned integer, was {start}"
                )));
            }
        };

        let end = match end.as_u64() {
            Some(n) => n as usize,
            _ => {
                return Err(JError::new(format!(
                    "third argument (end index) must be an unsigned integer, was {end}"
                )));
            }
        };

        if start > end || end > array.len() {
            return Err(JError::new(format!(
                "slice indexes out of bounds. start index: {:?}, end index: {:?}, array length: {:?}", 
                start, end, array.len())
            ));
        }

        let slice: Vec<JValue> = array[start..end].to_vec();
        Ok(JValue::Array(slice))
    }

    fn add_module(&self, args: Args) -> Result<JValue, JError> {
        let mut args = args.function_args.into_iter();
        let module_bytes: String = Args::next("module_bytes", &mut args)?;
        let config = Args::next("config", &mut args)?;

        let module_hash = self.modules.add_module_base64(module_bytes, config)?;

        Ok(JValue::String(module_hash))
    }

    fn add_module_from_vault(&self, args: Args, params: ParticleParams) -> Result<JValue, JError> {
        let mut args = args.function_args.into_iter();
        let module_path: String = Args::next("module_path", &mut args)?;
        let config = Args::next("config", &mut args)?;

        let module_hash = self
            .modules
            .add_module_from_vault(module_path, config, params)?;

        Ok(JValue::String(module_hash))
    }

    fn add_blueprint(&self, args: Args) -> Result<JValue, JError> {
        let mut args = args.function_args.into_iter();
        let blueprint_request: AddBlueprint = Args::next("blueprint_request", &mut args)?;

        let blueprint_id = self.modules.add_blueprint(blueprint_request)?;
        Ok(JValue::String(blueprint_id))
    }

    fn load_module_config_from_vault(
        &self,
        args: Args,
        params: ParticleParams,
    ) -> Result<JValue, JError> {
        let mut args = args.function_args.into_iter();
        let config_path: String = Args::next("config_path", &mut args)?;

        let config = self
            .modules
            .load_module_config_from_vault(config_path, params)?;
        let config = serde_json::to_value(config)
            .map_err(|err| JError::new(format!("Error serializing config to JSON: {err}")))?;

        Ok(config)
    }

    fn default_module_config(&self, args: Args) -> Result<JValue, JError> {
        let mut args = args.function_args.into_iter();
        let module_name: String = Args::next("module_name", &mut args)?;

        let config = NamedModuleConfig {
            name: module_name,
            load_from: None,
            file_name: None,
            config: <_>::default(),
        };
        let config = serde_json::to_value(config)
            .map_err(|err| JError::new(format!("Error serializing config to JSON: {err}")))?;

        Ok(config)
    }

    fn make_blueprint(&self, args: Args) -> Result<JValue, JError> {
        let mut args = args.function_args.into_iter();
        let name = Args::next("name", &mut args)?;
        let dependencies = Args::next("dependencies", &mut args)?;
        let blueprint_request = AddBlueprint { name, dependencies };

        let blueprint_request = serde_json::to_value(blueprint_request).map_err(|err| {
            JError::new(format!(
                "Error serializing blueprint_request to JSON: {err}"
            ))
        })?;
        Ok(blueprint_request)
    }

    fn load_blueprint_from_vault(
        &self,
        args: Args,
        params: ParticleParams,
    ) -> Result<JValue, JError> {
        let mut args = args.function_args.into_iter();
        let blueprint_path = Args::next("blueprint_path", &mut args)?;

        let blueprint_request = self
            .modules
            .load_blueprint_from_vault(blueprint_path, params)?;

        let blueprint_request = serde_json::to_value(blueprint_request).map_err(|err| {
            JError::new(format!(
                "Error serializing blueprint_request to JSON: {err}"
            ))
        })?;
        Ok(blueprint_request)
    }

    fn list_modules(&self) -> Result<JValue, JError> {
        self.modules.list_modules()
    }

    fn get_module_interface(&self, args: Args) -> Result<JValue, JError> {
        let mut args = args.function_args.into_iter();
        let hash: String = Args::next("hex_hash", &mut args)?;
        self.modules.get_interface(&hash)
    }

    fn get_blueprints(&self) -> Result<JValue, JError> {
        self.modules
            .get_blueprints()
            .into_iter()
            .map(|bp| {
                serde_json::to_value(&bp).map_err(|err| {
                    JError::new(format!("error serializing blueprint {bp:?}: {err}"))
                })
            })
            .collect()
    }

    fn create_service(&self, args: Args, params: ParticleParams) -> Result<JValue, JError> {
        let mut args = args.function_args.into_iter();
        let blueprint_id: String = Args::next("blueprint_id", &mut args)?;

        let service_id = self
            .services
            .create_service(blueprint_id, params.init_peer_id)?;

        Ok(JValue::String(service_id))
    }

    fn remove_service(&self, args: Args, params: ParticleParams) -> Result<(), JError> {
        let mut args = args.function_args.into_iter();
        let service_id_or_alias: String = Args::next("service_id_or_alias", &mut args)?;
        self.services
            .remove_service(service_id_or_alias, params.init_peer_id, false)?;
        Ok(())
    }

    fn list_services(&self) -> JValue {
        JValue::Array(self.services.list_services())
    }

    fn call_service(&self, function_args: Args, particle: ParticleParams) -> FunctionOutcome {
        self.services.call_service(function_args, particle)
    }

    fn get_interface(&self, args: Args) -> Result<JValue, JError> {
        let mut args = args.function_args.into_iter();
        let service_id: String = Args::next("service_id", &mut args)?;
        Ok(self.services.get_interface(service_id)?)
    }

    fn add_alias(&self, args: Args, params: ParticleParams) -> Result<(), JError> {
        let mut args = args.function_args.into_iter();

        let alias: String = Args::next("alias", &mut args)?;
        let service_id: String = Args::next("service_id", &mut args)?;
        self.services
            .add_alias(alias, service_id, params.init_peer_id)?;
        Ok(())
    }

    fn resolve_alias(&self, args: Args) -> Result<JValue, JError> {
        let mut args = args.function_args.into_iter();

        let alias: String = Args::next("alias", &mut args)?;
        let service_id = self.services.resolve_alias(alias)?;

        Ok(JValue::String(service_id))
    }

    fn kademlia(&self) -> &KademliaApi {
        self.connectivity.as_ref()
    }

    fn connection_pool(&self) -> &ConnectionPoolApi {
        self.connectivity.as_ref()
    }

    fn service_stat(&self, args: Args) -> Result<JValue, JError> {
        let mut args = args.function_args.into_iter();
        let service_id_or_alias: String = Args::next("service_id", &mut args)?;
        // Resolve aliases; also checks that the requested service exists.
        let service_id = self.services.to_service_id(service_id_or_alias)?;
        let metrics = self
            .services
            .metrics
            .as_ref()
            .ok_or_else(|| JError::new("Service stats collection is disabled"))?;
        if let Some(result) = metrics.builtin.read(&service_id) {
            Ok(json!({
                "status": true,
                "error": "",
                "result": vec![result],
            }))
        } else {
            Ok(json!({
                "status": false,
                "error": format!("No stats were collected for the `{service_id}` service"),
                "result": [],
            }))
        }
    }

    fn sign(&self, args: Args) -> Result<JValue, JError> {
        let tetraplets = args.tetraplets;
        let mut args = args.function_args.into_iter();
        let result: Result<JValue, JError> = try {
            let data: Vec<u8> = Args::next("data", &mut args)?;

            let tetraplet = tetraplets.get(0).map(|v| v.as_slice());
            if let Some([t]) = tetraplet {
                if t.peer_pk != self.local_peer_id.to_base58() {
                    return Err(JError::new(format!(
                        "data is expected to be produced by service 'registry' on peer '{}', was from peer '{}'",
                        self.local_peer_id, t.peer_pk
                    )));
                }

                if (t.service_id.as_str(), t.function_name.as_str())
                    != ("registry", "get_record_bytes")
                {
                    return Err(JError::new(format!(
                        "data is expected to result from a call to 'registry.get_record_bytes', was from '{}.{}'",
                        t.service_id, t.function_name
                    )));
                }

                if !t.json_path.is_empty() {
                    return Err(JError::new(
                        "json_path for data tetraplet is expected to be empty",
                    ));
                }
            } else {
                return Err(JError::new(format!("expected tetraplet for a scalar argument, got tetraplet for an array: {tetraplet:?}, tetraplets")));
            }

            json!(self.root_keypair.sign(&data)?.to_vec())
        };

        match result {
            Ok(sig) => Ok(json!({
                "success": true,
                "error": [],
                "signature": vec![sig]
            })),

            Err(error) => Ok(json!({
                "success": false,
                "error": vec![JValue::from(error)],
                "signature": []
            })),
        }
    }

    fn verify(&self, args: Args) -> Result<JValue, JError> {
        let mut args = args.function_args.into_iter();
        let signature: Vec<u8> = Args::next("signature", &mut args)?;
        let data: Vec<u8> = Args::next("data", &mut args)?;
        let signature =
            Signature::from_bytes(self.root_keypair.public().get_key_format(), signature);

        Ok(JValue::Bool(
            self.root_keypair.public().verify(&data, &signature).is_ok(),
        ))
    }

    fn get_peer_id(&self) -> Result<JValue, JError> {
        Ok(JValue::String(self.root_keypair.get_peer_id().to_base58()))
    }
}

fn make_module_config(args: Args) -> Result<JValue, JError> {
    use toml_utils::table;

    let mut args = args.function_args.into_iter();

    let name = Args::next("name", &mut args)?;
    let mem_pages_count = Args::next_opt("mem_pages_count", &mut args)?;
    let max_heap_size: Option<String> = Args::next_opt("max_heap_size", &mut args)?;
    let max_heap_size = match max_heap_size {
        Some(s) => Some(bytesize::ByteSize::from_str(&s).map_err(|err| {
            JError::new(format!(
                "error parsing max_heap_size from String to ByteSize: {err}"
            ))
        })?),
        None => None,
    };
    let logger_enabled = Args::next_opt("logger_enabled", &mut args)?;
    let preopened_files = Args::next_opt("preopened_files", &mut args)?;
    let envs = Args::next_opt("envs", &mut args)?.map(table);
    let mapped_dirs = Args::next_opt("mapped_dirs", &mut args)?.map(table);
    let mounted_binaries = Args::next_opt("mounted_binaries", &mut args)?.map(table);
    let logging_mask = Args::next_opt("logging_mask", &mut args)?;

    let config = NamedModuleConfig {
        name,
        load_from: None,
        file_name: None,
        config: ModuleConfig {
            mem_pages_count,
            max_heap_size,
            logger_enabled,
            wasi: Some(WASIConfig {
                preopened_files,
                envs,
                mapped_dirs,
            }),
            mounted_binaries,
            logging_mask,
        },
    };

    let config = serde_json::to_value(config)
        .map_err(|err| JError::new(format!("Error serializing config to JSON: {err}")))?;

    Ok(config)
}

fn parse_from_str<T>(
    field: &'static str,
    mut args: &mut impl Iterator<Item = JValue>,
) -> Result<Option<T>, JError>
where
    T: FromStr + for<'a> Deserialize<'a>,
    <T as FromStr>::Err: std::error::Error + 'static,
{
    #[derive(thiserror::Error, Debug)]
    #[error("Error while deserializing field {field_name}")]
    struct Error<E: std::error::Error> {
        field_name: String,
        #[source]
        err: E,
    }

    #[derive(serde::Deserialize, Debug)]
    #[serde(untagged)]
    pub enum Either<T> {
        String(String),
        Target(T),
    }

    let number: Option<Either<T>> = Args::next_opt(field, &mut args)?;

    if number.is_none() {
        return Ok(None);
    }

    number
        .map(|i| match i {
            Either::String(s) => Ok(s.parse::<T>()?),
            Either::Target(n) => Ok(n),
        })
        .transpose()
        .map_err(|err: T::Err| {
            Error {
                field_name: field.to_string(),
                err,
            }
            .into()
        })
}

fn get_delay(delay: Option<Duration>, interval: Option<Duration>) -> Duration {
    use rand::prelude::*;
    let mut rng = rand::thread_rng();
    match (delay, interval) {
        (Some(delay), _) => delay,
        (None, Some(interval)) => Duration::from_secs(rng.gen_range(0..=interval.as_secs())),
        (None, None) => Duration::from_secs(0),
    }
}

#[derive(thiserror::Error, Debug)]
enum ResolveVaultError {
    #[error("Incorrect vault path `{1}`: doesn't belong to vault (`{2}`)")]
    WrongVault(
        #[source] Option<path::StripPrefixError>,
        path::PathBuf,
        path::PathBuf,
    ),
    #[error("Incorrect vault  path `{1}`: doesn't exist")]
    NotFound(#[source] std::io::Error, path::PathBuf),
}

/// Map the given virtual path to the real one from the file system of the node.
fn resolve_vault_path(
    particles_vault_dir: &path::Path,
    path: &path::Path,
    particle_id: &str,
) -> Result<path::PathBuf, ResolveVaultError> {
    let virtual_prefix = path::Path::new(VIRTUAL_PARTICLE_VAULT_PREFIX).join(particle_id);
    let real_prefix = particles_vault_dir.join(particle_id);

    let rest = path.strip_prefix(&virtual_prefix).map_err(|e| {
        ResolveVaultError::WrongVault(Some(e), path.to_path_buf(), virtual_prefix.clone())
    })?;
    let real_path = real_prefix.join(rest);
    let resolved_path = real_path
        .canonicalize()
        .map_err(|e| ResolveVaultError::NotFound(e, path.to_path_buf()))?;
    // Check again after normalization that the path leads to the real particle vault
    if resolved_path.starts_with(&real_prefix) {
        Ok(resolved_path)
    } else {
        Err(ResolveVaultError::WrongVault(
            None,
            resolved_path,
            real_prefix,
        ))
    }
}

#[cfg(test)]
mod prop_tests {
    use std::str::FromStr;

    use prop::collection::vec;
    use proptest::arbitrary::StrategyFor;
    use proptest::collection::{SizeRange, VecStrategy};
    use proptest::prelude::*;
    use serde_json::json;

    use particle_args::Args;

    use crate::builtins::make_module_config;

    prop_compose! {
      /// Generates ByteSize strings
      fn heap_size
        ()
        // FIXME: limit is 100k GB because ByteSize can't handle exabytes. terabytes and petabytes are missing for the same reason.
        (n in prop::option::of(0..100_000), si in "(?i)([kmg]i)?B")
        -> Vec<String>
      {
        n.map(|n| vec![format!("{n} {si}")]).unwrap_or_default()
      }
    }

    /// Wraps value into AIR-style option (vec of length 0 or 1)
    pub fn air_opt<T: Strategy>(element: T) -> proptest::collection::VecStrategy<T> {
        vec(element, 0..1)
    }

    /// Generates an associative array of strings of a given size
    pub fn assoc_vec(size: impl Into<SizeRange>) -> VecStrategy<VecStrategy<StrategyFor<String>>> {
        vec(vec(any::<String>(), 2..=2), size)
    }

    proptest! {
        #[test]
        fn module_config(
            name in any::<String>(),
            mem_pages in air_opt(any::<u32>()),
            logger_enabled in air_opt(proptest::bool::ANY),
            heap in heap_size(),
            preopened_files in air_opt(any::<String>()),
            envs in air_opt(assoc_vec(0..10)),
            mapped_dirs in air_opt(assoc_vec(0..10)),
            mounted_binaries in air_opt(assoc_vec(0..10)),
            logging_mask in air_opt(any::<i32>()),
        ) {
            let mem_pages: Vec<u32> = mem_pages;
            let heap: Vec<String> = heap;
            let preopened_files: Vec<String> = preopened_files;
            let envs: Vec<Vec<Vec<String>>> = envs;
            let mapped_dirs: Vec<Vec<Vec<String>>> = mapped_dirs;
            let mounted_binaries: Vec<Vec<Vec<String>>> = mounted_binaries;
            let logging_mask: Vec<i32> = logging_mask;

            let args = vec![
                json!(name),              // required: name
                json!(mem_pages),         // mem_pages_count = optional: None
                json!(heap),      // optional: max_heap_size
                json!(logger_enabled),    // optional: logger_enabled
                json!(preopened_files),   // optional: preopened_files
                json!(envs),              // optional: envs
                json!(mapped_dirs),       // optional: mapped_dirs
                json!(mounted_binaries),  // optional: mounted_binaries
                json!(logging_mask),      // optional: logging_mask
            ];
            let args = Args {
                service_id: "".to_string(),
                function_name: "".to_string(),
                function_args: args,
                tetraplets: vec![],
            };

            let config = make_module_config(args).expect("parse config via make_module_config");
            let prop_heap = heap.get(0).map(|h| bytesize::ByteSize::from_str(h).unwrap().to_string());
            let config_heap = config.get("max_heap_size").map(|h| bytesize::ByteSize::from_str(h.as_str().unwrap()).unwrap().to_string());
            prop_assert_eq!(prop_heap, config_heap);
        }
    }
}

#[cfg(test)]
mod resolve_path_tests {
    use std::fs::File;
    use std::path::Path;

    use particle_services::VIRTUAL_PARTICLE_VAULT_PREFIX;

    use crate::builtins::{resolve_vault_path, ResolveVaultError};

    fn with_env(callback: fn(&str, &Path, &str, &Path) -> ()) {
        let particle_id = "particle_id";
        let dir = tempfile::tempdir().expect("can't create temp dir");
        let real_vault_prefix = dir.path().canonicalize().expect("").join("vault");
        let real_vault_dir = real_vault_prefix.join(particle_id);
        std::fs::create_dir_all(&real_vault_dir).expect("can't create dirs");

        let filename = "file";
        let real_path = real_vault_dir.join(filename);
        File::create(&real_path).expect("can't create a file");

        callback(
            particle_id,
            real_vault_prefix.as_path(),
            filename,
            real_path.as_path(),
        );

        dir.close().ok();
    }

    #[test]
    fn test_resolve_path_ok() {
        with_env(|particle_id, real_prefix, filename, path| {
            let virtual_path = Path::new(VIRTUAL_PARTICLE_VAULT_PREFIX)
                .join(particle_id)
                .join(filename);
            let result = resolve_vault_path(real_prefix, &virtual_path, particle_id).unwrap();
            assert_eq!(result, path);
        });
    }

    #[test]
    fn test_resolve_path_wrong_vault() {
        with_env(|particle_id, real_prefix, filename, _path| {
            let virtual_path = Path::new(VIRTUAL_PARTICLE_VAULT_PREFIX)
                .join("other-particle-id")
                .join(filename);
            let result = resolve_vault_path(real_prefix, &virtual_path, particle_id);
            assert!(result.is_err());
            assert!(matches!(
                result.unwrap_err(),
                ResolveVaultError::WrongVault(_, _, _)
            ));
        });
    }

    #[test]
    fn test_resolve_path_not_found() {
        with_env(|particle_id, real_prefix, _filename, _path| {
            let virtual_path = Path::new(VIRTUAL_PARTICLE_VAULT_PREFIX)
                .join(particle_id)
                .join("other-file");
            let result = resolve_vault_path(real_prefix, &virtual_path, particle_id);
            assert!(result.is_err());
            assert!(matches!(
                result.unwrap_err(),
                ResolveVaultError::NotFound(_, _)
            ));
        });
    }
}
