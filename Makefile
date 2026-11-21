IMAGE ?= shopping-sessions:latest
CPUS  ?= 8
MEM   ?= 24g
PLATFORM ?= linux/amd64
DEV   ?= dev
ANSWER ?= dev-answer
PY    ?= .venv/bin/python
BIN   ?= target/release/shopping-sessions
export IMAGE CPUS MEM PLATFORM

.PHONY: build native test fixtures fixtures-native dev run-dev run-dev-native check-dev fuzz oracle invariants all-checks

## Docker image for the scorer's platform
build:
	docker buildx build --platform $(PLATFORM) --load -t $(IMAGE) .

## Native release binary
native:
	cargo build --release

test:
	cargo test --release

## 36 fixtures in the container with scorer flags
fixtures: build
	$(PY) scripts/run_fixtures.py fixtures -- scripts/docker_run.sh

fixtures-native: native
	$(PY) scripts/run_fixtures.py fixtures -- $(BIN)

## 1M-event dev set with the official generator (tools/generate, built from generator.zip)
dev:
	tools/generate --seed 1 --events 1000000 --clicks 250000 --output $(DEV)

run-dev: build
	rm -rf out && time scripts/docker_run.sh $(DEV) out

run-dev-native: native
	rm -rf out && time DATA_DIR=$(DEV) OUTPUT_DIR=out $(BIN)

check-dev:
	$(PY) scripts/compare.py out $(ANSWER)

## differential fuzz vs the Python oracle: make fuzz N=2000 SEED=3
N ?= 500
SEED ?= 1
fuzz: native
	$(PY) tests/fuzz.py $(BIN) $(N) $(SEED)

## the oracle alone on fixtures
oracle:
	$(PY) scripts/run_fixtures.py fixtures -- $(PY) reference/reference.py

invariants:
	$(PY) tests/invariants.py out $(DEV)

## everything to run before a (rate-limited) submission
all-checks: test fixtures-native run-dev-native check-dev invariants fuzz
