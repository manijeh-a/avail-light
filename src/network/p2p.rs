use allow_block_list::BlockedPeers;
use color_eyre::{eyre::WrapErr, Report, Result};
use kad_mem_store::{MemoryStore, MemoryStoreConfig};
use libp2p::{
	autonat, dcutr, identify, identity,
	kad::{self, PeerRecord, QueryId},
	mdns, noise, ping, relay,
	swarm::NetworkBehaviour,
	tcp, upnp, yamux, PeerId, StreamProtocol, Swarm, SwarmBuilder,
};
use multihash::{self, Hasher};
use std::collections::HashMap;
use tokio::sync::{
	mpsc::{self},
	oneshot,
};
use tracing::info;

#[cfg(feature = "network-analysis")]
pub mod analyzer;
mod client;
mod event_loop;
mod kad_mem_store;

use crate::{
	shutdown::Controller,
	types::{KademliaMode, LibP2PConfig, SecretKey},
};
pub use client::Client;
use event_loop::EventLoop;

use self::client::BlockStat;
use libp2p_allow_block_list as allow_block_list;

#[derive(Debug)]
pub enum QueryChannel {
	GetRecord(oneshot::Sender<Result<PeerRecord>>),
	PutRecord,
	Bootstrap(oneshot::Sender<Result<()>>),
}

pub struct EventLoopEntries<'a> {
	swarm: &'a mut Swarm<Behaviour>,
	pending_kad_queries: &'a mut HashMap<QueryId, QueryChannel>,
	pending_swarm_events: &'a mut HashMap<PeerId, oneshot::Sender<Result<()>>>,
	/// <block_num, (total_cells, result_cell_counter, time_stat)>
	active_blocks: &'a mut HashMap<u32, BlockStat>,
}

impl<'a> EventLoopEntries<'a> {
	pub fn new(
		swarm: &'a mut Swarm<Behaviour>,
		pending_kad_queries: &'a mut HashMap<QueryId, QueryChannel>,
		pending_swarm_events: &'a mut HashMap<PeerId, oneshot::Sender<Result<()>>>,
		active_blocks: &'a mut HashMap<u32, BlockStat>,
	) -> Self {
		Self {
			swarm,
			pending_kad_queries,
			pending_swarm_events,
			active_blocks,
		}
	}

	pub fn insert_query(&mut self, query_id: QueryId, result_sender: QueryChannel) {
		self.pending_kad_queries.insert(query_id, result_sender);
	}

	pub fn insert_swarm_event(
		&mut self,
		peer_id: PeerId,
		result_sender: oneshot::Sender<Result<()>>,
	) {
		self.pending_swarm_events.insert(peer_id, result_sender);
	}

	pub fn behavior_mut(&mut self) -> &mut Behaviour {
		self.swarm.behaviour_mut()
	}

	pub fn swarm(&mut self) -> &mut Swarm<Behaviour> {
		self.swarm
	}
}

pub trait Command {
	fn run(&mut self, entries: EventLoopEntries) -> Result<(), Report>;
	fn abort(&mut self, error: Report);
}

type SendableCommand = Box<dyn Command + Send + Sync>;
type CommandSender = mpsc::Sender<SendableCommand>;
type CommandReceiver = mpsc::Receiver<SendableCommand>;

// Behaviour struct is used to derive delegated Libp2p behaviour implementation
#[derive(NetworkBehaviour)]
#[behaviour(event_process = false)]
pub struct Behaviour {
	kademlia: kad::Behaviour<MemoryStore>,
	identify: identify::Behaviour,
	ping: ping::Behaviour,
	mdns: mdns::tokio::Behaviour,
	auto_nat: autonat::Behaviour,
	relay_client: relay::client::Behaviour,
	dcutr: dcutr::Behaviour,
	upnp: upnp::tokio::Behaviour,
	blocked_peers: allow_block_list::Behaviour<BlockedPeers>,
}

