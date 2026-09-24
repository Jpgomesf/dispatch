.PHONY: lint typecheck test check

lint:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings

typecheck:
	cargo check --all-targets

test:
	cargo test

check: lint typecheck test
