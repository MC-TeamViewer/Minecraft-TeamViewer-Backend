FROM astral/uv:python3.12-bookworm-slim

WORKDIR /app

COPY pyproject.toml uv.lock README.md /app/
COPY src /app/src

RUN uv sync --frozen --no-dev

EXPOSE 8765

CMD ["uv", "run", "src/main.py"]
