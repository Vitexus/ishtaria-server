.DEFAULT_GOAL := run

CONFIG ?= etc/server.toml
HEIGHTMAP ?= ../ishtaria-worldgen/examples/planet-seed42-256.pgm
SEED ?= 42
DATABASE_URL_TEST ?= postgresql:///postgres?host=/var/run/postgresql

.PHONY: run import check fmt clippy test test-db build release help

## Start the server from local sources (uses the stored world; subsequent starts).
run:
	cargo run --locked -- "$(CONFIG)"

## Start the server with the example world import (first start of a new database).
import:
	cargo run --locked -- "$(CONFIG)" --import "$(HEIGHTMAP)" --seed $(SEED)

## All CI gates: formatting, lints and tests.
check: fmt clippy test

dbreset:
	sudo -u postgres psql -v ON_ERROR_STOP=1 -c "DROP DATABASE IF EXISTS ishtaria WITH (FORCE)" -c "CREATE DATABASE ishtaria OWNER ishtaria"

fmt:
	cargo fmt --check

clippy:
	cargo clippy --locked --all-targets -- -D warnings

test:
	cargo test --locked

## Tests including the PostgreSQL integration tests (SQLx makes a throw-away database; never point DATABASE_URL at a live one).
test-db:
	DATABASE_URL='$(DATABASE_URL_TEST)' \
	ISHTARIA_TEST_HEIGHTMAP="$(CURDIR)/$(HEIGHTMAP)" \
	cargo test --locked -- --include-ignored

build:
	cargo build --locked

release:
	cargo build --locked --release

help:
	@printf '%s\n' \
		'make [run]  Start the server from local sources (stored world)' \
		'make import  Start the server importing the example heightmap (first start)' \
		'make check   Run fmt, clippy and tests' \
		'make fmt     Check formatting' \
		'make clippy  Run clippy with -D warnings' \
		'make test    Run unit tests' \
		'make test-db Run tests including PostgreSQL integration tests' \
		'make build   Build the debug binary' \
		'make release Build the release binary' \
		'Overrides: CONFIG=etc/server.toml HEIGHTMAP=... SEED=42 DATABASE_URL_TEST=...'
