#!/usr/bin/env bash
# Pre-commit lint: ruff only until the package exists, then the full make target.
set -euo pipefail
if [ -f pyproject.toml ]; then make lint; else ruff check .; fi
