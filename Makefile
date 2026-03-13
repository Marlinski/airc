.PHONY: build aircd airc dev test clean

# Build everything
build:
	cargo build

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
