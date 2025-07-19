use crate::{
    bitcoind_client::BitcoindRpcClient,
    config::Config,
    node::ChannelManager,
    onchain_wallet::WalletManager,
    storage::{PaymentStatus, PaymentStore},
};
use bdk_wallet::SignOptions;
use bitcoin::{Amount, FeeRate};
use lightning::{
    chain::chaininterface::{ConfirmationTarget, FeeEstimator},
    events::{Event, ReplayEvent},
};
use std::sync::Arc;

pub struct LdkEventHandler {
    pub(crate) bitcoind_client: Arc<BitcoindRpcClient>,
    pub(crate) onchain_wallet: Arc<WalletManager>,
    pub(crate) channel_manager: Arc<ChannelManager>,
    pub(crate) payment_store: Arc<PaymentStore>,
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
                let mut locked_wallet = self.onchain_wallet.bdk_wallet.lock().unwrap();
                let mut tx_builder = locked_wallet.build_tx();

                let amount = Amount::from_sat(channel_value_satoshis);
                let fee_rate = FeeRate::from_sat_per_kwu(
                    self.bitcoind_client
                        .get_est_sat_per_1000_weight(ConfirmationTarget::UrgentOnChainSweep)
                        as u64,
                );

                tx_builder
                    .add_recipient(output_script, amount)
                    .fee_rate(fee_rate);

                let mut psbt = match tx_builder.finish() {
                    Ok(psbt) => {
                        log::debug!("Created funding transaction: {:?}", psbt);
                        psbt
                    }
                    Err(err) => {
                        log::error!("Failed to create funding transaction: {}", err);
                        // Force-close channel on the manager
                        return Ok(());
                    }
                };

                match locked_wallet.sign(&mut psbt, SignOptions::default()) {
                    Ok(finalized) => {
                        if !finalized {
                            //return Err(Error::OnchainTxCreationFailed);
                        }
                    }
                    Err(err) => {
                        log::error!("Failed to create funding transaction: {}", err);
                        // return Err(err.into());
                    }
                }

                let funding_tx = match psbt.extract_tx() {
                    Ok(tx) => tx,
                    Err(e) => {
                        log::error!("Failed to extract transaction: {}", e);
                        return Ok(());
                    }
                };

                log::info!(
                    "Generated funding transaction {}",
                    funding_tx.compute_txid()
                );

                let _ = self.channel_manager.funding_transaction_generated(
                    temporary_channel_id,
                    counterparty_node_id,
                    funding_tx,
                );
                return Ok(());
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

                // TODO: user_channel_id and do not unwrap
                self.channel_manager
                    .accept_inbound_channel(&temporary_channel_id, &counterparty_node_id, 0)
                    .unwrap();
            }
            Event::ChannelClosed {
                channel_id,
                user_channel_id,
                reason,
                counterparty_node_id: _,
                ..
            } => {
                log::info!(
                    "Channel {} with peer {} got closed. REASON: {}",
                    channel_id,
                    user_channel_id,
                    reason
                )
            }

            // -------- Payment-related events --------
            Event::PaymentSent {
                payment_preimage,
                payment_hash,
                fee_paid_msat,
                ..
            } => match self.payment_store.get_payment(&payment_hash) {
                Ok(mut payment) => {
                    log::info!(
                        "Payment {} for amount {} sent successfully. Preimage: {}. Paid fee {}",
                        payment_hash,
                        payment.amount_msat,
                        payment_preimage,
                        fee_paid_msat.unwrap(), // Safe to unwrap because this will only be none
                                                // for LDK versions prior to 0.0.103
                    );
                    payment.status = PaymentStatus::Succeeded;
                    payment.payment_preimage = Some(payment_preimage);
                    payment.fee_paid_msat = fee_paid_msat;
                    // TODO: set payment updated timestamp

                    if self.payment_store.update_payment(payment).is_err() {
                        log::error!(
                            "Could not update existing payment in store. Something really wrong..."
                        )
                    }
                }
                Err(e) => {
                    log::error!(
                        "Could not retrieve payment from store: {}. Something really wrong...",
                        e
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
                        if payment_preimage.is_some() {
                            self.channel_manager.claim_funds(payment_preimage.unwrap());
                        } else {
                            log::debug!("got unknown payment hash {:?}. Ignoring...", payment_hash)
                        }
                    }
                    _ => self.channel_manager.fail_htlc_backwards(&payment_hash),
                }
            }
            Event::PaymentClaimed { .. } => {}
            Event::PaymentFailed {
                payment_id: _,
                payment_hash,
                reason,
            } => {
                if payment_hash.is_some() {
                    match self.payment_store.get_payment(&payment_hash.unwrap()) {
                        Ok(mut payment) => {
                            log::info!(
                                "Payment {:?} failed with reason {:?}",
                                payment_hash,
                                reason.unwrap()
                            );
                            payment.status = PaymentStatus::Failed;

                            let _ = self.payment_store.update_payment(payment);
                        }
                        Err(e) => {
                            log::error!(
                                "Could not retrieve payment {:?} during PaymentFailed notification. Error {}",
                                payment_hash,
                                e
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
                log::info!("Payment {} took path {:?}", payment_hash.unwrap(), path); // Safe to
                // unwrap the hash. Will only be none for payments before LDK 0.0.104
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
