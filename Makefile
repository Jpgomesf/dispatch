.PHONY: lint typecheck test check

lint:
	uv run --extra dev ruff check .
	uv run --extra dev ruff format --check .

typecheck:
	uv run --extra dev mypy

test:
	uv run --extra dev pytest -q

check: lint typecheck test
