use std::error::Error;
use std::fs::{self, File};
use std::io::BufReader;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{
    fs::exists,
    sync::{Arc, Mutex},
};

use base64::{Engine, prelude::BASE64_STANDARD};
use bdk_bitcoind_rpc::bitcoincore_rpc::{Auth, Client};
use bdk_bitcoind_rpc::{Emitter, NO_EXPECTED_MEMPOOL_TXS};
use bitcoin::hashes::Hash;
use bitcoin::key::rand::rngs::OsRng;
use bitcoin::secp256k1::PublicKey;
use bitcoin::secp256k1::rand::RngCore;
use bitcoin::{Address, Amount, BlockHash, OutPoint};
use lightning::bolt11_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, Description};
use lightning::chain::{ChannelMonitorUpdateStatus, Listen, Watch};
use lightning::events::bump_transaction::sync::BumpTransactionEventHandlerSync;
use lightning::io::Cursor;
use lightning::ln::channel_state::ChannelDetails;
use lightning::ln::channelmanager::{
    Bolt11InvoiceParameters, ChannelManagerReadArgs, PaymentId, Retry,
};
use lightning::ln::msgs::SocketAddress;
use lightning::ln::peer_handler::{MessageHandler, PeerDetails};
use lightning::ln::types::ChannelId;
use lightning::routing::router::RouteParametersConfig;
use lightning::sign::{KeysManager, NodeSigner, Recipient};
use lightning::types::payment::PaymentHash;
use lightning::util::errors::APIError;
use lightning::util::persist::{
    KVStoreSync, SCORER_PERSISTENCE_KEY, SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
    SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
};
use lightning::util::persist::{
    NETWORK_GRAPH_PERSISTENCE_KEY, NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
    NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE, read_channel_monitors,
};
use lightning::util::ser::ReadableArgs;
use lightning::{
    chain::{self, BestBlock, Filter},
    ln::{
        self,
        channelmanager::{self, ChainParameters},
        peer_handler::IgnoringMessageHandler,
    },
    onion_message::messenger::{self, DefaultMessageRouter},
    routing::{
        gossip,
        router::DefaultRouter,
        scoring::{
            ProbabilisticScorer, ProbabilisticScoringDecayParameters,
            ProbabilisticScoringFeeParameters,
        },
    },
    sign::InMemorySigner,
    util::config::UserConfig,
};
use lightning_background_processor::{
    GossipSync, NO_LIQUIDITY_MANAGER, process_events_async_with_kv_store_sync,
};
use lightning_block_sync::poll::ChainPoller;
use lightning_block_sync::{SpvClient, UnboundedCache, init};
use lightning_block_sync::{gossip::TokioSpawner, http::HttpEndpoint};
use lightning_net_tokio::SocketDescriptor as LdkTokioSocketDescriptor;
use lightning_persister::fs_store::FilesystemStore;
use rand::Rng;
use serde::{Serialize, Serializer};
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;

use crate::bitcoind_client::ChainListener;
use crate::event_handler::LdkEventHandler;
use crate::storage::{Payment, PaymentDirection, PaymentStatus, PeerInfo};
use crate::{
    bitcoind_client::BitcoindRpcClient, config::Config, logger::NodeLogger,
    onchain_wallet::WalletManager, storage::NodeStore,
};

type NetworkGraph = gossip::NetworkGraph<Arc<NodeLogger>>;

// Implements UtxoLookup trait
type GossipVerifier = lightning_block_sync::gossip::GossipVerifier<
    TokioSpawner,
    Arc<BitcoindRpcClient>,
    Arc<NodeLogger>,
>;

type P2PGossipSync = gossip::P2PGossipSync<Arc<NetworkGraph>, Arc<GossipVerifier>, Arc<NodeLogger>>;

// Scorer for the DefaultRouter. Impls ScoreLookUp trait
type Scorer = ProbabilisticScorer<Arc<NetworkGraph>, Arc<NodeLogger>>;

type Router = DefaultRouter<
    Arc<NetworkGraph>,
    Arc<NodeLogger>,
    Arc<WalletManager>,
    Arc<Mutex<Scorer>>,
    ProbabilisticScoringFeeParameters,
    Scorer,
>;

// MessageRouter impl for the ChannelManager
type OnionMessageRouter =
    DefaultMessageRouter<Arc<NetworkGraph>, Arc<NodeLogger>, Arc<WalletManager>>;

