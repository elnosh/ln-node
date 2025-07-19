use env_logger::Env;
use lightning_node::{config::Config, node::NodeBuilder, server::UnixSocketServer};
use log::LevelFilter;
use std::{error::Error, fs, sync::Arc};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("debug"))
        .filter_module("bitcoincore_rpc", LevelFilter::Off)
        .init();
    let node_builder = NodeBuilder::new(Config::default());

    let node = node_builder.build()?;
    let node = Arc::new(node);
    let node_server = Arc::clone(&node);
    let socket_server = UnixSocketServer::new("/tmp/ldk_node.sock".into(), node_server).unwrap();

    // TODO: proper shutdown

    let mut tasks = vec![];
    tasks.push(tokio::spawn(async move {
        node.start().await.unwrap();
    }));

    tasks.push(tokio::spawn(async move {
        socket_server.start_server();
    }));

    for task in tasks {
        task.await?
    }

    fs::remove_file("/tmp/ldk_node.sock").unwrap();

    Ok(())
}
