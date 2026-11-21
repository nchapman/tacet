BINARY := tacet
CMD := ./cmd/tacet
GO := go
WEB_DIR := web

.PHONY: all build web-install web-build web-test web-check check fmt fmt-check vet lint test test-race cover build-linux-arm64 build-linux-amd64 clean

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
	$(GO) build -o $(BINARY) $(CMD)

build-linux-arm64: web-build
	CGO_ENABLED=0 GOOS=linux GOARCH=arm64 $(GO) build -trimpath -ldflags="-s -w" -o dist/$(BINARY)-linux-arm64 $(CMD)

build-linux-amd64: web-build
	CGO_ENABLED=0 GOOS=linux GOARCH=amd64 $(GO) build -trimpath -ldflags="-s -w" -o dist/$(BINARY)-linux-amd64 $(CMD)

fmt:
	gofmt -s -w .

fmt-check:
	@unformatted=$$(gofmt -l .); \
	if [ -n "$$unformatted" ]; then \
		echo "Files need gofmt:"; echo "$$unformatted"; exit 1; \
	fi

vet:
	$(GO) vet ./...

lint:
	golangci-lint run ./...

test: web-build
	$(GO) test ./...

test-race: web-build
	$(GO) test -race -count=1 ./...

cover: web-build
	$(GO) test -race -coverprofile=coverage.out ./...
	$(GO) tool cover -func=coverage.out

check: web-check fmt-check vet lint test-race

clean:
	rm -rf $(BINARY) dist/ coverage.out internal/api/web/dist/*
	touch internal/api/web/dist/.gitkeep
