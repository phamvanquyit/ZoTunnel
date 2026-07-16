.PHONY: build build-server build-client build-all clean test run-server run-client docker docker-up docker-down

build: build-all

build-server:
	cargo build --release -p zo-tunnel-server

build-client:
	cargo build --release -p zo-tunnel-client

build-all:
	cargo build --release

test:
	cargo test --workspace

run-server:
	RUST_LOG=info cargo run -p zo-tunnel-server -- start --domain localhost --force

run-client:
	RUST_LOG=info cargo run -p zo-tunnel-client -- http 3000 --name my-app

docker:
	docker compose build

docker-up:
	docker compose up -d --build

docker-down:
	docker compose down

clean:
	cargo clean

cross-linux-amd64:
	cross build --release --target x86_64-unknown-linux-gnu -p zo-tunnel-client

cross-linux-arm64:
	cross build --release --target aarch64-unknown-linux-gnu -p zo-tunnel-client

cross-macos-arm64:
	cross build --release --target aarch64-apple-darwin -p zo-tunnel-client

cross-all: cross-linux-amd64 cross-linux-arm64
