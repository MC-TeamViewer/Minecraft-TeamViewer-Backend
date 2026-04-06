import hashlib


def build_battle_map_observation_hash(
    canonicalize,
    *,
    dimension: str,
    map_size: int,
    anchor_row: int,
    anchor_col: int,
    snapshot_observed_at: int,
    candidates: list[dict],
    cells: list[dict],
) -> str:
    raw = canonicalize(
        {
            "dimension": dimension,
            "mapSize": map_size,
            "anchorRow": anchor_row,
            "anchorCol": anchor_col,
            "snapshotObservedAt": snapshot_observed_at,
            "candidates": candidates,
            "cells": cells,
        }
    )
    return hashlib.sha1(raw.encode("utf-8")).hexdigest()[:16]


def build_battle_chunk_semantic_projection_hash(
    canonicalize,
    normalize_chunk,
    projected: dict[str, dict],
) -> str:
    normalized_projection = {
        chunk_id: normalize_chunk(chunk_data)
        for chunk_id, chunk_data in projected.items()
        if isinstance(chunk_id, str) and chunk_id and isinstance(chunk_data, dict)
    }
    raw = canonicalize(normalized_projection)
    return hashlib.sha1(raw.encode("utf-8")).hexdigest()[:16]
