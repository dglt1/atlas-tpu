#!/bin/bash

# Load Cargo environment
. "$HOME/.cargo/env"

# Environment variables
export IDENTITY_KEYPAIR_FILE=/home/solana/atlas-tpu/atlas-keypair.json
export GRPC_URL=http://localhost:10001
export RPC_URL=http://localhost:8899
export PORT=4040
export TPU_CONNECTION_POOL_SIZE=1
#export X_TOKEN=
export TXN_SENDER_THREADS=1500
export MAX_TXN_SEND_RETRIES=3
export TXN_SEND_RETRY_INTERVAL=1
export MAX_RETRY_QUEUE_SIZE=4000

# Logging and metrics
export RUST_LOG=info
export METRICS_URI=localhost
export METRICS_PORT=7998

# Run the application
cargo run --release