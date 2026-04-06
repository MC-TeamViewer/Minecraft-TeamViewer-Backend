from __future__ import annotations

import re


def normalize_protocol_version(version: str | int | float | None) -> str:
    text = str(version or "").strip()
    return text or "0.0.0"


def parse_protocol_version(version: str | int | float | None) -> tuple[int, int, int]:
    text = normalize_protocol_version(version)
    core = text.split("-", 1)[0]
    tokens = core.split(".") if "." in core else [core]
    parsed: list[int] = []

    for token in tokens[:3]:
        match = re.match(r"^(\d+)", token.strip())
        parsed.append(int(match.group(1)) if match else 0)

    while len(parsed) < 3:
        parsed.append(0)

    return parsed[0], parsed[1], parsed[2]


def protocol_at_least(current: str | int | float | None, minimum: str | int | float | None) -> bool:
    return parse_protocol_version(current) >= parse_protocol_version(minimum)
