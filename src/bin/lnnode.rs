use env_logger::Env;
use lightning_node::{config::Config, node::NodeBuilder, server::UnixSocketServer};
use log::LevelFilter;
use std::{error::Error, fs, sync::Arc};
use tokio::signal;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("debug"))
        .filter_module("bitcoincore_rpc", LevelFilter::Off)
        .init();
    let node_builder = NodeBuilder::new(Config::default());

    let node = node_builder.build()?;
    let node = Arc::new(node);
    let node_server = Arc::clone(&node);

    let shutdown_token = CancellationToken::new();
    let server_shutdown = shutdown_token.clone();
    let socket_server =
        UnixSocketServer::new("/tmp/ldk_node.sock".into(), node_server, server_shutdown).unwrap();

    let running_node = Arc::clone(&node);
    let node_handle = tokio::spawn(async move {
        running_node.start().await.unwrap();
    });

    let server_handle = tokio::spawn(async move {
        socket_server.start_server().await;
    });

    match signal::ctrl_c().await {
        Ok(_) => {
            log::info!("Starting shutdown");
            node.shutdown();
            shutdown_token.cancel();
        }
        Err(_) => {
            log::error!("Could not listen to shutdown signal.")
        }
    }

    let _ = node_handle.await?;
    server_handle.abort();

    fs::remove_file("/tmp/ldk_node.sock").unwrap();

    log::info!("Finished shutdown");

    Ok(())
}
