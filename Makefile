SHELL := /bin/bash
.PHONY: ffmt udeps all check misc

misc:
	typos

ffmt:
	cargo +nightly fmt --all -- --config-path rustfmt.nightly.toml

udeps:
	cargo machete

check:
	cargo sort -w
	taplo fmt
	cargo fmt --all
	cargo clippy --all-targets

all: misc check ffmt udeps
