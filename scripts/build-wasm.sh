#!/bin/bash
set -e

GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"
PROJECT_ROOT="$SCRIPT_DIR/.."

RELEASE_FLAG=""
BUILD_MODE="debug"
if [[ "$1" == "--release" ]]; then
    RELEASE_FLAG="--release"
    BUILD_MODE="release"
    echo -e "${YELLOW}Building in release mode${NC}"
fi

echo -e "${GREEN}Step 1: cargo build --target wasm32-unknown-unknown ${RELEASE_FLAG}${NC}"
cd "$PROJECT_ROOT"
cargo build -p client --lib --target wasm32-unknown-unknown $RELEASE_FLAG

WASM_PATH="$PROJECT_ROOT/target/wasm32-unknown-unknown/$BUILD_MODE/client.wasm"
if [[ ! -f "$WASM_PATH" ]]; then
    echo -e "${RED}Error: WASM file not found at: $WASM_PATH${NC}"
    exit 1
fi

echo -e "${GREEN}Step 2: wasm-bindgen${NC}"
mkdir -p "$PROJECT_ROOT/www/src/wasm"
wasm-bindgen "$WASM_PATH" \
    --out-dir "$PROJECT_ROOT/www/src/wasm" \
    --target web \
    --no-typescript

echo -e "${GREEN}Build done.${NC}"
