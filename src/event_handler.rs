use crate::{
    bitcoind_client::BitcoindRpcClient,
    config::Config,
    node::ChannelManager,
    onchain_wallet::WalletManager,
    storage::{NodeStore, PaymentStatus},
};
use lightning::{
    events::{Event, ReplayEvent},
    util::errors::APIError,
};
use std::sync::Arc;

pub struct LdkEventHandler {
    pub(crate) bitcoind_client: Arc<BitcoindRpcClient>,
    pub(crate) onchain_wallet: Arc<WalletManager>,
    pub(crate) channel_manager: Arc<ChannelManager>,
    pub(crate) node_store: Arc<NodeStore>,
    pub(crate) config: Config,
}

impl LdkEventHandler {
    pub async fn handle_event(&self, event: Event) -> Result<(), ReplayEvent> {
        match event {
            // -------- Channel-related events --------

            // Generate a funding tx for a channel open
            Event::FundingGenerationReady {
                temporary_channel_id,
                counterparty_node_id,
                channel_value_satoshis,
                output_script,
                ..
            } => {
                match self.onchain_wallet.build_transaction(
                    channel_value_satoshis,
                    output_script,
                    Arc::clone(&self.bitcoind_client),
                ) {
                    Ok(funding_tx) => {
                        let funding_txid = funding_tx.compute_txid();
                        match self.channel_manager.funding_transaction_generated(
                            temporary_channel_id,
                            counterparty_node_id,
                            funding_tx,
                        ) {
                            Ok(_) => {
                                log::info!(
                                    "Generated funding transaction {} for channel with node {}",
                                    funding_txid,
                                    counterparty_node_id
                                );
                            }
                            Err(_) => {
                                log::error!("Channel unavailable to create funding transaction",)
                            }
                        }
                    }
                    Err(e) => {
                        log::error!("Could not create funding transaction {}", e)
                        // NOTE: should something be done to tell LDK we can't create the funding
                        // tx?
                    }
                }
            }
            Event::ChannelPending {
                channel_id,
                user_channel_id: _,
                counterparty_node_id: _,
                ..
            } => {
                log::info!("Channel {} waiting for confirmation.", channel_id)
            }
            Event::ChannelReady {
                channel_id,
                user_channel_id: _,
                counterparty_node_id,
                ..
            } => {
                log::info!(
                    "Channel {} with peer {} ready to be used",
                    channel_id,
                    counterparty_node_id
                )
            }
            Event::OpenChannelRequest {
                temporary_channel_id,
                counterparty_node_id,
                funding_satoshis,
                channel_type,
                ..
            } => {
                // TODO: need to check inside LDK if they will do the checks we set in our config
                // we passed to the channel manager. I.e the configs with params for new channels,
                // does LDK check them before passing this event to us or the fact that we set the
                // knob to manually accept inbound channels means they will immediately hand it to
                // us and we have to do all our checks here.
                if funding_satoshis < self.config.min_channel_value_sat {}

                if channel_type.supports_anchors_zero_fee_htlc_tx() {
                    // NOTE: For now assume here that we want a buffer 10000 sats per channel
                    // to keep as reserve in onchain funds in case we need to fee-bump a tx for
                    // a channel closure
                    let onchain_balance = self.onchain_wallet.bdk_wallet.lock().unwrap().balance();
                    let confirmed_balance_sat = onchain_balance.confirmed.to_sat();
                    if confirmed_balance_sat < 10_000 {
                        match self.channel_manager.force_close_without_broadcasting_txn(
                            &temporary_channel_id,
                            &counterparty_node_id,
                            "unable to accept new channel".to_string(),
                        ) {
                            Ok(_) => {
                                log::info!(
                                    "Rejecting channel {:?} for {} sats. Do not have enough onchain funds for anchor channel",
                                    temporary_channel_id,
                                    funding_satoshis
                                );
                            }
                            Err(_) => {
                                log::error!("Error rejecting channel {:?}", temporary_channel_id);
                            }
                        }
                    }
                }

                // TODO: user_channel_id
                match self.channel_manager.accept_inbound_channel(
                    &temporary_channel_id,
                    &counterparty_node_id,
                    0,
                ) {
                    Ok(_) => {
                        log::info!(
                            "Accepted channel {} from {} for {} sats",
                            temporary_channel_id,
                            counterparty_node_id,
                            funding_satoshis
                        );
                    }
                    Err(e) => match e {
                        APIError::APIMisuseError { err } | APIError::ChannelUnavailable { err } => {
                            log::error!("Could not accept channel {}", err)
                        }
                        _ => {
                            log::error!("Could not accept channel")
                        }
                    },
                }
            }
            Event::ChannelClosed {
                channel_id,
                user_channel_id,
                reason,
                counterparty_node_id: _,
                ..
            } => {
                log::info!(
                    "Channel {} with peer {} got closed. Reason: {}",
                    channel_id,
                    user_channel_id,
                    reason
                )
            }

            // -------- Payment-related events --------
            Event::PaymentSent {
                payment_id,
                payment_preimage,
                payment_hash,
                fee_paid_msat,
                ..
            } => match self.node_store.get_payment(&payment_id.unwrap()) {
                Ok(mut payment) => {
                    log::info!(
                        "Payment {} for amount {} sent successfully. Preimage: {}. Paid fee {}",
                        payment_hash,
                        // the amount is always Some when sending
                        payment.amount_msat.unwrap(),
                        payment_preimage,
                        fee_paid_msat.unwrap(), // Safe to unwrap because this will only be none
                                                // for LDK versions prior to 0.0.103
                    );
                    payment.status = PaymentStatus::Succeeded;
                    payment.payment_preimage = Some(payment_preimage);
                    payment.fee_paid_msat = fee_paid_msat;
                    // TODO: set payment updated timestamp

                    if self.node_store.update_payment(payment).is_err() {
                        log::error!(
                            "Could not update existing payment in store. Something really wrong..."
                        )
                    }
                }
                Err(e) => {
                    log::error!(
                        "Could not retrieve payment from store: {e}. Something really wrong..."
                    )
                }
            },
            Event::PaymentClaimable {
                receiver_node_id: _,
                payment_hash,
                purpose,
                ..
            } => {
                // TODO: check if payment is stored in db
                match purpose {
                    lightning::events::PaymentPurpose::Bolt11InvoicePayment {
                        payment_preimage,
                        ..
                    } => {
                        if let Some(preimage) = payment_preimage {
                            self.channel_manager.claim_funds(preimage);
                        } else {
                            log::debug!("got unknown payment hash {:?}. Ignoring...", payment_hash)
                        }
                    }
                    _ => self.channel_manager.fail_htlc_backwards(&payment_hash),
                }
            }
            Event::PaymentClaimed { .. } => {}
            Event::PaymentFailed {
                payment_id,
                payment_hash,
                reason,
            } => {
                if payment_hash.is_some() {
                    match self.node_store.get_payment(&payment_id) {
                        Ok(mut payment) => {
                            log::info!(
                                "Payment {:?} failed with reason {:?}",
                                payment_hash,
                                reason.unwrap() // Safe to unwrap because this will only be none
                                                // for LDK versions prior to 0.0.115
                            );
                            payment.status = PaymentStatus::Failed;

                            let _ = self.node_store.update_payment(payment);
                        }
                        Err(e) => {
                            log::error!(
                                "Could not retrieve payment {:?} during PaymentFailed notification. Error {e}",
                                payment_hash,
                            )
                        }
                    }
                }
            }
            // This does not mean the entire payment failed. Just this path that was tried.
            Event::PaymentPathFailed { .. } => {}
            Event::PaymentPathSuccessful {
                payment_id: _,
                payment_hash,
                path,
            } => {
                // Safe to unwrap the hash. Will only be none for payments before LDK 0.0.104
                log::info!("Payment {} took path {:?}", payment_hash.unwrap(), path);
            }
            Event::PaymentForwarded { .. } => {}
            // Not handling bolt12 invoices manually.
            Event::InvoiceReceived { .. } => {}
            // NOTE: This event will removed in future releases of LDK as the forwarding of HTLCs
            // will be primarily driven by the background processor
            Event::PendingHTLCsForwardable { time_forwardable } => {
                tokio::time::sleep(time_forwardable).await;
                self.channel_manager.process_pending_htlc_forwards();
            }
            Event::HTLCIntercepted { .. } => {}
            Event::HTLCHandlingFailed { .. } => {}

            // -------- Onchain --------
            Event::SpendableOutputs { .. } => {}
            Event::BumpTransaction(_bump_event) => {}
            Event::DiscardFunding { .. } => {}
            Event::FundingTxBroadcastSafe { .. } => {}

            // -------- Onion Message --------
            Event::OnionMessageIntercepted { .. } => {}
            Event::OnionMessagePeerConnected { .. } => {}
            Event::ConnectionNeeded { .. } => {}

            // -------- Others --------
            Event::ProbeFailed { .. } => {}
            Event::ProbeSuccessful { .. } => {}
        }
        Ok(())
    }
}
