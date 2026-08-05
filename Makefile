# anvil Makefile
# Convenience wrapper for common development tasks

.PHONY: help build run test check fmt clippy security verify package support-bundle clean install dev watch benchmark debug

help:
	@echo "anvil Development Commands"
	@echo "==========================="
	@echo ""
	@echo "Build Commands:"
	@echo "  make build      - Build the optimized release binary"
	@echo "  make run        - Run in development mode"
	@echo "  make install    - Build and install to the current user account"
	@echo "  make package    - Build a relocatable release archive and checksum"
	@echo ""
	@echo "Quality Commands:"
	@echo "  make test       - Run all tests"
	@echo "  make check      - Check code without building"
	@echo "  make fmt        - Format code"
	@echo "  make clippy     - Run the repository lint policy"
	@echo "  make security   - Audit dependencies and shell scripts"
	@echo "  make verify     - Run the complete local quality gate"
	@echo ""
	@echo "Development:"
	@echo "  make dev        - Run the development helper"
	@echo "  make watch      - Watch for changes and rebuild"
	@echo "  make benchmark  - Run performance benchmarks"
	@echo "  make debug      - Show debug information"
	@echo "  make support-bundle - Create a privacy-preserving support archive"
	@echo ""
	@echo "Cleanup:"
	@echo "  make clean      - Clean build artifacts"
	@echo ""

build:
	@./scripts/dev.sh build

run:
	@./scripts/dev.sh run

test:
	@./scripts/dev.sh test

check:
	@./scripts/dev.sh check

fmt:
	@./scripts/dev.sh fmt

clippy:
	@./scripts/dev.sh clippy

security:
	@./scripts/dev.sh security

verify:
	@./scripts/dev.sh verify

package:
	@./scripts/dev.sh package

support-bundle:
	@./scripts/support-bundle.sh

clean:
	@./scripts/dev.sh clean

install:
	@./scripts/install.sh

dev:
	@./scripts/dev.sh

watch:
	@./scripts/dev.sh watch

benchmark:
	@./scripts/benchmark.sh

debug:
	@./scripts/debug.sh info

# Default target
.DEFAULT_GOAL := help
