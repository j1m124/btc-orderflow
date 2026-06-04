.PHONY: help dev build build-wasm build-wasm-dev build-web check check-client check-protocol check-server server clean install

# wasm-bindgen-cli MUST match the wasm-bindgen crate version pulled by
# Cargo.lock. Drift here = the JS bindings reference symbols the WASM blob
# doesn't export. The CI workflow pins the same value; bump together.
WASM_BINDGEN_VERSION := 0.2.120

help: ## Show help information
	@echo "btc-orderflow - Available commands:"
	@echo ""
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2}'

install: ## Install all dependencies
	@echo "Installing wasm-bindgen-cli@$(WASM_BINDGEN_VERSION)..."
	@cargo install --locked wasm-bindgen-cli --version $(WASM_BINDGEN_VERSION) || true
	@echo "Installing frontend dependencies..."
	@cd www && bun install

# --- Type-checking ---------------------------------------------------------
#
# .cargo/config.toml no longer sets a default target. Each crate is checked
# against its actual target: protocol is no-deps so the host works; the wasm
# client needs --target wasm32-unknown-unknown; the server is host-native.

check: check-protocol check-client check-server ## Type-check every crate against its target

check-protocol: ## Type-check the shared wire-protocol crate
	@cargo check -p btc_orderflow_protocol

check-client: ## Type-check the wasm client crate
	@cargo check -p btc_orderflow --target wasm32-unknown-unknown

check-server: ## Type-check the native server crate
	@cargo check -p btc_orderflow_server

# --- Run server ------------------------------------------------------------

server: ## Run the native server (cargo run -p btc_orderflow_server)
	@cargo run -p btc_orderflow_server

# --- Frontend build --------------------------------------------------------

build-wasm: ## Build WASM (release mode)
	@./scripts/build-wasm.sh --release

build-wasm-dev: ## Build WASM (debug mode)
	@./scripts/build-wasm.sh

build-web: ## Build frontend
	@cd www && bun run build

build: build-wasm build-web ## Build complete project (WASM + frontend)

dev: build-wasm-dev ## Start client dev server (WASM + Vite at localhost:3001)
	@cd www && bun install && bun run dev

clean: ## Clean build artifacts
	@echo "Cleaning build artifacts..."
	@rm -rf www/dist
	@rm -rf www/src/wasm/*.js www/src/wasm/*.wasm
	@cargo clean
