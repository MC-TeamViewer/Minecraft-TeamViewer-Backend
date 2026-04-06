import logging
import tomllib
from pathlib import Path
from typing import Optional


logger = logging.getLogger("teamviewrelay.state")


def load_toml_file(path: Path, label: str) -> dict:
    if not path.exists():
        logger.warning("%s file not found, fallback to defaults: %s", label, path)
        return {}

    try:
        with path.open("rb") as fp:
            data = tomllib.load(fp)
        return data if isinstance(data, dict) else {}
    except Exception as exc:
        logger.warning("Failed to load %s file %s, fallback to defaults: %s", label, path, exc)
        return {}


def parse_battle_chunk_symbol_config(config: dict) -> dict[str, str]:
    markers = config.get("markers") if isinstance(config, dict) else None
    if not isinstance(markers, dict):
        return {}

    parsed: dict[str, str] = {}
    for marker_type, symbols in markers.items():
        normalized_marker_type = str(marker_type or "").strip()
        if not normalized_marker_type or not isinstance(symbols, list):
            continue
        for raw_symbol in symbols:
            symbol = str(raw_symbol or "").strip()
            if not symbol:
                continue
            parsed[symbol] = normalized_marker_type
    return parsed


def coerce_int(value, default: int, min_value: int, max_value: int) -> int:
    if not isinstance(value, (int, float)):
        return default
    coerced = int(value)
    if coerced < min_value:
        return min_value
    if coerced > max_value:
        return max_value
    return coerced


def coerce_float(value, default: float, min_value: float, max_value: float) -> float:
    if not isinstance(value, (int, float)):
        return default
    coerced = float(value)
    if coerced < min_value:
        return min_value
    if coerced > max_value:
        return max_value
    return coerced


def coerce_bool(value, default: bool) -> bool:
    if isinstance(value, bool):
        return value
    return default


def parse_congestion_levels(raw_levels) -> list[tuple[int, float]]:
    if not isinstance(raw_levels, list):
        return []

    levels: list[tuple[int, float]] = []
    for item in raw_levels:
        if not isinstance(item, (list, tuple)) or len(item) != 2:
            continue
        threshold, hz = item[0], item[1]
        if not isinstance(threshold, (int, float)) or not isinstance(hz, (int, float)):
            continue
        threshold_int = max(1, int(threshold))
        hz_float = max(0.5, float(hz))
        levels.append((threshold_int, hz_float))

    levels.sort(key=lambda pair: pair[0], reverse=True)
    return levels


def normalize_tab_uuid(value) -> Optional[str]:
    text = str(value or "").strip().lower()
    if len(text) != 36:
        return None
    return text


def normalize_tab_name(value) -> Optional[str]:
    if not isinstance(value, str):
        return None
    text = value.strip()
    if not text:
        return None
    return text[:64]


def normalize_room_code(value, default_room_code: str) -> str:
    if isinstance(value, str):
        text = value.strip()
        if text:
            return text[:64]
    return default_room_code