pub(crate) type ChainMonitor = chain::chainmonitor::ChainMonitor<
    InMemorySigner,
    Arc<dyn Filter + Send + Sync>,
    // BroadcasterInterface impl
    Arc<BitcoindRpcClient>,
    // FeeEstimator impl
    Arc<BitcoindRpcClient>,
    Arc<NodeLogger>,
    Arc<FilesystemStore>,
    Arc<WalletManager>,
>;

pub(crate) type ChannelManager = channelmanager::ChannelManager<
    Arc<ChainMonitor>,
    // BroadcasterInterface impl
    Arc<BitcoindRpcClient>,
    // EntropySource, NodeSigner, SignerProvider impls
    Arc<WalletManager>,
    Arc<WalletManager>,
    Arc<WalletManager>,
    // FeeEstimator impl
    Arc<BitcoindRpcClient>,
    Arc<Router>,
    Arc<OnionMessageRouter>,
    Arc<NodeLogger>,
>;

type OnionMessenger = messenger::OnionMessenger<
    // EntropySource and NodeSigner impls
    Arc<WalletManager>,
    Arc<WalletManager>,
    Arc<NodeLogger>,
    // NodeIdLookUp impl
    Arc<ChannelManager>,
    Arc<OnionMessageRouter>,
    // OffersMessageHandler impl
    Arc<ChannelManager>,
    // AsyncPaymentsMessageHandler, DNSResolverMessageHandler and CustomOnionMessageHandler
    IgnoringMessageHandler,
    IgnoringMessageHandler,
    IgnoringMessageHandler,
>;

type PeerManager = ln::peer_handler::PeerManager<
    // SocketDescriptor impl from lightning-net-tokio crate
    LdkTokioSocketDescriptor,
    // ChannelMessageHandler is impl'd by the ChannelManager
    Arc<ChannelManager>,
    // RoutingMessageHandler impl
    Arc<P2PGossipSync>,
    // OnionMessageHandler impl
    Arc<OnionMessenger>,
    Arc<NodeLogger>,
    IgnoringMessageHandler,
    Arc<WalletManager>,
    Arc<ChainMonitor>,
>;

// Types to handle LDK `BumpTransactionEvent` events
type LdkWallet =
    lightning::events::bump_transaction::sync::WalletSync<Arc<WalletManager>, Arc<NodeLogger>>;

pub(crate) type BumpTxEventHandler = BumpTransactionEventHandlerSync<
    Arc<BitcoindRpcClient>,
    Arc<LdkWallet>,
    Arc<WalletManager>,
    Arc<NodeLogger>,
>;

type OutputSweeper = lightning::util::sweep::OutputSweeperSync<
    Arc<BitcoindRpcClient>,
    Arc<WalletManager>,
    Arc<BitcoindRpcClient>,
    Arc<dyn chain::Filter + Send + Sync>,
    Arc<FilesystemStore>,
    Arc<NodeLogger>,
    Arc<KeysManager>,
>;

#[derive(Debug, Serialize)]
pub struct ChannelInfo {
    #[serde(serialize_with = "serialize_channel_id")]
    pub channel_id: ChannelId,
    pub counterparty: PublicKey,
    pub funding_txo: Option<OutPoint>,
    pub short_channel_id: Option<u64>,
    pub channel_value_sat: u64,
    pub local_reserve: Option<u64>,
    pub outbound_capacity_sat: u64,
    pub inbound_capacity_sat: u64,
    pub confirmations_required: Option<u32>,
    pub is_outbound: bool,
    pub is_channel_ready: bool,
    pub is_usable: bool,
    pub is_announced: bool,
}

impl From<&ChannelDetails> for ChannelInfo {
    fn from(value: &ChannelDetails) -> Self {
        Self {
            channel_id: value.channel_id,
            counterparty: value.counterparty.node_id,
            funding_txo: value
                .funding_txo
                .and_then(|tx| Some(tx.into_bitcoin_outpoint())),
            short_channel_id: value.short_channel_id,
            channel_value_sat: value.channel_value_satoshis,
            local_reserve: value.unspendable_punishment_reserve,
            outbound_capacity_sat: value.outbound_capacity_msat / 1000,
            inbound_capacity_sat: value.inbound_capacity_msat / 1000,
            confirmations_required: value.confirmations_required,
            is_outbound: value.is_outbound,
            is_channel_ready: value.is_channel_ready,
            is_usable: value.is_usable,
            is_announced: value.is_announced,
        }
    }
}

fn serialize_channel_id<S>(channel_id: &ChannelId, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&hex::encode(channel_id.0))
}

pub struct Balance {
    pub onchain_confirmed: Amount,
    pub onchain_pending: Amount,
    pub lightning: Amount,
}

