import hashlib
import json
import math
from typing import Dict, Optional

from pydantic import ValidationError


def compact_state_map(state_map: Dict[str, dict]) -> Dict[str, dict]:
    return {sid: node.get("data", {}) for sid, node in state_map.items()}


def prune_none_fields(value):
    if isinstance(value, dict):
        return {
            key: prune_none_fields(item)
            for key, item in value.items()
            if item is not None
        }
    if isinstance(value, list):
        return [prune_none_fields(item) for item in value]
    return value


def canonical_number(value: float) -> str:
    if not math.isfinite(value):
        return "null"
    rounded = round(float(value), 6)
    text = f"{rounded:.6f}".rstrip("0").rstrip(".")
    if text in ("", "-0"):
        return "0"
    return text


def canonical_value(value) -> str:
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, int):
        return str(value)
    if isinstance(value, float):
        return canonical_number(value)
    if isinstance(value, str):
        return json.dumps(value, ensure_ascii=False, separators=(",", ":"))
    if isinstance(value, dict):
        items = []
        for key in sorted(value.keys(), key=lambda item: str(item)):
            key_json = json.dumps(str(key), ensure_ascii=False, separators=(",", ":"))
            items.append(f"{key_json}:{canonical_value(value[key])}")
        return "{" + ",".join(items) + "}"
    if isinstance(value, list):
        return "[" + ",".join(canonical_value(item) for item in value) + "]"

    try:
        return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
    except TypeError:
        return json.dumps(str(value), ensure_ascii=False, separators=(",", ":"))


def state_digest(state_map: Dict[str, dict]) -> str:
    lines = []
    for node_id in sorted(state_map.keys()):
        node = state_map.get(node_id, {})
        data = node.get("data", {}) if isinstance(node, dict) else {}
        node_json = json.dumps(str(node_id), ensure_ascii=False, separators=(",", ":"))
        lines.append(f"{node_json}:{canonical_value(data)}")

    raw = "\n".join(lines)
    return hashlib.sha1(raw.encode("utf-8")).hexdigest()[:16]


def state_digest_plain(state_map: Dict[str, dict]) -> str:
    lines = []
    for node_id in sorted(state_map.keys()):
        data = state_map.get(node_id, {})
        if not isinstance(data, dict):
            data = {}
        node_json = json.dumps(str(node_id), ensure_ascii=False, separators=(",", ":"))
        lines.append(f"{node_json}:{canonical_value(data)}")

    raw = "".join(f"{line}\n" for line in lines)
    return hashlib.sha1(raw.encode("utf-8")).hexdigest()[:16]


def make_empty_patch() -> dict:
    return {
        "players": {"upsert": {}, "delete": []},
        "entities": {"upsert": {}, "delete": []},
        "waypoints": {"upsert": {}, "delete": []},
        "battleChunks": {"upsert": {}, "delete": []},
    }


def has_patch_changes(patch: dict) -> bool:
    for scope in ("players", "entities", "waypoints", "battleChunks"):
        if patch[scope]["upsert"] or patch[scope]["delete"]:
            return True
    return False


def merge_patch(base: dict, extra: dict) -> None:
    for scope in ("players", "entities", "waypoints", "battleChunks"):
        base[scope]["upsert"].update(extra[scope]["upsert"])
        base[scope]["delete"].extend(extra[scope]["delete"])


def compute_field_delta(old_data: Optional[dict], new_data: dict) -> dict:
    if old_data is None:
        return dict(new_data)

    delta = {}
    for key, value in new_data.items():
        if old_data.get(key) != value:
            delta[key] = value
    return delta


def merge_patch_and_validate(model_cls, existing_node: Optional[dict], patch_data: dict) -> dict:
    merged = {}
    if existing_node and isinstance(existing_node.get("data"), dict):
        merged.update(existing_node["data"])
    if isinstance(patch_data, dict):
        merged.update(patch_data)
    validated = model_cls(**merged)
    return validated.model_dump()


def payload_preview(payload, limit: int = 320) -> str:
    try:
        text = json.dumps(payload, ensure_ascii=False, separators=(",", ":"))
    except Exception:
        text = str(payload)
    if len(text) <= limit:
        return text
    return text[:]


def missing_fields_from_validation_error(error: ValidationError) -> list:
    fields = []
    for item in error.errors():
        if item.get("type") != "missing":
            continue
        loc = item.get("loc")
        if isinstance(loc, (list, tuple)) and loc:
            fields.append(".".join(str(part) for part in loc))
        elif loc is not None:
            fields.append(str(loc))
    return fields


def build_state_node(submit_player_id: Optional[str], current_time: float, normalized: dict) -> dict:
    return {
        "timestamp": current_time,
        "submitPlayerId": submit_player_id,
        "data": normalized,
    }