// Init function initializes all needed needed configs for the functioning
// p2p network Client and network Event Loop
// If in fat client mode, we enable deleting local Kademlia records as a memory optimization
pub fn init(
	cfg: LibP2PConfig,
	dht_parallelization_limit: usize,
	ttl: u64,
	is_fat_client: bool,
	id_keys: libp2p::identity::Keypair,
	shutdown: Controller<String>,
) -> Result<(Client, EventLoop)> {
	let local_peer_id = PeerId::from(id_keys.public());
	info!(
		"Local peer id: {:?}. Public key: {:?}.",
		local_peer_id,
		id_keys.public()
	);

	// Use identify protocol_version as Kademlia protocol name
	let kademlia_protocol_name =
		StreamProtocol::try_from_owned(cfg.identify.protocol_version.clone())
			.expect("Invalid Kademlia protocol name");

	// build the Swarm, connecting the lower transport logic with the
	// higher layer network behaviour logic
	let mut swarm = SwarmBuilder::with_existing_identity(id_keys)
		.with_tokio()
		.with_tcp(
			tcp::Config::default().port_reuse(true).nodelay(true),
			noise::Config::new,
			yamux::Config::default,
		)?
		.with_quic()
		.with_dns()?
		.with_relay_client(noise::Config::new, yamux::Config::default)?
		.with_behaviour(|key, relay_client| {
			// configure Kademlia Memory Store
			let kad_store = MemoryStore::with_config(
				key.public().to_peer_id(),
				MemoryStoreConfig {
					max_records: cfg.kademlia.max_kad_record_number, // ~2hrs
					max_value_bytes: cfg.kademlia.max_kad_record_size + 1,
					max_providers_per_key: usize::from(cfg.kademlia.record_replication_factor), // Needs to match the replication factor, per libp2p docs
					max_provided_keys: cfg.kademlia.max_kad_provided_keys,
				},
			);
			// create Kademlia Config
			let mut kad_cfg = kad::Config::default();
			kad_cfg
				.set_publication_interval(cfg.kademlia.publication_interval)
				.set_replication_interval(cfg.kademlia.record_replication_interval)
				.set_replication_factor(cfg.kademlia.record_replication_factor)
				.set_query_timeout(cfg.kademlia.query_timeout)
				.set_parallelism(cfg.kademlia.query_parallelism)
				.set_caching(kad::Caching::Enabled {
					max_peers: cfg.kademlia.caching_max_peers,
				})
				.disjoint_query_paths(cfg.kademlia.disjoint_query_paths)
				.set_record_filtering(kad::StoreInserts::FilterBoth)
				.set_protocol_names(vec![kademlia_protocol_name]);

			// create Identify Protocol Config
			let identify_cfg =
				identify::Config::new(cfg.identify.protocol_version.clone(), key.public())
					.with_agent_version(cfg.identify.agent_version.to_string());

			// create AutoNAT Client Config
			let autonat_cfg = autonat::Config {
				retry_interval: cfg.autonat.retry_interval,
				refresh_interval: cfg.autonat.refresh_interval,
				boot_delay: cfg.autonat.boot_delay,
				throttle_server_period: cfg.autonat.throttle_server_period,
				only_global_ips: cfg.autonat.only_global_ips,
				..Default::default()
			};

			Ok(Behaviour {
				ping: ping::Behaviour::new(ping::Config::new()),
				identify: identify::Behaviour::new(identify_cfg),
				relay_client,
				dcutr: dcutr::Behaviour::new(key.public().to_peer_id()),
				kademlia: kad::Behaviour::with_config(
					key.public().to_peer_id(),
					kad_store,
					kad_cfg,
				),
				auto_nat: autonat::Behaviour::new(key.public().to_peer_id(), autonat_cfg),
				mdns: mdns::Behaviour::new(mdns::Config::default(), key.public().to_peer_id())?,
				upnp: upnp::tokio::Behaviour::default(),
				blocked_peers: allow_block_list::Behaviour::default(),
			})
		})?
		.with_swarm_config(|c| c.with_idle_connection_timeout(cfg.connection_idle_timeout))
		.build();

	let kad_mode = if is_fat_client {
		KademliaMode::Server.into()
	} else {
		cfg.kademlia_mode.into()
	};

	// Setting the mode this way disables automatic mode changes.
	//
	// Because the identify protocol doesn't allow us to change
	// agent data on the fly, we're forced to use static Kad modes
	// instead of relying on dynamic changes
	swarm.behaviour_mut().kademlia.set_mode(Some(kad_mode));

	// create sender channel for Event Loop Commands
	let (command_sender, command_receiver) = mpsc::channel(10000);

	Ok((
		Client::new(command_sender, dht_parallelization_limit, ttl),
		EventLoop::new(
			swarm,
			command_receiver,
			cfg.relays,
			cfg.bootstrap_interval,
			is_fat_client,
			shutdown,
			cfg.identify,
		),
	))
}

// Keypair function creates identity Keypair for a local node.
// From such generated keypair it derives multihash identifier of the local peer.
pub fn keypair(cfg: LibP2PConfig) -> Result<(libp2p::identity::Keypair, String)> {
	let keypair = match cfg.secret_key {
		// If seed is provided, generate secret key from seed
		Some(SecretKey::Seed { seed }) => {
			let seed_digest = multihash::Sha3_256::digest(seed.as_bytes());
			identity::Keypair::ed25519_from_bytes(seed_digest)
				.wrap_err("error generating secret key from seed")?
		},
		// Import secret key if provided
		Some(SecretKey::Key { key }) => {
			let mut decoded_key = [0u8; 32];
			hex::decode_to_slice(key.into_bytes(), &mut decoded_key)
				.wrap_err("error decoding secret key from config")?;
			identity::Keypair::ed25519_from_bytes(decoded_key)
				.wrap_err("error importing secret key")?
		},
		// If neither seed nor secret key provided, generate secret key from random seed
		None => identity::Keypair::generate_ed25519(),
	};
	let peer_id = PeerId::from(keypair.public()).to_string();
	Ok((keypair, peer_id))
}