pub struct NodeBuilder {
    config: Config,
    ldk_config: UserConfig,
}

impl NodeBuilder {
    pub fn new(config: Config) -> Self {
        let mut ldk_config = UserConfig::default();

        ldk_config.channel_handshake_limits.min_funding_satoshis = config.min_channel_value_sat;
        ldk_config.channel_handshake_config.announce_for_forwarding = config.default_channel_public;
        // Setting to use anchor channels also requires having to manually accept inbound channels
        // to allows to check if we have enough onchain funds for a new channel.
        ldk_config
            .channel_handshake_config
            .negotiate_anchors_zero_fee_htlc_tx = true;
        ldk_config.manually_accept_inbound_channels = true;

        ldk_config.channel_handshake_config.our_max_accepted_htlcs = config.max_pending_htlcs;
        ldk_config.accept_forwards_to_priv_channels = true;

        Self { config, ldk_config }
    }

    pub fn with_manually_accept_inbound_channels(mut self, manual: bool) -> Self {
        self.ldk_config.manually_accept_inbound_channels = manual;
        self
    }

    pub fn with_min_channel_value_sat(&mut self, min_value: u64) -> &mut Self {
        self.config.min_channel_value_sat = min_value;
        self.ldk_config
            .channel_handshake_limits
            .min_funding_satoshis = min_value;
        self
    }

    pub fn with_max_pending_htlcs(&mut self, max_htlcs: u16) -> &mut Self {
        self.config.max_pending_htlcs = max_htlcs;
        self.ldk_config
            .channel_handshake_config
            .our_max_accepted_htlcs = max_htlcs;
        self
    }

    pub fn with_default_channel_public(&mut self, public: bool) -> &mut Self {
        self.config.default_channel_public = public;
        self.ldk_config
            .channel_handshake_config
            .announce_for_forwarding = public;
        self
    }

    pub fn setup_bitcoind_rpc_client(
        &mut self,
        rpc_host: String,
        rpc_port: u16,
        user: String,
        password: String,
    ) -> &mut Self {
        self.config.bitcoind_rpc_host = rpc_host;
        self.config.bitcoind_rpc_port = rpc_port;
        self.config.bitcoind_username = user;
        self.config.bitcoind_password = password;
        self
    }

