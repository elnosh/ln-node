use std::path::PathBuf;

use bitcoin::Network;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub bitcoind_rpc_host: String,
    pub bitcoind_rpc_port: u16,
    pub bitcoind_username: String,
    pub bitcoind_password: String,
    #[serde(default)]
    pub node_path: PathBuf,
    #[serde(default = "default_network")]
    pub network: Network,
    #[serde(default)]
    pub min_channel_value_sat: u64,
    #[serde(default)]
    pub default_channel_public: bool,
    #[serde(default)]
    pub max_pending_htlcs: u16,
}

impl Default for Config {
    fn default() -> Self {
        let mut default_node_dir = home::home_dir().unwrap();
        default_node_dir.push(".lightning-node");
        Self {
            bitcoind_rpc_host: "127.0.0.1".to_string(),
            bitcoind_rpc_port: 18443,
            bitcoind_username: "user".to_string(),
            bitcoind_password: "password".to_string(),
            node_path: default_node_dir,
            network: Network::Regtest,
            min_channel_value_sat: 100_000,
            default_channel_public: true,
            max_pending_htlcs: 50,
        }
    }
}

fn default_network() -> Network {
    Network::Regtest
}
