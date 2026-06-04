.PHONY: help dev build-wasm build-wasm-dev build-web build clean install

# wasm-bindgen-cli MUST match the wasm-bindgen crate version pulled by
# Cargo.lock. Drift here = the JS bindings reference symbols the WASM blob
# doesn't export. The CI workflow pins the same value; bump together.
WASM_BINDGEN_VERSION := 0.2.120

help: ## Show help information
	@echo "btc-orderflow - Available commands:"
	@echo ""
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-15s\033[0m %s\n", $$1, $$2}'

install: ## Install all dependencies
	@echo "Installing wasm-bindgen-cli@$(WASM_BINDGEN_VERSION)..."
	@cargo install --locked wasm-bindgen-cli --version $(WASM_BINDGEN_VERSION) || true
	@echo "Installing frontend dependencies..."
	@cd www && bun install

build-wasm: ## Build WASM (release mode)
	@./scripts/build-wasm.sh --release

build-wasm-dev: ## Build WASM (debug mode)
	@./scripts/build-wasm.sh

build-web: ## Build frontend
	@cd www && bun run build

build: build-wasm build-web ## Build complete project (WASM + frontend)

dev: build-wasm-dev ## Start development server (WASM + Vite at localhost:3001)
	@cd www && bun install && bun run dev

clean: ## Clean build artifacts
	@echo "Cleaning build artifacts..."
	@rm -rf www/dist
	@rm -rf www/src/wasm/*.js www/src/wasm/*.wasm
	@cargo clean
