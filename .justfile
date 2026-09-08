build:
  cargo build

test:
  cargo test
  cargo test --no-default-features

check:
  cargo fmt --check
  cargo clippy --all-targets -- -D warnings
  cargo clippy --all-targets --no-default-features -- -D warnings
  just test

serve *ARGS:
  cargo run -- serve {{ARGS}}

up:
  podman-compose up -d --build

up-observability:
  podman-compose --profile observability up -d --build
