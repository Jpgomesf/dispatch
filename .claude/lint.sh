#!/usr/bin/env bash
# Pre-commit lint: rustfmt + clippy via the Makefile.
set -euo pipefail
make lint
