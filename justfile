

buf-version := '1.54.0'
bin := x'~/.local/bin'
kernel := `uname -s`
machine := `uname -m`

all: misc ffmt check test udeps license

ci: misc ffmt-ci check-ci test udeps license

misc:
    typos

check:
	cargo sort -w
	taplo fmt
	cargo fmt --all
	cargo clippy --all-targets

check-ci:
    cargo sort -w -c
    taplo fmt --check
    cargo fmt --all --check
    cargo clippy --all-targets -- -D warnings

ffmt:
	cargo +nightly fmt --all -- --config-path rustfmt.nightly.toml

ffmt-ci:
	cargo +nightly fmt --all --check -- --config-path rustfmt.nightly.toml

test:
	cargo nextest run

udeps:
	cargo machete

license:
	license-eye header check

dev:
    docker compose up --build