    pub fn build(&self) -> Result<Node, Box<dyn Error>> {
        let endpoint = HttpEndpoint::for_host(self.config.bitcoind_rpc_host.clone())
            .with_port(self.config.bitcoind_rpc_port);
        let credentials = BASE64_STANDARD.encode(format!(
            "{}:{}",
            self.config.bitcoind_username, self.config.bitcoind_password
        ));

        let network = self.config.network;
        let logger = Arc::new(NodeLogger {});
        let node_path = self.config.node_path.clone();
        if !exists(&node_path)? {
            fs::create_dir_all(&node_path)?;
        }

        let kv_store_path = node_path.join("store");
        let kv_store = Arc::new(FilesystemStore::new(kv_store_path.clone()));

        let bitcoind_client = Arc::new(BitcoindRpcClient::new(&credentials, endpoint));
        let wallet_manager = Arc::new(WalletManager::new(node_path.clone(), self.config.network)?);

        // Will have 2 store structs - a KVStore for the channel manager and the payments store.
        let network_graph = match kv_store.read(
            NETWORK_GRAPH_PERSISTENCE_PRIMARY_NAMESPACE,
            NETWORK_GRAPH_PERSISTENCE_SECONDARY_NAMESPACE,
            NETWORK_GRAPH_PERSISTENCE_KEY,
        ) {
            Ok(d) => {
                let mut reader = Cursor::new(d);
                NetworkGraph::read(&mut reader, logger.clone())
                    .map_err(|e| format!("Failed to deserialize NetworkGraph: {e}"))?
            }
            Err(e) => {
                if e.kind() == bitcoin::io::ErrorKind::NotFound {
                    NetworkGraph::new(network, Arc::clone(&logger))
                } else {
                    return Err(format!("Could not read network graph from disk {e}").into());
                }
            }
        };
        let network_graph = Arc::new(network_graph);

        let scorer = match kv_store.read(
            SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
            SCORER_PERSISTENCE_SECONDARY_NAMESPACE,
            SCORER_PERSISTENCE_KEY,
        ) {
            Ok(d) => {
                let mut reader = Cursor::new(d);
                Scorer::read(
                    &mut reader,
                    (
                        ProbabilisticScoringDecayParameters::default(),
                        Arc::clone(&network_graph),
                        Arc::clone(&logger),
                    ),
                )
                .map_err(|e| format!("Failed to deserialize scorer: {e}"))?
            }
            Err(e) => {
                if e.kind() == bitcoin::io::ErrorKind::NotFound {
                    Scorer::new(
                        ProbabilisticScoringDecayParameters::default(),
                        Arc::clone(&network_graph),
                        Arc::clone(&logger),
                    )
                } else {
                    return Err(format!("Could not scoring data from disk {e}").into());
                }
            }
        };
        let scorer = Arc::new(Mutex::new(scorer));

        let router = Arc::new(Router::new(
            Arc::clone(&network_graph),
            Arc::clone(&logger),
            Arc::clone(&wallet_manager),
            Arc::clone(&scorer),
            ProbabilisticScoringFeeParameters::default(),
        ));

        let peer_storage_key = wallet_manager.get_peer_storage_key();
        let chain_monitor = Arc::new(ChainMonitor::new(
            None,
            Arc::clone(&bitcoind_client),
            Arc::clone(&logger),
            Arc::clone(&bitcoind_client),
            Arc::clone(&kv_store),
            Arc::clone(&wallet_manager),
            peer_storage_key,
        ));

        let onion_message_router = Arc::new(OnionMessageRouter::new(
            Arc::clone(&network_graph),
            Arc::clone(&wallet_manager),
        ));

        let cur_time = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as u32;

        let channel_manager = if !exists(&kv_store_path)? {
            // Create new ChannelManager from scratch

            let bitcoind_clone = Arc::clone(&bitcoind_client);
            let best_block = tokio::task::block_in_place(move || {
                Handle::current().block_on(async move { bitcoind_clone.get_best_block().await })
            })?;

            let chain_params = ChainParameters {
                network: self.config.network,
                best_block: BestBlock::new(best_block.0, best_block.1),
            };

            Arc::new(ChannelManager::new(
                Arc::clone(&bitcoind_client),
                Arc::clone(&chain_monitor),
                Arc::clone(&bitcoind_client),
                Arc::clone(&router),
                Arc::clone(&onion_message_router),
                Arc::clone(&logger),
                Arc::clone(&wallet_manager),
                Arc::clone(&wallet_manager),
                Arc::clone(&wallet_manager),
                self.ldk_config.clone(),
                chain_params,
                cur_time,
            ))
        } else {
            let channel_monitors = read_channel_monitors(
                Arc::clone(&kv_store),
                Arc::clone(&wallet_manager),
                Arc::clone(&wallet_manager),
            )?;

            let mut channel_monitor_references = Vec::new();
            for (_, channel_monitor) in channel_monitors.iter() {
                channel_monitor_references.push(channel_monitor);
            }

            let read_args = ChannelManagerReadArgs::new(
                Arc::clone(&wallet_manager),
                Arc::clone(&wallet_manager),
                Arc::clone(&wallet_manager),
                Arc::clone(&bitcoind_client),
                Arc::clone(&chain_monitor),
                Arc::clone(&bitcoind_client),
                Arc::clone(&router),
                Arc::clone(&onion_message_router),
                Arc::clone(&logger),
                self.ldk_config.clone(),
                channel_monitor_references.clone(),
            );

            let file = File::open(kv_store_path.join("manager"))?;
            let (_block_hash, channel_manager) =
                <(BlockHash, ChannelManager)>::read(&mut BufReader::new(file), read_args)
                    .map_err(|e| format!("Could not read channel manager from disk {e}"))?;

            // TODO: check if there's a way to start chain monitor with channel monitors instead of
            // having to pass them like this
            for monitor in channel_monitor_references {
                let update_status = chain_monitor
                    .watch_channel(monitor.channel_id(), monitor.clone())
                    .map_err(|_| "Could not pass Channel Monitors to the Chain Monitor")?;
                assert!(update_status == ChannelMonitorUpdateStatus::Completed);
            }

            Arc::new(channel_manager)
        };

        let onion_messenger = Arc::new(OnionMessenger::new(
            Arc::clone(&wallet_manager),
            Arc::clone(&wallet_manager),
            Arc::clone(&logger),
            Arc::clone(&channel_manager),
            Arc::clone(&onion_message_router),
            Arc::clone(&channel_manager),
            IgnoringMessageHandler {},
            IgnoringMessageHandler {},
            IgnoringMessageHandler {},
        ));

        let gossip_sync = Arc::new(P2PGossipSync::new(
            Arc::clone(&network_graph),
            None,
            Arc::clone(&logger),
        ));

        let message_handler = MessageHandler {
            chan_handler: Arc::clone(&channel_manager),
            route_handler: Arc::clone(&gossip_sync),
            onion_message_handler: Arc::clone(&onion_messenger),
            custom_message_handler: IgnoringMessageHandler {},
            send_only_message_handler: Arc::clone(&chain_monitor),
        };

        let mut ephemeral_bytes = [0u8; 32];
        OsRng.fill_bytes(&mut ephemeral_bytes);
        let peer_manager = Arc::new(PeerManager::new(
            message_handler,
            cur_time,
            &ephemeral_bytes,
            Arc::clone(&logger),
            Arc::clone(&wallet_manager),
        ));

        let gossip_verifier = Arc::new(GossipVerifier::new(
            Arc::clone(&bitcoind_client),
            TokioSpawner {},
            Arc::clone(&gossip_sync),
            Arc::clone(&peer_manager),
        ));

        // This makes for a weird setup because the GossipVerifier needs a P2PGossipSync and
        // vice-versa so the P2PGossipSync is first initialized with no gossip verifier and
        // then initialize the verifier with the gossip sync to now add it here.
        gossip_sync.add_utxo_lookup(Some(gossip_verifier));

        let node_store = Arc::new(NodeStore::new(node_path.clone()));

        let node_id = wallet_manager
            .get_node_id(Recipient::Node)
            .map_err(|()| "could not get node id")?;

        let cancellation_token = CancellationToken::new();

        Ok(Node {
            //ldk_config: self.ldk_config,
            node_id,
            config: self.config.clone(),
            onchain_wallet: wallet_manager,
            channel_manager,
            chain_monitor,
            peer_manager,
            kv_store,
            gossip_sync,
            scorer,
            onion_messenger,
            logger,
            node_store,
            bitcoind_client,
            shutdown_token: cancellation_token,
        })
    }
}

