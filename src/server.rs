use std::io::{self};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use bitcoin::secp256k1::PublicKey;
use lightning::bolt11_invoice::Bolt11Invoice;
use lightning::ln::types::ChannelId;
use serde_json::json;
use tokio::net::UnixListener;
use tokio_util::sync::CancellationToken;

use crate::node::Node;

pub struct UnixSocketServer {
    listener: UnixListener,
    node: Arc<Node>,
    shutdown_listener: CancellationToken,
}

impl UnixSocketServer {
    pub fn new(
        path: PathBuf,
        node: Arc<Node>,
        shutdown_listener: CancellationToken,
    ) -> Result<UnixSocketServer, io::Error> {
        let listener = UnixListener::bind(path)?;
        Ok(UnixSocketServer {
            listener,
            node,
            shutdown_listener,
        })
    }

    pub async fn start_server(&self) {
        loop {
            tokio::select! {
                res = self.listener.accept() => {
                    match res {
                        Ok((stream, _addr)) => {
                            stream.readable().await.unwrap();

                            let mut buffer = [0; 4096];
                            let n = stream.try_read(&mut buffer).unwrap();
                            let request: serde_json::Value = serde_json::from_slice(&buffer[..n]).unwrap();

                            let response = match request["method"].as_str().unwrap() {
                                "node_pubkey" => {
                                    let node_pubkey = self.node.node_id();
                                    json!({ "result": node_pubkey })
                                }
                                "new_address" => {
                                    let address = self.node.new_onchain_address();
                                    json!({ "result": address.to_string() })
                                }
                                "balance" => {
                                    let balance = self.node.balance();
                                    json!({ "onchain": { "confirmed": balance.onchain_confirmed, "pending": balance.onchain_pending }, "lightning": balance.lightning })
                                }
                                "open_channel" => {
                                    let pubkey = request["params"]["pubkey"].as_str().unwrap();
                                    let amount = request["params"]["amount"].as_u64().unwrap();
                                    self.node
                                        .open_channel(pubkey.parse().unwrap(), amount, 0)
                                        .unwrap();
                                    json!({ "result": "Channel opened" })
                                }
                                "list_channels" => {
                                    let channels = self.node.list_channels();
                                    json!({"channels": channels })
                                }
                                "close_channel" => {
                                    let chan_id = request["params"]["channel_id"].as_str().unwrap();
                                    let chan_id = ChannelId::from_bytes(
                                        hex::decode(chan_id).unwrap().try_into().unwrap(),
                                    );

                                    match self.node.close_channel(&chan_id) {
                                        Ok(_) => json!({"result": "Channel closed"}),
                                        Err(e) => {
                                            json!({"result": format!("Could not close channel {}", e)})
                                        }
                                    }
                                }
                                "send_payment" => {
                                    let invoice = request["params"]["invoice"].as_str().unwrap();
                                    self.node
                                        .pay_bolt11_invoice(Bolt11Invoice::from_str(invoice).unwrap(), None)
                                        .unwrap();
                                    json!({ "result": "Payment sent" })
                                }
                                "receive" => {
                                    let amount_msat = request["params"]["amount"].as_u64().unwrap();
                                    let bolt11 =
                                        self.node.create_bolt11_invoice(Some(amount_msat)).unwrap();
                                    json!({"invoice": bolt11.to_string()})
                                }
                                "connect" => {
                                    let node_pubkey = request["params"]["pubkey"].as_str().unwrap();
                                    let node_pubkey = PublicKey::from_str(node_pubkey).unwrap();
                                    let address = request["params"]["address"].as_str().unwrap();
                                    let address = SocketAddr::from_str(address).unwrap();

                                    match self.node.connect_to_peer(&node_pubkey, address) {
                                        Ok(_) => json!({"result": "Connected to peer"}),
                                        Err(e) => {
                                            json!({"result": format!("Could not connect to peer {}", e)})
                                        }
                                    }
                                }
                                _ => json!({ "error": "Unknown method" }),
                            };
                            let response_str = response.to_string();
                            stream.writable().await.unwrap();
                            stream.try_write(response_str.as_bytes()).unwrap();
                        }
                        Err(e) => {
                            log::error!("Connection failed {e}")
                        }
                }
                }
                _ = self.shutdown_listener.cancelled() => {
                        break
                }
            }
        }
    }
}
