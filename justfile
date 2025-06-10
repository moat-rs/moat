

buf-version := '1.54.0'
bin := x'~/.local/bin'
kernel := `uname -s`
machine := `uname -m`


all: misc check ffmt udeps

ci: misc check-ci ffmt-ci udeps

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

udeps:
	cargo machete

buf-install:
    curl -sSL \
    "https://github.com/bufbuild/buf/releases/download/v{{buf-version}}/buf-{{kernel}}-{{machine}}" \
    -o "{{bin}}/buf"
    chmod +x "{{bin}}/buf"

buf-uninstall:
    rm -f "{{bin}}/buf"

dev:
    docker compose up --build

