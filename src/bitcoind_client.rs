use bitcoin::block::Header;
use bitcoin::consensus::encode::serialize_hex;
use bitcoin::{Block, BlockHash, OutPoint, Transaction};
use lightning::chain::Listen;
use lightning::chain::chaininterface::{BroadcasterInterface, ConfirmationTarget, FeeEstimator};
use lightning::chain::transaction::TransactionData;
use lightning_block_sync::gossip::UtxoSource;
use lightning_block_sync::http::HttpEndpoint;
use lightning_block_sync::rpc::RpcClient;
use lightning_block_sync::{AsyncBlockSourceResult, BlockData, BlockHeaderData, BlockSource};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::error::Error;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::select;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::node::{ChainMonitor, ChannelManager};
use crate::onchain_wallet::WalletManager;

const CONF_TARGETS: [ConfirmationTarget; 8] = [
    ConfirmationTarget::MaximumFeeEstimate,
    ConfirmationTarget::UrgentOnChainSweep,
    ConfirmationTarget::MinAllowedAnchorChannelRemoteFee,
    ConfirmationTarget::MinAllowedNonAnchorChannelRemoteFee,
    ConfirmationTarget::AnchorChannelFee,
    ConfirmationTarget::NonAnchorChannelFee,
    ConfirmationTarget::ChannelCloseMinimum,
    ConfirmationTarget::OutputSpendingFee,
];

pub struct BitcoindRpcClient {
    rpc_client: RpcClient,
    fee_cache: Arc<Mutex<HashMap<ConfirmationTarget, u32>>>,
}

impl BitcoindRpcClient {
    pub fn new(credentials: &str, endpoint: HttpEndpoint) -> Self {
        let fallback_fees = CONF_TARGETS
            .iter()
            .map(|target| (target.clone(), get_fallback_fee(*target)));
        let fee_cache = HashMap::from_iter(fallback_fees);

        Self {
            rpc_client: RpcClient::new(credentials, endpoint),
            fee_cache: Arc::new(Mutex::new(fee_cache)),
        }
    }

    fn confirmation_target_to_blocks(&self, target: ConfirmationTarget) -> u32 {
        match target {
            ConfirmationTarget::MaximumFeeEstimate => 1,
            ConfirmationTarget::UrgentOnChainSweep => 1,
            ConfirmationTarget::OutputSpendingFee => 6,
            ConfirmationTarget::MinAllowedAnchorChannelRemoteFee => 1008,
            ConfirmationTarget::MinAllowedNonAnchorChannelRemoteFee => 144,
            ConfirmationTarget::AnchorChannelFee => 1008,
            ConfirmationTarget::NonAnchorChannelFee => 12,
            ConfirmationTarget::ChannelCloseMinimum => 144,
        }
    }

    async fn fetch_fee_estimate(
        &self,
        confirmation_target: ConfirmationTarget,
    ) -> Result<u32, Box<dyn Error>> {
        let blocks = self.confirmation_target_to_blocks(confirmation_target);

        let response: Value = self
            .rpc_client
            .call_method("estimatesmartfee", &[json!(blocks)])
            .await
            .map_err(|e| format!("estimatesmartfee RPC call failed: {}", e))?;

        let fee_rate = response
            .get("feerate")
            .and_then(|v| v.as_f64())
            .ok_or("Missing or invalid feerate in response")?;

        let sat_per_1000_weight = (fee_rate * 100_000_000.0 / 4.0) as u32;

        Ok(sat_per_1000_weight.max(253))
    }

    // method to run in a background that will poll fee estimates from bitcoin core every 5 minutes
    pub async fn update_fee_estimates(&self, shutdown_signal: CancellationToken) {
        let update_fees = || async {
            for target in CONF_TARGETS {
                match self.fetch_fee_estimate(target).await {
                    Ok(mut rate) => {
                        let mut cache = self.fee_cache.lock().unwrap();
                        if target == ConfirmationTarget::MaximumFeeEstimate {
                            rate = rate.saturating_add(2500);
                        };
                        cache.insert(target, rate);
                    }
                    Err(e) => {
                        log::error!("Could not fetch fee estimate from bitcoind {e}")
                    }
                }
            }
            log::info!("Updated fee estimates");
        };

        update_fees().await;

        loop {
            select! {
                _ = sleep(Duration::from_secs(60 * 5)) => {
                    update_fees().await;
                }
                _ = shutdown_signal.cancelled() => {
                    break
                }
            }
        }
    }

    pub async fn get_best_block(&self) -> Result<(BlockHash, u32), Box<dyn Error>> {
        let response: Value = self
            .rpc_client
            .call_method("getblockchaininfo", &[])
            .await
            .map_err(|e| format!("getblockchaininfo RPC call failed: {}", e))?;

        let best_block_hash = response
            .get("bestblockhash")
            .and_then(|v| v.as_str())
            .ok_or("Missing or invalid bestblockhash in response")?;

        let block_height = response
            .get("blocks")
            .and_then(|v| v.as_u64())
            .ok_or("Missing blocks in response")?;

        let best_block_hash =
            BlockHash::from_str(best_block_hash).expect("Invalid block hash format");

        // should be ok to downcast
        Ok((best_block_hash, block_height as u32))
    }
}

