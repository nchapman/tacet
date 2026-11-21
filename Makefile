BINARY := tacet
CMD := ./cmd/tacet
GO := go

.PHONY: all build check fmt fmt-check vet lint test test-race cover build-linux-arm64 build-linux-amd64 clean

all: build

build:
	$(GO) build -o $(BINARY) $(CMD)

build-linux-arm64:
	CGO_ENABLED=0 GOOS=linux GOARCH=arm64 $(GO) build -trimpath -ldflags="-s -w" -o dist/$(BINARY)-linux-arm64 $(CMD)

build-linux-amd64:
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

test:
	$(GO) test ./...

test-race:
	$(GO) test -race -count=1 ./...

cover:
	$(GO) test -race -coverprofile=coverage.out ./...
	$(GO) tool cover -func=coverage.out

check: fmt-check vet lint test-race

clean:
	rm -rf $(BINARY) dist/ coverage.out
