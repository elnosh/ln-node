use clap::{Parser, Subcommand};
use serde_json::json;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::usize;

#[derive(Parser)]
#[command()]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    NodePubkey,
    NewAddress,
    Balance,
    OpenChannel { pubkey: String, amount: u64 },
    ListChannels,
    CloseChannel { channel_id: String },
    SendPayment { invoice: String },
    Receive { amount: u64 },
    Connect { pubkey: String, address: String },
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::NodePubkey => node_pubkey(),
        Commands::NewAddress => new_address(),
        Commands::Balance => balance(),
        Commands::OpenChannel { pubkey, amount } => open_channel(pubkey, amount),
        Commands::ListChannels => list_channels(),
        Commands::CloseChannel { channel_id } => close_channel(channel_id),
        Commands::SendPayment { invoice } => send_payment(invoice),
        Commands::Receive { amount } => receive(amount),
        Commands::Connect { pubkey, address } => connect_peer(pubkey, address),
    }
}

const BUFFER_SIZE: usize = 4096;

fn node_pubkey() {
    let mut stream = UnixStream::connect("/tmp/ldk_node.sock").unwrap();
    let request = json!({"method": "node_pubkey"});
    stream.write_all(request.to_string().as_bytes()).unwrap();

    let mut buffer = [0; 1024];
    let n = stream.read(&mut buffer).unwrap();
    let response: serde_json::Value = serde_json::from_slice(&buffer[..n]).unwrap();
    println!("{}", response);
}

fn new_address() {
    let mut stream = UnixStream::connect("/tmp/ldk_node.sock").unwrap();
    let request = json!({"method": "new_address"});
    stream.write_all(request.to_string().as_bytes()).unwrap();

    let mut buffer = [0; 1024];
    let n = stream.read(&mut buffer).unwrap();
    let response: serde_json::Value = serde_json::from_slice(&buffer[..n]).unwrap();
    println!("{}", response);
}

fn balance() {
    let mut stream = UnixStream::connect("/tmp/ldk_node.sock").unwrap();
    let request = json!({"method": "balance"});
    stream.write_all(request.to_string().as_bytes()).unwrap();

    let mut buffer = [0; 1024];
    let n = stream.read(&mut buffer).unwrap();
    let response: serde_json::Value = serde_json::from_slice(&buffer[..n]).unwrap();
    println!("{}", response);
}

fn open_channel(pubkey: String, amount: u64) {
    let mut stream = UnixStream::connect("/tmp/ldk_node.sock").unwrap();
    let request = json!({
        "method": "open_channel",
        "params": {
            "pubkey": pubkey,
            "amount": amount
        }
    });
    stream.write_all(request.to_string().as_bytes()).unwrap();

    let mut buffer = [0; BUFFER_SIZE];
    let n = stream.read(&mut buffer).unwrap();
    let response: serde_json::Value = serde_json::from_slice(&buffer[..n]).unwrap();
    println!("{}", response);
}

fn list_channels() {
    let mut stream = UnixStream::connect("/tmp/ldk_node.sock").unwrap();
    let request = json!({"method": "list_channels"});
    stream.write_all(request.to_string().as_bytes()).unwrap();

    let mut buffer = [0; BUFFER_SIZE];
    let n = stream.read(&mut buffer).unwrap();
    let response: serde_json::Value = serde_json::from_slice(&buffer[..n]).unwrap();
    println!("{}", response);
}

fn close_channel(channel_id: String) {
    let mut stream = UnixStream::connect("/tmp/ldk_node.sock").unwrap();
    let request = json!({
        "method": "close_channel",
        "params": {
            "channel_id": channel_id
        }
    });
    stream.write_all(request.to_string().as_bytes()).unwrap();

    let mut buffer = [0; BUFFER_SIZE];
    let n = stream.read(&mut buffer).unwrap();
    let response: serde_json::Value = serde_json::from_slice(&buffer[..n]).unwrap();
    println!("{}", response);
}

fn send_payment(invoice: String) {
    let mut stream = UnixStream::connect("/tmp/ldk_node.sock").unwrap();
    let request = json!({
        "method": "send_payment",
        "params": {
            "invoice": invoice
        }
    });
    stream.write_all(request.to_string().as_bytes()).unwrap();

    let mut buffer = [0; BUFFER_SIZE];
    let n = stream.read(&mut buffer).unwrap();
    let response: serde_json::Value = serde_json::from_slice(&buffer[..n]).unwrap();
    println!("{}", response);
}

fn receive(amount: u64) {
    let mut stream = UnixStream::connect("/tmp/ldk_node.sock").unwrap();
    let request = json!({
        "method": "receive",
        "params": {
            "amount": amount * 1000
        }
    });
    stream.write_all(request.to_string().as_bytes()).unwrap();

    let mut buffer = [0; BUFFER_SIZE];
    let n = stream.read(&mut buffer).unwrap();
    let response: serde_json::Value = serde_json::from_slice(&buffer[..n]).unwrap();
    println!("{}", response);
}

fn connect_peer(pubkey: String, address: String) {
    let mut stream = UnixStream::connect("/tmp/ldk_node.sock").unwrap();
    let request = json!({
        "method": "connect",
        "params": {
            "pubkey": pubkey,
            "address": address,
        }
    });
    stream.write_all(request.to_string().as_bytes()).unwrap();

    let mut buffer = [0; BUFFER_SIZE];
    let n = stream.read(&mut buffer).unwrap();
    let response: serde_json::Value = serde_json::from_slice(&buffer[..n]).unwrap();
    println!("{}", response);
}
