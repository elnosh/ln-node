# AGENTS.md - Coding Agent Guidelines

Rust-based Lightning Network node built on [LDK](https://github.com/lightningdevkit/rust-lightning) with on-chain wallet management via [BDK](https://github.com/bitcoindevkit/bdk).

## Project Structure

```
src/
├── bin/
│   ├── lnnode.rs      # Main daemon binary
│   └── lncli.rs       # CLI tool binary
├── lib.rs             # Library exports
├── node.rs            # Core Node and NodeBuilder implementation
├── event_handler.rs   # LDK event handling
├── bitcoind_client.rs # Bitcoin Core RPC client
├── onchain_wallet.rs  # BDK wallet + LDK key management
├── storage.rs         # Persistence (payments, peers)
├── server.rs          # Unix socket server
├── config.rs          # Configuration structs
└── logger.rs          # Logging implementation
```

## Build Commands

```bash
cargo build              # Build the project
cargo build --release    # Build for release
cargo run --bin lnnode   # Run the node daemon
cargo run --bin lncli -- <subcommand>  # Run the CLI tool
cargo check              # Check compilation without building
```

## Testing

```bash
cargo test                      # Run all tests
cargo test test_name            # Run a single test by name
cargo test module_name::        # Run tests in a specific module
cargo test -- --nocapture       # Run tests with stdout output
RUST_BACKTRACE=1 cargo test     # Run with backtrace on failure
```

## Linting and Formatting

```bash
cargo fmt                              # Format all code
cargo fmt --check                      # Check formatting without modifying
cargo clippy                           # Run clippy linter
cargo clippy -- -D warnings            # Run clippy with warnings as errors
cargo clippy --all-targets -- -D warnings  # Run on all targets
```

## Code Style Guidelines

### Import Organization

Organize imports in three groups, separated by blank lines:

```rust
// 1. Standard library
use std::error::Error;
use std::sync::{Arc, Mutex};

// 2. External crates
use bitcoin::secp256k1::PublicKey;
use lightning::ln::channelmanager::ChannelManager;
use serde::{Deserialize, Serialize};

// 3. Local crate modules
use crate::bitcoind_client::BitcoindRpcClient;
use crate::config::Config;
```

### Naming Conventions

- **snake_case**: functions, variables, modules (`open_channel`, `node_id`)
- **PascalCase**: types, structs, enums, traits (`NodeBuilder`, `PaymentStatus`)
- **SCREAMING_SNAKE_CASE**: constants (`SEED_FILENAME`, `PAYMENTS_PRIMARY_NAMESPACE`)

### Type Aliases

Use type aliases for complex generic types:

```rust
type NetworkGraph = gossip::NetworkGraph<Arc<NodeLogger>>;
type Scorer = ProbabilisticScorer<Arc<NetworkGraph>, Arc<NodeLogger>>;
```

### Error Handling

1. Use `Result<T, Box<dyn Error>>` for fallible functions
2. Use `?` operator for error propagation
3. Use `map_err` for error context:
   ```rust
   NetworkGraph::read(&mut reader, logger.clone())
       .map_err(|e| format!("Failed to deserialize NetworkGraph: {e}"))?
   ```
4. Match on Result for complex error handling:
   ```rust
   match self.channel_manager.accept_inbound_channel(...) {
       Ok(_) => log::info!("Accepted channel..."),
       Err(e) => match e {
           APIError::APIMisuseError { err } => log::error!("Error: {}", err),
           _ => log::error!("Could not accept channel"),
       },
   }
   ```

### Struct Patterns

- Use the builder pattern for complex initialization (`NodeBuilder`)
- Implement `Default` for config structs
- Use `#[derive(Debug, Clone, Deserialize)]` for common traits

### Visibility

- `pub` for public API
- `pub(crate)` for crate-internal visibility
- Default to private (no modifier)

### Async Patterns

- Use `tokio` runtime with `#[tokio::main]`
- Use `Arc<T>` for shared ownership across async tasks
- Use `Arc<Mutex<T>>` for shared mutable state
- Use `CancellationToken` for graceful shutdown
- Use `tokio::spawn` for background tasks

### Comments

```rust
// TODO: check if payment is stored in db
// NOTE: For now assume here that we want a buffer 10000 sats per channel
// -------- Channel-related events --------
```

### Logging

Use the `log` crate with appropriate levels:
```rust
log::info!("Channel {} ready to be used", channel_id);
log::error!("Could not accept channel {}", err);
log::debug!("Got unknown payment hash {:?}", payment_hash);
```

## Key Dependencies

- `lightning` (v0.2) - Core LDK library
- `bdk_wallet` (v2.0.0) - Bitcoin Dev Kit wallet
- `bitcoin` (v0.32.6) - Bitcoin primitives
- `tokio` - Async runtime
- `serde` / `serde_json` - Serialization
- `clap` - CLI argument parsing

## Configuration

The node reads from `config.json`:
```json
{
  "bitcoind_rpc_host": "127.0.0.1",
  "bitcoind_rpc_port": 18443,
  "bitcoind_username": "user",
  "bitcoind_password": "password"
}
```

Default data directory: `~/.lightning-node/`
