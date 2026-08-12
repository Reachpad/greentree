# greentree task runner

default: check

build:
    cargo build

clippy:
    cargo clippy --all-targets -- -D warnings

fmt:
    cargo fmt --all --check

test:
    cargo nextest run

# The aggregate gate: run before every push.
check: build clippy fmt test

demo:
    bash docs/demo.sh
