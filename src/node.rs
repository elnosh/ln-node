use std::error::Error;
use std::fs::File;
use std::io::BufReader;
use std::net::SocketAddr;
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
use bitcoin::{Address, Amount, BlockHash};
use lightning::bolt11_invoice::{Bolt11Invoice, Bolt11InvoiceDescription, Description};
use lightning::chain::{ChannelMonitorUpdateStatus, Listen, Watch};
use lightning::io::Cursor;
use lightning::ln::channel_state::ChannelDetails;
use lightning::ln::channelmanager::{
    Bolt11InvoiceParameters, ChannelManagerReadArgs, PaymentId, RecipientOnionFields, Retry,
};
use lightning::ln::peer_handler::MessageHandler;
use lightning::ln::types::ChannelId;
use lightning::routing::router::{PaymentParameters, RouteParameters};
use lightning::sign::{NodeSigner, Recipient};
use lightning::types::payment::PaymentHash;
use lightning::util::errors::APIError;
use lightning::util::persist::{
    KVStore, SCORER_PERSISTENCE_KEY, SCORER_PERSISTENCE_PRIMARY_NAMESPACE,
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
use lightning_background_processor::{GossipSync, process_events_async};
use lightning_block_sync::poll::ChainPoller;
use lightning_block_sync::{SpvClient, UnboundedCache, init};
use lightning_block_sync::{gossip::TokioSpawner, http::HttpEndpoint};
use lightning_net_tokio::SocketDescriptor as LdkTokioSocketDescriptor;
use lightning_persister::fs_store::FilesystemStore;
use rand::Rng;
use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;

use crate::bitcoind_client::ChainListener;
use crate::event_handler::LdkEventHandler;
use crate::storage::{Payment, PaymentDirection, PaymentStatus};
use crate::{
    bitcoind_client::BitcoindRpcClient, config::Config, logger::NodeLogger,
    onchain_wallet::WalletManager, storage::PaymentStore,
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
>;

#[derive(Serialize, Deserialize)]
pub struct ChannelInfo {
    //pub channel_id: ChannelId,
    pub channel_id: [u8; 32],
    pub counterparty: PublicKey,
    // pub funding_txo: Option<OutPoint>,
    pub short_channel_id: Option<u64>,
    pub channel_value_satoshis: u64,
    // pub user_channel_id: u128,
    pub outbound_capacity_msat: u64,
    pub inbound_capacity_msat: u64,
    pub confirmations_required: Option<u32>,
    pub is_outbound: bool,
    pub is_channel_ready: bool,
    pub is_usable: bool,
    pub is_announced: bool,
}

impl From<&ChannelDetails> for ChannelInfo {
    fn from(value: &ChannelDetails) -> Self {
        Self {
            channel_id: value.channel_id.0,
            counterparty: value.counterparty.node_id,
            short_channel_id: value.short_channel_id,
            channel_value_satoshis: value.channel_value_satoshis,
            //user_channel_id: value.user_channel_id,
            outbound_capacity_msat: value.outbound_capacity_msat,
            inbound_capacity_msat: value.inbound_capacity_msat,
            confirmations_required: value.confirmations_required,
            is_outbound: value.is_outbound,
            is_channel_ready: value.is_channel_ready,
            is_usable: value.is_usable,
            is_announced: value.is_announced,
        }
    }
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

    fn setup_bitcoind_rpc_client(
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

        let chain_monitor = Arc::new(ChainMonitor::new(
            None,
            Arc::clone(&bitcoind_client),
            Arc::clone(&logger),
            Arc::clone(&bitcoind_client),
            Arc::clone(&kv_store),
        ));

        let onion_message_router = Arc::new(OnionMessageRouter::new(
            Arc::clone(&network_graph),
            Arc::clone(&wallet_manager),
        ));

        let cur_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as u32;

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
                self.ldk_config,
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
                self.ldk_config,
                channel_monitor_references.clone(),
            );

            let file = File::open(kv_store_path.join("manager"))?;
            let (_block_hash, channel_manager) =
                <(BlockHash, ChannelManager)>::read(&mut BufReader::new(file), read_args)
                    .map_err(|e| format!("Could not read channel manager from disk {e}"))?;

            for monitor in channel_monitor_references {
                let update_status = chain_monitor
                    .watch_channel(monitor.get_funding_txo().0, monitor.clone())
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

        let payment_store = Arc::new(PaymentStore::new(node_path.clone()));

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
            payment_store,
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
    pub(crate) payment_store: Arc<PaymentStore>,
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
                    .get_monitor(monitor.0)
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

            let chain_tip = init::synchronize_listeners(
                Arc::clone(&self.bitcoind_client),
                self.config.network,
                &mut cache,
                listeners,
            )
            .await
            .map_err(|e| e.into_inner())?;

            chain_tip
        };

        let event_handler = Arc::new(LdkEventHandler {
            bitcoind_client: Arc::clone(&self.bitcoind_client),
            onchain_wallet: Arc::clone(&self.onchain_wallet),
            channel_manager: Arc::clone(&self.channel_manager),
            payment_store: Arc::clone(&self.payment_store),
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
            process_events_async(
                background_persister,
                |e| background_event_handler.handle_event(e),
                background_chain_mon,
                background_chan_man,
                Some(background_onion_messenger),
                background_gossip_sync,
                background_peer_man,
                background_logger,
                Some(background_scorer),
                sleeper,
                mobile_interruptable_platform,
                || {
                    Some(
                        SystemTime::now()
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .unwrap(),
                    )
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
        // - broadcast announcements (if we have open channels)

        // Background task to listen for inbound connections
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

        // write peers to disk
        self.peer_manager.disconnect_all_peers();

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
    ) -> Result<(), Box<dyn Error>> {
        let mut recipient_onion = RecipientOnionFields::secret_only(*invoice.payment_secret());
        recipient_onion.payment_metadata = invoice.payment_metadata().map(|v| v.clone());

        let mut payment_params = PaymentParameters::from_node_id(
            invoice.recover_payee_pub_key(),
            invoice.min_final_cltv_expiry_delta() as u32,
        )
        .with_route_hints(invoice.route_hints())
        .map_err(|_| "invalid invoice")?;

        if let Some(expiry) = invoice.expires_at() {
            payment_params = payment_params.with_expiry_time(expiry.as_secs());
        }
        if let Some(features) = invoice.features() {
            payment_params = payment_params
                .with_bolt11_features(features.clone())
                .unwrap();
        }

        let amount = match invoice.amount_milli_satoshis() {
            Some(a) => a,
            None => amount_msat.ok_or("amount not specified for amountless invoice")?,
        };

        let route_params = RouteParameters::from_payment_params_and_value(payment_params, amount);

        // TODO: save payment in store
        // NOTE: future releases of LDK will have a `pay_for_bolt11_invoice` method that could be
        // used instead. It does most of the parsing and params setup as done here.

        let payment_hash = invoice.payment_hash().to_byte_array();
        self.channel_manager
            .send_payment(
                PaymentHash(payment_hash),
                recipient_onion,
                PaymentId(payment_hash),
                route_params,
                Retry::Timeout(Duration::from_secs(15)),
            )
            // TODO: properly handle error
            .map_err(|_e| "error making payment")?;

        Ok(())
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

        let payment = Payment {
            payment_hash: PaymentHash(bolt11_invoice.payment_hash().to_byte_array()),
            payment_preimage: None,
            direction: PaymentDirection::Inbound,
            // TODO: don't do this
            amount_msat: amount_msat.unwrap_or(0),
            fee_paid_msat: None,
            status: PaymentStatus::Pending,
        };
        self.payment_store.add_payment(payment)?;

        Ok(bolt11_invoice)
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
        tokio::task::block_in_place(move || {
            Handle::current().block_on(async move {
        match lightning_net_tokio::connect_outbound(
            Arc::clone(&self.peer_manager),
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
                    if self
                        .peer_manager
                        .list_peers()
                        .iter()
                        .any(|peer| peer.counterparty_node_id == *peer_pubkey)
                    {
                        return Ok(());
                    }
                }
            }
            None => return Err("Connection timed out".into()),
        }
            })
        })
    }
}