pub struct Node {
    // alias: Option<String>,
    // ldk_config: UserConfig,
    node_id: PublicKey,
    config: Config,
    pub(crate) bitcoind_client: Arc<BitcoindRpcClient>,
    pub(crate) onchain_wallet: Arc<WalletManager>,
    pub(crate) channel_manager: Arc<ChannelManager>,
    chain_monitor: Arc<ChainMonitor>,
    peer_manager: Arc<PeerManager>,
    kv_store: Arc<FilesystemStore>,
    gossip_sync: Arc<P2PGossipSync>,
    scorer: Arc<Mutex<Scorer>>,
    onion_messenger: Arc<OnionMessenger>,
    logger: Arc<NodeLogger>,
    pub(crate) node_store: Arc<NodeStore>,
    shutdown_token: CancellationToken,
}

impl Node {
    pub async fn start(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        let mut cache = UnboundedCache::new();

        let monitors = self.chain_monitor.list_monitors();
        // Sync channel manager and channel monitors to same chain tip
        let chain_tip = {
            let channel_manager_best_block = self.channel_manager.current_best_block().block_hash;
            let mut listeners = vec![(
                channel_manager_best_block,
                &*self.channel_manager as &(dyn Listen + Send + Sync),
            )];

            let mut monitor_listeners = Vec::with_capacity(monitors.len());
            for monitor in monitors {
                let locked_monitor = self
                    .chain_monitor
                    .get_monitor(monitor)
                    .map_err(|_| "monitor not found")?
                    .clone();

                let best_block = locked_monitor.current_best_block();

                let monitor_listener = (
                    locked_monitor,
                    Arc::clone(&self.bitcoind_client),
                    Arc::clone(&self.bitcoind_client),
                    Arc::clone(&self.logger),
                );

                monitor_listeners.push((best_block.block_hash, monitor_listener));
            }

            for listener in monitor_listeners.iter_mut() {
                listeners.push((listener.0, &listener.1 as &(dyn Listen + Send + Sync)));
            }

            init::synchronize_listeners(
                Arc::clone(&self.bitcoind_client),
                self.config.network,
                &mut cache,
                listeners,
            )
            .await
            .map_err(|e| e.into_inner())?
        };

        let fee_bitcoind_client = Arc::clone(&self.bitcoind_client);
        let fee_cancellation_token = self.shutdown_token.clone();
        // background task to update fee estimates
        tokio::spawn(async move {
            fee_bitcoind_client
                .update_fee_estimates(fee_cancellation_token)
                .await;
        });

        // reconnect to peers on startup
        let reconnect_peer_manager = Arc::clone(&self.peer_manager);
        let peer_store = Arc::clone(&self.node_store);
        tokio::spawn(async move {
            let peers_stored = peer_store.list_peers()?;

            for peer in peers_stored {
                let addr = match peer.address {
                    SocketAddress::TcpIpV4 { addr, port } => {
                        SocketAddr::new(IpAddr::V4(Ipv4Addr::from(addr)), port)
                    }
                    SocketAddress::TcpIpV6 { addr, port } => {
                        SocketAddr::new(IpAddr::V6(Ipv6Addr::from(addr)), port)
                    }
                    _ => {
                        continue;
                    }
                };

                match connect(Arc::clone(&reconnect_peer_manager), &peer.node_id, addr) {
                    Ok(_) => {
                        log::info!("Connected to peer {}", peer.node_id);
                    }
                    Err(e) => {
                        log::error!("Could not reconnect to peer {e}");
                    }
                }
            }

            Ok::<(), Box<dyn Error + Send + Sync>>(())
        });

        let ldk_wallet = LdkWallet::new(Arc::clone(&self.onchain_wallet), Arc::clone(&self.logger));
        let bump_tx_handler = BumpTxEventHandler::new(
            Arc::clone(&self.bitcoind_client),
            Arc::new(ldk_wallet),
            Arc::clone(&self.onchain_wallet),
            Arc::clone(&self.logger),
        );

        let event_handler = Arc::new(LdkEventHandler {
            bitcoind_client: Arc::clone(&self.bitcoind_client),
            onchain_wallet: Arc::clone(&self.onchain_wallet),
            bump_tx_handler: Arc::new(bump_tx_handler),
            channel_manager: Arc::clone(&self.channel_manager),
            node_store: Arc::clone(&self.node_store),
            config: self.config.clone(),
        });

        let background_persister = Arc::clone(&self.kv_store);
        let background_event_handler = Arc::clone(&event_handler);
        let background_chain_mon = Arc::clone(&self.chain_monitor);
        let background_chan_man = Arc::clone(&self.channel_manager);
        let background_gossip_sync = GossipSync::p2p(Arc::clone(&self.gossip_sync));
        let background_peer_man = Arc::clone(&self.peer_manager);
        let background_onion_messenger = Arc::clone(&self.onion_messenger);
        let background_logger = Arc::clone(&self.logger);
        let background_scorer = Arc::clone(&self.scorer);

        // Setup the sleeper.
        let (stop_sender, stop_receiver) = tokio::sync::watch::channel(());
        let sleeper = move |d| {
            let mut receiver = stop_receiver.clone();
            Box::pin(async move {
                tokio::select! {
                    _ = tokio::time::sleep(d) => false,
                    _ = receiver.changed() => true,
                }
            })
        };

        let mobile_interruptable_platform = false;

        let handle = tokio::spawn(async move {
            process_events_async_with_kv_store_sync(
                background_persister,
                |e| background_event_handler.handle_event(e),
                background_chain_mon,
                background_chan_man,
                Some(background_onion_messenger),
                background_gossip_sync,
                background_peer_man,
                NO_LIQUIDITY_MANAGER,
                Option::<Arc<OutputSweeper>>::None,
                background_logger,
                Some(background_scorer),
                sleeper,
                mobile_interruptable_platform,
                || {
                    SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .ok()
                },
            )
            .await
        });

        // Pass blocks to ldk
        let block_connected_bitcoind = Arc::clone(&self.bitcoind_client);

        let channel_manager_listener = Arc::clone(&self.channel_manager);
        let chain_monitor_listener = Arc::clone(&self.chain_monitor);
        let onchain_wallet_listener = Arc::clone(&self.onchain_wallet);
        let network = self.config.network;

        tokio::spawn(async move {
            {
                let mut wallet_lock = onchain_wallet_listener.bdk_wallet.lock().unwrap();
                let wallet_tip = wallet_lock.latest_checkpoint();

                log::info!(
                    "Current wallet tip is: {} at height {}",
                    &wallet_tip.hash(),
                    &wallet_tip.height()
                );

                // Setting up another client not ideal
                let rpc_client = Client::new(
                    "127.0.0.1:18446",
                    Auth::UserPass("polaruser".to_string(), "polarpass".to_string()),
                )
                .unwrap();

                let mut emitter = Emitter::new(
                    &rpc_client,
                    wallet_tip.clone(),
                    wallet_tip.height(),
                    NO_EXPECTED_MEMPOOL_TXS,
                );

                while let Some(block) = emitter.next_block().unwrap() {
                    wallet_lock
                        .apply_block_connected_to(
                            &block.block,
                            block.block_height(),
                            block.connected_to(),
                        )
                        .unwrap();
                }
                let wallet_tip = wallet_lock.latest_checkpoint();
                log::info!("Finished syncing blocks at height {}", wallet_tip.height());
            }

            let chain_poller = ChainPoller::new(block_connected_bitcoind, network);
            let chain_listener = ChainListener {
                channel_manager: channel_manager_listener,
                chain_monitor: chain_monitor_listener,
                onchain_wallet: onchain_wallet_listener,
            };

            let mut spv_client =
                SpvClient::new(chain_tip, chain_poller, &mut cache, &chain_listener);
            loop {
                spv_client.poll_best_tip().await.unwrap();
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });

        // TODO:
        // - broadcast node announcement (if we have public channels)

        // background task to listen for inbound connections
        let peer_manager_conn_listener = Arc::clone(&self.peer_manager);
        tokio::spawn(async move {
            // TODO: handle unwraps
            let listen_port = 9735;
            let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", listen_port))
                .await
                .unwrap();

            loop {
                let tcp_stream = listener.accept().await.unwrap().0;

                let peer_manager_conn = Arc::clone(&peer_manager_conn_listener);
                tokio::spawn(async move {
                    lightning_net_tokio::setup_inbound(
                        peer_manager_conn,
                        tcp_stream.into_std().unwrap(),
                    )
                    .await;
                });
            }
        });

        // Wait for shutdown signal and stop background processing
        self.shutdown_token.cancelled().await;
        stop_sender.send(()).unwrap();

        let _ = handle.await.unwrap();

        // write peers to disk before shutdown
        {
            let peers: Vec<PeerDetails> = self.peer_manager.list_peers();
            for peer in peers {
                let address = match peer.socket_address {
                    Some(address) => address,
                    None => {
                        // If we are connected to them, we must have an address so it shouldn't get
                        // to here.
                        log::error!("Could not save peer {} to store", peer.counterparty_node_id);
                        continue;
                    }
                };

                let peer_info = PeerInfo {
                    node_id: peer.counterparty_node_id,
                    address,
                };
                match self.node_store.add_peer(&peer_info) {
                    Ok(_) => {
                        log::info!("Saved peer to store {:?}", peer_info);
                    }
                    Err(e) => {
                        log::error!("Could not save peer {e} to store");
                    }
                }
            }
            self.peer_manager.disconnect_all_peers();
        }

        Ok(())
    }

    pub fn open_channel(
        &self,
        peer_pubkey: PublicKey,
        channel_value_sats: u64,
        push_msat: u64,
    ) -> Result<ChannelId, Box<dyn Error>> {
        log::info!(
            "Got request to open channel to {:?} for {}",
            peer_pubkey,
            channel_value_sats
        );
        if self.peer_manager.peer_by_node_id(&peer_pubkey).is_none() {
            return Err(format!(
                "Unable to open channel. Not connected to peer {}",
                peer_pubkey,
            )
            .into());
        }

        // TODO: Verify we have enough onchain funds for channel
        log::info!("Creating channel to peer {:?}", peer_pubkey);

        let mut rng = rand::rng();
        let channel_id: u128 = rng.random();

        let channel_id = self
            .channel_manager
            .create_channel(
                peer_pubkey,
                channel_value_sats,
                push_msat,
                channel_id,
                None,
                None,
            )
            .map_err(|e| match e {
                APIError::APIMisuseError { err } | APIError::ChannelUnavailable { err } => {
                    format!("Could not open channel {err}")
                }
                _ => "Could not open channel".to_string(),
            })?;

        Ok(channel_id)
    }

    pub fn shutdown(&self) {
        self.shutdown_token.cancel();
    }

    pub fn close_channel(&self, channel_id: &ChannelId) -> Result<(), Box<dyn Error>> {
        let channels = self.channel_manager.list_channels();

        let channel = channels
            .iter()
            .find(|channel| channel.channel_id == *channel_id)
            .ok_or(format!("channel {} does not exist", channel_id))?;

        self.channel_manager
            .close_channel(channel_id, &channel.counterparty.node_id)
            .map_err(|e| match e {
                APIError::APIMisuseError { err } | APIError::ChannelUnavailable { err } => {
                    format!("Could not close channel {err}").into()
                }
                _ => "Could close channel".into(),
            })
    }

    pub fn list_channels(&self) -> Vec<ChannelInfo> {
        self.channel_manager
            .list_channels()
            .iter()
            .map(|cd| ChannelInfo::from(cd))
            .collect()
    }

    pub fn pay_bolt11_invoice(
        &self,
        invoice: Bolt11Invoice,
        amount_msat: Option<u64>,
    ) -> Result<PaymentId, Box<dyn Error>> {
        let amount = invoice
            .amount_milli_satoshis()
            .or(amount_msat)
            .ok_or("Amount not specified")?;

        let payment_id = PaymentId(invoice.payment_hash().to_byte_array());
        self.channel_manager
            .pay_for_bolt11_invoice(
                &invoice,
                payment_id,
                amount_msat,
                RouteParametersConfig::default(),
                Retry::Timeout(Duration::from_secs(15)),
            )
            // TODO: properly handle error
            .map_err(|e| format!("Could not make payment {:?}", e))?;

        let payment = Payment {
            payment_hash: PaymentHash(invoice.payment_hash().to_byte_array()),
            payment_preimage: None,
            payment_id,
            direction: PaymentDirection::Outbound,
            amount_msat: Some(amount),
            fee_paid_msat: None,
            status: PaymentStatus::Pending,
        };
        self.node_store.add_payment(payment)?;

        Ok(payment_id)
    }

    pub fn create_bolt11_invoice(
        &self,
        amount_msat: Option<u64>,
    ) -> Result<Bolt11Invoice, Box<dyn Error>> {
        let bolt11_invoice_params = Bolt11InvoiceParameters {
            amount_msats: amount_msat,
            description: Bolt11InvoiceDescription::Direct(Description::empty()),
            invoice_expiry_delta_secs: None,
            min_final_cltv_expiry_delta: None,
            payment_hash: None,
        };

        let bolt11_invoice = self
            .channel_manager
            .create_bolt11_invoice(bolt11_invoice_params)
            .map_err(|e| match e {
                lightning::bolt11_invoice::SignOrCreationError::SignError(_) => {
                    "could not create invoice".to_string()
                }
                lightning::bolt11_invoice::SignOrCreationError::CreationError(ce) => {
                    format!("could not create invoice {ce}")
                }
            })?;

        let payment_hash = bolt11_invoice.payment_hash().to_byte_array();
        let payment = Payment {
            payment_hash: PaymentHash(payment_hash),
            payment_preimage: None,
            payment_id: PaymentId(payment_hash),
            direction: PaymentDirection::Inbound,
            amount_msat,
            fee_paid_msat: None,
            status: PaymentStatus::Pending,
        };
        self.node_store.add_payment(payment)?;

        Ok(bolt11_invoice)
    }

    pub fn get_payment(&self, payment_id: PaymentId) -> Option<Payment> {
        self.node_store.get_payment(&payment_id).ok()
    }

    pub fn new_onchain_address(&self) -> Address {
        self.onchain_wallet.next_address()
    }

    pub fn balance(&self) -> Balance {
        let onchain_balance = self.onchain_wallet.bdk_wallet.lock().unwrap().balance();
        let channels = self.channel_manager.list_channels();

        let outbound_capacity: u64 = channels
            .iter()
            .map(|channel| channel.outbound_capacity_msat / 1000)
            .sum();

        Balance {
            onchain_confirmed: onchain_balance.confirmed,
            onchain_pending: onchain_balance.untrusted_pending + onchain_balance.trusted_pending,
            lightning: Amount::from_sat(outbound_capacity),
        }
    }

    pub fn node_id(&self) -> PublicKey {
        self.node_id
    }

    pub fn connect_to_peer(
        &self,
        peer_pubkey: &PublicKey,
        peer_addr: SocketAddr,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        connect(Arc::clone(&self.peer_manager), peer_pubkey, peer_addr)
    }
}

fn connect(
    peer_manager: Arc<PeerManager>,
    peer_pubkey: &PublicKey,
    peer_addr: SocketAddr,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    tokio::task::block_in_place(move || {
        Handle::current().block_on(async move {
            match lightning_net_tokio::connect_outbound(
                Arc::clone(&peer_manager),
                *peer_pubkey,
                peer_addr,
            )
            .await
            {
                Some(connection_closed_future) => {
                    let mut connection_closed_future = Box::pin(connection_closed_future);
                    loop {
                        tokio::select! {
                            _ = &mut connection_closed_future => return Err("Connection closed".into()),
                            _ = tokio::time::sleep(Duration::from_millis(10)) => {},
                        }
                        if peer_manager
                            .list_peers()
                            .iter()
                            .any(|peer| peer.counterparty_node_id == *peer_pubkey)
                        {
                            return Ok(());
                        }
                    }
                }
                None => Err("Connection timed out".into()),
            }
        })
    })
}