def upsert_report(report_map: Dict[str, Dict[str, dict]], object_id: str, source_id: Optional[str], node: dict) -> None:
    source_key = source_id if isinstance(source_id, str) else ""
    source_bucket = report_map.setdefault(object_id, {})
    source_bucket[source_key] = node


def delete_report(report_map: Dict[str, Dict[str, dict]], object_id: str, source_id: Optional[str]) -> bool:
    if object_id not in report_map:
        return False

    source_bucket = report_map[object_id]
    source_key = source_id if isinstance(source_id, str) else ""
    if source_key not in source_bucket:
        return False

    del source_bucket[source_key]
    if not source_bucket:
        del report_map[object_id]
    return True


def touch_reports(
    report_map: Dict[str, Dict[str, dict]],
    object_ids: list[str],
    source_id: Optional[str],
    current_time: float,
) -> int:
    if not isinstance(object_ids, list) or not object_ids:
        return 0

    source_key = source_id if isinstance(source_id, str) else ""
    touched = 0
    for object_id in object_ids:
        if not isinstance(object_id, str) or not object_id:
            continue
        source_bucket = report_map.get(object_id)
        if not isinstance(source_bucket, dict):
            continue
        node = source_bucket.get(source_key)
        if not isinstance(node, dict):
            continue
        node["timestamp"] = float(current_time)
        touched += 1

    return touched


def node_timestamp(node: Optional[dict]) -> float:
    if not isinstance(node, dict):
        return 0.0
    value = node.get("timestamp")
    if not isinstance(value, (int, float)):
        return 0.0
    return float(value)


def resolve_report_map(
    report_map: Dict[str, Dict[str, dict]],
    selected_sources: Dict[str, str],
    switch_threshold_sec: float,
    *,
    prefer_object_id_source: bool = False,
) -> Dict[str, dict]:
    resolved: Dict[str, dict] = {}
    next_selected_sources: Dict[str, str] = {}

    for object_id, source_bucket in report_map.items():
        if not isinstance(source_bucket, dict) or not source_bucket:
            continue

        valid_bucket: Dict[str, dict] = {
            source_id: node
            for source_id, node in source_bucket.items()
            if isinstance(node, dict)
        }
        if not valid_bucket:
            continue

        best_source_id = None
        best_node = None
        best_timestamp = float("-inf")
        for source_id, node in valid_bucket.items():
            timestamp_value = node_timestamp(node)

            if timestamp_value > best_timestamp:
                best_source_id = source_id
                best_node = node
                best_timestamp = timestamp_value
                continue

            if timestamp_value == best_timestamp:
                current_best_key = str(best_source_id) if best_source_id is not None else ""
                current_key = str(source_id)
                if current_key < current_best_key:
                    best_source_id = source_id
                    best_node = node

        chosen_source_id = best_source_id
        chosen_node = best_node

        preferred_source = str(object_id) if prefer_object_id_source else None
        if preferred_source and preferred_source in valid_bucket:
            preferred_node = valid_bucket[preferred_source]
            preferred_ts = node_timestamp(preferred_node)
            if best_timestamp - preferred_ts <= switch_threshold_sec:
                chosen_source_id = preferred_source
                chosen_node = preferred_node

        previous_source = selected_sources.get(object_id)
        if previous_source in valid_bucket:
            previous_node = valid_bucket[previous_source]
            previous_ts = node_timestamp(previous_node)
            chosen_ts = node_timestamp(chosen_node)
            if chosen_ts - previous_ts <= switch_threshold_sec:
                chosen_source_id = previous_source
                chosen_node = previous_node

        if chosen_node is not None and chosen_source_id is not None:
            resolved[object_id] = chosen_node
            next_selected_sources[object_id] = chosen_source_id

    selected_sources.clear()
    selected_sources.update(next_selected_sources)
    return resolved


def compute_scope_patch(old_map: Dict[str, dict], new_map: Dict[str, dict], *, full_replace: bool = False) -> dict:
    scope_patch = {"upsert": {}, "delete": []}

    for object_id in old_map.keys() - new_map.keys():
        scope_patch["delete"].append(object_id)

    for object_id, new_node in new_map.items():
        old_node = old_map.get(object_id)
        old_data = old_node.get("data") if isinstance(old_node, dict) else None
        new_data = new_node.get("data") if isinstance(new_node, dict) else None
        if not isinstance(new_data, dict):
            new_data = {}
        if full_replace:
            delta = dict(new_data) if old_data != new_data else {}
        else:
            delta = compute_field_delta(old_data if isinstance(old_data, dict) else None, new_data)
        if delta:
            scope_patch["upsert"][object_id] = delta

    scope_patch["delete"].sort()
    return scope_patch
