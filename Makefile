BINARY := tacet
CARGO := cargo
WEB_DIR := web

.PHONY: all build web-install web-build web-test web-check check fmt fmt-check lint test clean

all: build

web-install:
	cd $(WEB_DIR) && npm ci

web-build:
	cd $(WEB_DIR) && npm run build

web-test:
	cd $(WEB_DIR) && npm test

web-check:
	cd $(WEB_DIR) && npm run check

build: web-build
	$(CARGO) build --release
	cp target/release/$(BINARY) $(BINARY)

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

lint: web-build
	$(CARGO) clippy --all-targets --locked -- -D warnings

test: web-build
	$(CARGO) test --locked

check: web-check fmt-check lint test

clean:
	$(CARGO) clean
	rm -rf $(BINARY) dist/ web/dist/*
	mkdir -p web/dist && touch web/dist/.gitkeep
