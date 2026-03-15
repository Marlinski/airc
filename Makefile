.PHONY: build aircd airc dev test clean install

# Install dependencies
install:
	cd packages/airc-client-ts && npm install

# Build everything (Rust binaries + JS client)
build:
	cargo build
	cd packages/airc-client-ts && npm run build

# Build the server daemon
aircd:
	cargo build --bin aircd

# Build the CLI client (includes MCP server via `airc mcp`)
airc:
	cargo build --bin airc

# Start the server in dev mode (foreground)
dev:
	RUST_LOG=info cargo run --bin aircd -- start --foreground

# Run all tests
test:
	cargo test

# Clean build artifacts
clean:
	cargo clean