fn get_fallback_fee(confirmation_target: ConfirmationTarget) -> u32 {
    match confirmation_target {
        ConfirmationTarget::MaximumFeeEstimate => 10000,
        ConfirmationTarget::UrgentOnChainSweep => 5000,
        ConfirmationTarget::OutputSpendingFee => 4000,
        ConfirmationTarget::MinAllowedAnchorChannelRemoteFee => 253,
        ConfirmationTarget::MinAllowedNonAnchorChannelRemoteFee => 1000,
        ConfirmationTarget::AnchorChannelFee => 253,
        ConfirmationTarget::NonAnchorChannelFee => 3000,
        ConfirmationTarget::ChannelCloseMinimum => 1000,
    }
}

impl FeeEstimator for BitcoindRpcClient {
    fn get_est_sat_per_1000_weight(&self, confirmation_target: ConfirmationTarget) -> u32 {
        *self
            .fee_cache
            .lock()
            .unwrap()
            .get(&confirmation_target)
            .unwrap()
    }
}

impl BroadcasterInterface for BitcoindRpcClient {
    fn broadcast_transactions(&self, txs: &[&Transaction]) {
        for tx in txs {
            let serialized_tx = serialize_hex(tx);

            // TODOs:
            // - use submitpackge rpc call if more than 1 tx
            // - don't block, can do it in a task.

            tokio::task::block_in_place(move || {
                Handle::current().block_on(async move {
                    match self
                        .rpc_client
                        .call_method::<Value>("sendrawtransaction", &[json!(serialized_tx)])
                        .await
                    {
                        Ok(_) => {
                            log::info!("broadcasting transaction {:?}", tx)
                        }
                        Err(e) => {
                            log::error!(
                                "WARNING: could not broadcast transaction LDK needed us to broadcast: {}", e
                            )
                        }
                    }
                })
            });
        }
    }
}

impl UtxoSource for BitcoindRpcClient {
    fn get_block_hash_by_height(&self, block_height: u32) -> AsyncBlockSourceResult<BlockHash> {
        self.rpc_client.get_block_hash_by_height(block_height)
    }

    fn is_output_unspent(&self, outpoint: OutPoint) -> AsyncBlockSourceResult<bool> {
        self.rpc_client.is_output_unspent(outpoint)
    }
}

impl BlockSource for BitcoindRpcClient {
    fn get_header<'a>(
        &'a self,
        header_hash: &'a BlockHash,
        height_hint: Option<u32>,
    ) -> AsyncBlockSourceResult<'a, BlockHeaderData> {
        self.rpc_client.get_header(header_hash, height_hint)
    }

    fn get_block<'a>(
        &'a self,
        header_hash: &'a BlockHash,
    ) -> AsyncBlockSourceResult<'a, BlockData> {
        self.rpc_client.get_block(header_hash)
    }

    fn get_best_block(&self) -> AsyncBlockSourceResult<(BlockHash, Option<u32>)> {
        self.rpc_client.get_best_block()
    }
}

// ChainListener aggregates calls in one place to provide block updates to the different structs
// that need it. Whenever a new block is connected or disconnected, it needs to be communicated to
// the ChannelManager, ChainMonitor and the onchain bdk wallet. So this listener will receive the
// notification of a new block and then call to provide the new block to all these structs.
pub(crate) struct ChainListener {
    pub(crate) channel_manager: Arc<ChannelManager>,
    pub(crate) chain_monitor: Arc<ChainMonitor>,
    pub(crate) onchain_wallet: Arc<WalletManager>,
}

impl Listen for ChainListener {
    fn filtered_block_connected(&self, header: &Header, txdata: &TransactionData, height: u32) {
        self.channel_manager
            .filtered_block_connected(header, txdata, height);

        self.chain_monitor
            .filtered_block_connected(header, txdata, height);

        let block = Block {
            header: *header,
            txdata: txdata.iter().map(|txdata| txdata.1.clone()).collect(),
        };

        self.onchain_wallet
            .bdk_wallet
            .lock()
            .unwrap()
            .apply_block(&block, height)
            // TODO: handle error
            .unwrap();
    }

    fn block_disconnected(&self, header: &bitcoin::block::Header, height: u32) {
        self.channel_manager.block_disconnected(header, height);
        self.chain_monitor.block_disconnected(header, height);
        // There's no notion of disconnecting blocks in BDK. See how to handle reorgs.
        // If this doesn't work, then will probably need to use this `Emitter`
        // https://docs.rs/bdk_bitcoind_rpc/latest/bdk_bitcoind_rpc/struct.Emitter.html
    }
}
