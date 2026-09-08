build:
  cargo build

test:
  cargo test

check:
  cargo fmt --check
  cargo clippy --all-targets -- -D warnings
  cargo test

serve *ARGS:
  cargo run -- serve {{ARGS}}
