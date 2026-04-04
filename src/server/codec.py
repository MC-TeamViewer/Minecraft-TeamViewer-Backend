from __future__ import annotations

from typing import Any, Protocol

from google.protobuf.descriptor import FieldDescriptor
from google.protobuf.message import Message

from .proto_generated.teamviewer.v1 import teamviewer_pb2
from .protocol import PacketDecodeError


class MessageCodec(Protocol):
    def decode(self, payload: bytes | bytearray | memoryview | str) -> dict[str, Any]: ...

    def encode(self, packet: dict[str, Any] | Any) -> bytes: ...


_WIRE_PAYLOADS: dict[str, tuple[str, type[Message]]] = {
    "player_handshake_request": ("handshake", teamviewer_pb2.PlayerHandshakeRequest),
    "web_map_handshake_request": ("handshake", teamviewer_pb2.WebMapHandshakeRequest),
    "admin_handshake_request": ("handshake", teamviewer_pb2.AdminHandshakeRequest),
    "ping": ("ping", teamviewer_pb2.Ping),
    "resync_request": ("resync_req", teamviewer_pb2.ResyncRequest),
    "handshake_ack": ("handshake_ack", teamviewer_pb2.HandshakeAck),
    "web_map_ack": ("web_map_ack", teamviewer_pb2.WebMapAck),
    "pong": ("pong", teamviewer_pb2.Pong),
    "snapshot_full": ("snapshot_full", teamviewer_pb2.SnapshotFull),
    "patch": ("patch", teamviewer_pb2.Patch),
    "digest": ("digest", teamviewer_pb2.Digest),
    "refresh_request": ("refresh_req", teamviewer_pb2.RefreshRequest),
    "report_rate_hint": ("report_rate_hint", teamviewer_pb2.ReportRateHint),
}

_PAYLOAD_TO_TYPE: dict[str, str] = {
    payload_name: packet_type
    for payload_name, (packet_type, _) in _WIRE_PAYLOADS.items()
}

_PAYLOAD_TO_MESSAGE: dict[str, type[Message]] = {
    payload_name: message_cls
    for payload_name, (_, message_cls) in _WIRE_PAYLOADS.items()
}

_TYPE_TO_PAYLOAD: dict[str, str] = {
    packet_type: payload_name
    for payload_name, (packet_type, _) in _WIRE_PAYLOADS.items()
    if packet_type != "handshake"
}

_WIRE_CHANNEL_TO_NAME: dict[int, str] = {
    teamviewer_pb2.WIRE_CHANNEL_PLAYER: "player",
    teamviewer_pb2.WIRE_CHANNEL_WEB_MAP: "web_map",
    teamviewer_pb2.WIRE_CHANNEL_ADMIN: "admin",
}

_WEB_MAP_COMMAND_TO_TYPE: dict[str, str] = {
    "resync_request": "resync_req",
    "set_player_mark": "command_player_mark_set",
    "clear_player_mark": "command_player_mark_clear",
    "clear_all_player_marks": "command_player_mark_clear_all",
    "set_same_server_filter": "command_same_server_filter_set",
    "set_tactical_waypoint": "command_tactical_waypoint_set",
    "delete_waypoints": "waypoints_delete",
}


def _normalize_field_key(name: str) -> str:
    return "".join(ch.lower() for ch in str(name or "") if ch.isalnum())


def _snake_to_camel(name: str) -> str:
    parts = [part for part in str(name or "").split("_") if part]
    if not parts:
        return name
    return parts[0] + "".join(part[:1].upper() + part[1:] for part in parts[1:])


def _message_to_value(value: Any) -> Any:
    if isinstance(value, Message):
        return _message_to_plain_dict(value)
    if isinstance(value, list):
        return [_message_to_value(item) for item in value]
    return value


def _battle_map_observation_to_plain_dict(message: Message) -> dict[str, Any]:
    return {
        "dimension": message.dimension,
        "mapSize": message.map_size,
        "anchorRow": message.anchor_row,
        "anchorCol": message.anchor_col,
        "snapshotObservedAt": message.snapshot_observed_at,
        "parsedAt": message.parsed_at,
        "candidates": [
            {
                "baseChunkX": candidate.base_chunk_x,
                "baseChunkZ": candidate.base_chunk_z,
                "positionSampledAt": candidate.position_sampled_at,
                "source": candidate.source,
            }
            for candidate in message.candidates
        ],
        "cells": [
            {
                "relChunkX": cell.rel_chunk_x,
                "relChunkZ": cell.rel_chunk_z,
                "symbol": cell.symbol if hasattr(cell, "symbol") else None,
                "colorRaw": cell.color_raw,
            }
            for cell in message.cells
        ],
    }


def _coerce_int(value: Any) -> int | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return int(value)
    if isinstance(value, float) and value.is_integer():
        return int(value)
    if isinstance(value, str):
        text = value.strip()
        if not text:
            return None
        try:
            return int(text)
        except ValueError:
            return None
    return None


def _split_battle_chunk_id(chunk_id: str) -> tuple[str, int, int] | None:
    if not isinstance(chunk_id, str):
        return None
    parts = chunk_id.rsplit("|", 3)
    if len(parts) == 3:
        dimension, chunk_x_raw, chunk_z_raw = parts
    elif len(parts) == 4:
        _, dimension, chunk_x_raw, chunk_z_raw = parts
    else:
        return None
    chunk_x = _coerce_int(chunk_x_raw)
    chunk_z = _coerce_int(chunk_z_raw)
    dimension_text = str(dimension or "").strip()
    if not dimension_text or chunk_x is None or chunk_z is None:
        return None
    return dimension_text, chunk_x, chunk_z


def _battle_chunk_synthetic_id(dimension: Any, chunk_x: Any, chunk_z: Any) -> str | None:
    dimension_text = str(dimension or "").strip()
    normalized_chunk_x = _coerce_int(chunk_x)
    normalized_chunk_z = _coerce_int(chunk_z)
    if not dimension_text or normalized_chunk_x is None or normalized_chunk_z is None:
        return None
    return f"{dimension_text}|{normalized_chunk_x}|{normalized_chunk_z}"


def _battle_chunk_ref_from_sources(chunk_id: str | None, data: dict[str, Any] | None = None) -> dict[str, Any] | None:
    payload = data if isinstance(data, dict) else {}
    dimension = str(payload.get("dimension") or "").strip()
    chunk_x = _coerce_int(payload.get("chunkX"))
    chunk_z = _coerce_int(payload.get("chunkZ"))
    if not dimension or chunk_x is None or chunk_z is None:
        parsed = _split_battle_chunk_id(chunk_id or "")
        if parsed is None:
            return None
        dimension, chunk_x, chunk_z = parsed
    return {
        "dimension": dimension,
        "coord": {
            "chunkX": chunk_x,
            "chunkZ": chunk_z,
        },
    }


def _battle_chunk_value_from_data(data: dict[str, Any] | None) -> dict[str, Any] | None:
    if not isinstance(data, dict):
        return None
    color_raw = data.get("colorRaw")
    if not isinstance(color_raw, str) or not color_raw:
        return None
    value: dict[str, Any] = {
        "colorRaw": color_raw,
    }
    for source_key, target_key in (
        ("symbol", "symbol"),
        ("markerType", "markerType"),
        ("colorNote", "colorNote"),
        ("observedAt", "observedAt"),
        ("positionSampledAt", "positionSampledAt"),
        ("alignmentSource", "alignmentSource"),
        ("reporterId", "reporterId"),
    ):
        raw = data.get(source_key)
        if raw is not None:
            value[target_key] = raw
    return value


def _battle_chunk_entry_to_local(entry: dict[str, Any]) -> tuple[str, dict[str, Any]] | None:
    if not isinstance(entry, dict):
        return None
    ref = entry.get("ref")
    if not isinstance(ref, dict):
        return None
    coord = ref.get("coord")
    if not isinstance(coord, dict):
        return None
    dimension = str(ref.get("dimension") or "").strip()
    chunk_x = _coerce_int(coord.get("chunkX"))
    chunk_z = _coerce_int(coord.get("chunkZ"))
    chunk_id = _battle_chunk_synthetic_id(dimension, chunk_x, chunk_z)
    if chunk_id is None:
        return None

    raw_value = entry.get("data")
    value = dict(raw_value) if isinstance(raw_value, dict) else {}
    value["dimension"] = dimension
    value["chunkX"] = chunk_x
    value["chunkZ"] = chunk_z
    return chunk_id, value


def _battle_chunk_entries_to_local_map(entries: Any) -> dict[str, Any]:
    if not isinstance(entries, list):
        return {}
    mapped: dict[str, Any] = {}
    for item in entries:
        local_entry = _battle_chunk_entry_to_local(item)
        if local_entry is None:
            continue
        chunk_id, value = local_entry
        mapped[chunk_id] = value
    return mapped


def _battle_chunk_refs_to_local_ids(entries: Any) -> list[str]:
    if not isinstance(entries, list):
        return []
    mapped: list[str] = []
    for item in entries:
        local_entry = _battle_chunk_entry_to_local({"ref": item, "data": {}})
        if local_entry is None:
            continue
        mapped.append(local_entry[0])
    return mapped


def _battle_chunk_snapshot_to_proto(scope: dict[str, Any] | None) -> list[dict[str, Any]]:
    if not isinstance(scope, dict):
        return []
    entries: list[dict[str, Any]] = []
    for chunk_id, raw_data in scope.items():
        if not isinstance(chunk_id, str) or not isinstance(raw_data, dict):
            continue
        ref = _battle_chunk_ref_from_sources(chunk_id, raw_data)
        value = _battle_chunk_value_from_data(raw_data)
        if ref is None or value is None:
            continue
        entries.append({
            "ref": ref,
            "data": value,
        })
    return entries


def _battle_chunk_patch_to_proto(scope: dict[str, Any] | None) -> dict[str, Any] | None:
    if not isinstance(scope, dict):
        return None
    proto_scope: dict[str, Any] = {}
    upsert = scope.get("upsert")
    delete = scope.get("delete")
    if isinstance(upsert, dict) and upsert:
        proto_upsert: list[dict[str, Any]] = []
        for chunk_id, patch in upsert.items():
            if not isinstance(chunk_id, str) or not isinstance(patch, dict):
                continue
            ref = _battle_chunk_ref_from_sources(chunk_id, patch)
            value = _battle_chunk_value_from_data(patch)
            if ref is None or value is None:
                continue
            proto_upsert.append({
                "ref": ref,
                "data": value,
            })
        if proto_upsert:
            proto_scope["upsert"] = proto_upsert
    if isinstance(delete, list) and delete:
        proto_delete = [
            ref
            for item in delete
            if isinstance(item, str) and item
            for ref in [_battle_chunk_ref_from_sources(item)]
            if ref is not None
        ]
        if proto_delete:
            proto_scope["delete"] = proto_delete
    return proto_scope or None


def _message_to_plain_dict(message: Message) -> dict[str, Any]:
    message_name = getattr(getattr(message, "DESCRIPTOR", None), "name", "")
    if message_name == "BattleMapObservation":
        return _battle_map_observation_to_plain_dict(message)

    output: dict[str, Any] = {}
    for field, value in message.ListFields():
        key = field.json_name
        if field.type == FieldDescriptor.TYPE_ENUM:
            enum_value = field.enum_type.values_by_number.get(int(value))
            output[key] = enum_value.name if enum_value is not None else int(value)
            continue

        if field.is_repeated:
            if field.message_type is not None and field.message_type.GetOptions().map_entry:
                mapped: dict[str, Any] = {}
                for map_key in value:
                    entry_value = value[map_key]
                    mapped[str(map_key)] = _message_to_value(entry_value)
                output[key] = mapped
            elif field.type == FieldDescriptor.TYPE_MESSAGE:
                output[key] = [_message_to_plain_dict(item) for item in value]
            else:
                output[key] = list(value)
            continue

        if field.type == FieldDescriptor.TYPE_MESSAGE:
            output[key] = _message_to_plain_dict(value)
            continue

        output[key] = value
    return output


def _patch_upserts_to_map(items: list[dict[str, Any]]) -> dict[str, Any]:
    mapped: dict[str, Any] = {}
    for item in items:
        if not isinstance(item, dict):
            continue
        item_id = item.get("id")
        if not isinstance(item_id, str) or not item_id:
            continue
        data = item.get("data")
        mapped[item_id] = data if isinstance(data, dict) else {}
    return mapped


def _remap_message_value(value: Any, field: FieldDescriptor) -> Any:
    if field.type != FieldDescriptor.TYPE_MESSAGE:
        return value

    message_type = field.message_type
    if message_type is None:
        return value

    if field.is_repeated:
        if message_type.GetOptions().map_entry:
            if not isinstance(value, dict):
                return value
            value_field = message_type.fields_by_name.get("value")
            if value_field is None:
                return value
            if value_field.type != FieldDescriptor.TYPE_MESSAGE:
                return dict(value)
            return {
                str(map_key): _remap_message_dict(map_value, value_field.message_type)
                if isinstance(map_value, dict)
                else map_value
                for map_key, map_value in value.items()
            }

        if not isinstance(value, list):
            return value
        return [
            _remap_message_dict(item, message_type) if isinstance(item, dict) else item
            for item in value
        ]

    if isinstance(value, dict):
        return _remap_message_dict(value, message_type)
    return value


def _remap_message_dict(data: dict[str, Any], descriptor) -> dict[str, Any]:
    remapped: dict[str, Any] = {}
    fields_by_exact: dict[str, FieldDescriptor] = {}
    fields_by_normalized: dict[str, FieldDescriptor] = {}

    for field in descriptor.fields:
        fields_by_exact[field.name] = field
        fields_by_exact[field.json_name] = field
        fields_by_normalized[_normalize_field_key(field.name)] = field
        fields_by_normalized[_normalize_field_key(field.json_name)] = field

    for key, value in data.items():
        key_text = str(key)
        field = fields_by_exact.get(key_text)
        if field is None:
            field = fields_by_normalized.get(_normalize_field_key(key_text))

        if field is None:
            remapped[key_text] = value
            continue

        remapped[field.json_name] = _remap_message_value(value, field)

    return remapped


def _coerce_scalar_for_field(field: FieldDescriptor, value: Any) -> Any:
    if field.type == FieldDescriptor.TYPE_ENUM:
        if isinstance(value, str):
            enum_value = field.enum_type.values_by_name.get(value)
            if enum_value is not None:
                return enum_value.number
        return value
    if field.type == FieldDescriptor.TYPE_STRING:
        if value is None:
            return ""
        return str(value)
    if field.type == FieldDescriptor.TYPE_BOOL:
        return bool(value)
    if field.type in {
        FieldDescriptor.TYPE_INT32,
        FieldDescriptor.TYPE_INT64,
        FieldDescriptor.TYPE_SINT32,
        FieldDescriptor.TYPE_SINT64,
        FieldDescriptor.TYPE_SFIXED32,
        FieldDescriptor.TYPE_SFIXED64,
        FieldDescriptor.TYPE_FIXED32,
        FieldDescriptor.TYPE_FIXED64,
        FieldDescriptor.TYPE_UINT32,
        FieldDescriptor.TYPE_UINT64,
    }:
        return int(value)
    if field.type in {
        FieldDescriptor.TYPE_DOUBLE,
        FieldDescriptor.TYPE_FLOAT,
    }:
        return float(value)
    if field.type == FieldDescriptor.TYPE_BYTES:
        if isinstance(value, bytes):
            return value
        if isinstance(value, bytearray):
            return bytes(value)
    return value


def _populate_message_field(message: Message, field: FieldDescriptor, value: Any) -> None:
    if field.type == FieldDescriptor.TYPE_MESSAGE:
        message_type = field.message_type
        if message_type is None:
            return

        if field.is_repeated:
            if message_type.GetOptions().map_entry:
                if not isinstance(value, dict):
                    return
                key_field = message_type.fields_by_name.get("key")
                value_field = message_type.fields_by_name.get("value")
                if key_field is None or value_field is None:
                    return
                container = getattr(message, field.name)
                for raw_key, raw_value in value.items():
                    map_key = _coerce_scalar_for_field(key_field, raw_key)
                    if value_field.type == FieldDescriptor.TYPE_MESSAGE:
                        if not isinstance(raw_value, dict):
                            continue
                        child = container[map_key]
                        _populate_message(child, raw_value, value_field.message_type)
                    else:
                        container[map_key] = _coerce_scalar_for_field(value_field, raw_value)
                return

            if not isinstance(value, list):
                return
            container = getattr(message, field.name)
            for item in value:
                if not isinstance(item, dict):
                    continue
                child = container.add()
                _populate_message(child, item, message_type)
            return

        if not isinstance(value, dict):
            return
        child = message_type._concrete_class()
        _populate_message(child, value, message_type)
        getattr(message, field.name).CopyFrom(child)
        return

    if field.is_repeated:
        if not isinstance(value, list):
            return
        container = getattr(message, field.name)
        container.extend(_coerce_scalar_for_field(field, item) for item in value)
        return

    setattr(message, field.name, _coerce_scalar_for_field(field, value))


def _populate_message(message: Message, data: dict[str, Any], descriptor=None) -> Message:
    message_descriptor = descriptor or message.DESCRIPTOR
    remapped = _remap_message_dict(data, message_descriptor)
    fields_by_json = {field.json_name: field for field in message_descriptor.fields}

    for key, value in remapped.items():
        field = fields_by_json.get(str(key))
        if field is None or value is None:
            continue
        _populate_message_field(message, field, value)

    return message


def _decode_payload(payload_name: str, payload: Message) -> dict[str, Any]:
    if payload_name in {"player_handshake_request", "web_map_handshake_request", "admin_handshake_request"}:
        data = _message_to_plain_dict(payload)
        data["type"] = "handshake"
        data["_payload_case"] = payload_name
        return data

    if payload_name == "web_map_command":
        command_name = payload.WhichOneof("command")
        if not command_name:
            raise PacketDecodeError("invalid_payload", "web_map_command must contain a command")
        command_payload = getattr(payload, command_name)
        data = _message_to_plain_dict(command_payload)
        data["type"] = _WEB_MAP_COMMAND_TO_TYPE.get(command_name, command_name)
        data["_payload_case"] = payload_name
        data["_command_case"] = command_name
        return data

    if payload_name == "player_report_bundle":
        data = _message_to_plain_dict(payload)
        bundle: dict[str, Any] = {
            "type": "player_report_bundle",
            "submitPlayerId": data.get("submitPlayerId"),
            "_payload_case": payload_name,
        }

        players_replace = data.get("playersReplace")
        if isinstance(players_replace, dict):
            bundle["playersReplace"] = players_replace.get("players", {})

        players_patch = data.get("playersPatch")
        if isinstance(players_patch, dict):
            bundle["playersPatch"] = {
                "upsert": _patch_upserts_to_map(players_patch.get("upsert", [])),
                "delete": list(players_patch.get("delete", [])),
            }

        entities_replace = data.get("entitiesReplace")
        if isinstance(entities_replace, dict):
            bundle["entitiesReplace"] = entities_replace.get("entities", {})

        entities_patch = data.get("entitiesPatch")
        if isinstance(entities_patch, dict):
            bundle["entitiesPatch"] = {
                "upsert": _patch_upserts_to_map(entities_patch.get("upsert", [])),
                "delete": list(entities_patch.get("delete", [])),
            }

        waypoints_replace = data.get("waypointsReplace")
        if isinstance(waypoints_replace, dict):
            bundle["waypointsReplace"] = waypoints_replace.get("waypoints", {})

        waypoints_patch = data.get("waypointsPatch")
        if isinstance(waypoints_patch, dict):
            bundle["waypointsPatch"] = {
                "upsert": _patch_upserts_to_map(waypoints_patch.get("upsert", [])),
                "delete": list(waypoints_patch.get("delete", [])),
            }

        tab_players_replace = data.get("tabPlayersReplace")
        if isinstance(tab_players_replace, dict):
            bundle["tabPlayersReplace"] = list(tab_players_replace.get("tabPlayers", []))

        tab_players_patch = data.get("tabPlayersPatch")
        if isinstance(tab_players_patch, dict):
            bundle["tabPlayersPatch"] = {
                "upsert": _patch_upserts_to_map(tab_players_patch.get("upsert", [])),
                "delete": list(tab_players_patch.get("delete", [])),
            }

        for key in (
            "battleMapObservation",
            "stateKeepalive",
            "sourceStateClear",
            "waypointsDelete",
            "waypointsEntityDeathCancel",
        ):
            value = data.get(key)
            if value is not None:
                if not isinstance(value, dict):
                    bundle[key] = value
                    continue
                if key == "battleMapObservation":
                    value["type"] = "battle_map_observation"
                elif key == "stateKeepalive":
                    value["type"] = "state_keepalive"
                elif key == "sourceStateClear":
                    value["type"] = "source_state_clear"
                elif key == "waypointsDelete":
                    value["type"] = "waypoints_delete"
                elif key == "waypointsEntityDeathCancel":
                    value["type"] = "waypoints_entity_death_cancel"
                bundle[key] = value

        return bundle

    if payload_name == "web_map_ack":
        data = _message_to_plain_dict(payload)
        packet: dict[str, Any] = {
            "type": "web_map_ack",
            "ok": bool(data.get("ok")),
            "_payload_case": payload_name,
        }
        for key in ("action", "error", "command"):
            value = data.get(key)
            if value is not None:
                packet[key] = value

        for detail_key, output_keys in (
            ("playerMark", ("playerId", "mark")),
            ("clearAllPlayerMarks", ("removedCount",)),
            ("sameServerFilter", ("enabled",)),
            ("tacticalWaypoint", ("waypointId", "waypoint")),
            ("waypointsDelete", ("waypointIds",)),
        ):
            detail = data.get(detail_key)
            if not isinstance(detail, dict):
                continue
            for output_key in output_keys:
                value = detail.get(output_key)
                if value is not None:
                    packet[output_key] = value
            break

        return packet

    if payload_name == "snapshot_full":
        data = _message_to_plain_dict(payload)
        data["battleChunks"] = _battle_chunk_entries_to_local_map(data.get("battleChunks"))
        data["type"] = "snapshot_full"
        data["_payload_case"] = payload_name
        return data

    if payload_name == "patch":
        data = _message_to_plain_dict(payload)
        battle_chunk_scope = data.get("battleChunks")
        if isinstance(battle_chunk_scope, dict):
            data["battleChunks"] = {
                "upsert": _battle_chunk_entries_to_local_map(battle_chunk_scope.get("upsert")),
                "delete": _battle_chunk_refs_to_local_ids(battle_chunk_scope.get("delete")),
            }
        data["type"] = "patch"
        data["_payload_case"] = payload_name
        return data

    if payload_name == "refresh_request":
        data = _message_to_plain_dict(payload)
        data["battleChunks"] = _battle_chunk_refs_to_local_ids(data.get("battleChunks"))
        data["type"] = "refresh_req"
        data["_payload_case"] = payload_name
        return data

    packet_type = _PAYLOAD_TO_TYPE.get(payload_name, payload_name)
    data = _message_to_plain_dict(payload)
    data["type"] = packet_type
    data["_payload_case"] = payload_name

    if payload_name in {"players_patch", "entities_patch", "tab_players_patch"}:
        data["upsert"] = _patch_upserts_to_map(data.get("upsert", []))
        data["delete"] = list(data.get("delete", []))
        return data

    return data


def _decode_wire_channel(channel_value: int) -> str | None:
    return _WIRE_CHANNEL_TO_NAME.get(int(channel_value))


def _scope_patch_to_proto(scope: dict[str, Any] | None) -> dict[str, Any] | None:
    if not isinstance(scope, dict):
        return None
    upsert = scope.get("upsert")
    delete = scope.get("delete")
    proto_scope: dict[str, Any] = {}
    if isinstance(upsert, dict) and upsert:
        proto_scope["upsert"] = [
            {"id": object_id, "data": patch}
            for object_id, patch in upsert.items()
            if isinstance(object_id, str) and object_id and isinstance(patch, dict)
        ]
    if isinstance(delete, list) and delete:
        proto_scope["delete"] = [item for item in delete if isinstance(item, str) and item]
    return proto_scope or None


def _convert_patch_body(body: dict[str, Any]) -> dict[str, Any]:
    proto_body: dict[str, Any] = {}
    for old_key, new_key in (
        ("players", "players"),
        ("entities", "entities"),
        ("waypoints", "waypoints"),
        ("playerMarks", "playerMarks"),
    ):
        converted = _scope_patch_to_proto(body.get(old_key))
        if converted:
            proto_body[new_key] = converted

    battle_chunks = _battle_chunk_patch_to_proto(body.get("battleChunks"))
    if battle_chunks:
        proto_body["battleChunks"] = battle_chunks

    meta = body.get("meta")
    if isinstance(meta, dict):
        tab_state_patch = meta.get("tabStatePatch")
        if isinstance(tab_state_patch, dict):
            converted_patch = dict(tab_state_patch)
            if "groups" in converted_patch:
                groups_value = converted_patch.get("groups")
                converted_patch["groups"] = {"values": groups_value if isinstance(groups_value, list) else []}
            proto_body["tabStatePatch"] = converted_patch

        if "connections" in meta:
            connections = meta.get("connections")
            proto_body["connections"] = {"values": connections if isinstance(connections, list) else []}

        if meta.get("connections_count") is not None:
            proto_body["connectionsCount"] = meta.get("connections_count")

    if body.get("server_time") is not None:
        proto_body["serverTime"] = body.get("server_time")

    return proto_body


def _convert_snapshot_body(body: dict[str, Any]) -> dict[str, Any]:
    proto_body: dict[str, Any] = {}
    for key in ("players", "entities", "waypoints", "playerMarks", "tabState", "connections"):
        value = body.get(key)
        if value is not None:
            proto_body[key] = value

    battle_chunks = _battle_chunk_snapshot_to_proto(body.get("battleChunks"))
    if battle_chunks:
        proto_body["battleChunks"] = battle_chunks

    if body.get("roomCode") is not None:
        proto_body["roomCode"] = body.get("roomCode")
    if body.get("connections_count") is not None:
        proto_body["connectionsCount"] = body.get("connections_count")
    if body.get("server_time") is not None:
        proto_body["serverTime"] = body.get("server_time")
    return proto_body


def _convert_digest_body(body: dict[str, Any]) -> dict[str, Any]:
    hashes = body.get("hashes")
    return dict(hashes) if isinstance(hashes, dict) else {}


def _convert_web_map_ack_body(body: dict[str, Any]) -> dict[str, Any]:
    proto_body: dict[str, Any] = {
        "ok": bool(body.get("ok")),
    }
    for key in ("action", "error", "command"):
        value = body.get(key)
        if value is not None:
            proto_body[key] = value

    if body.get("playerId") is not None or body.get("mark") is not None:
        detail: dict[str, Any] = {}
        if body.get("playerId") is not None:
            detail["playerId"] = body.get("playerId")
        if isinstance(body.get("mark"), dict):
            detail["mark"] = body.get("mark")
        proto_body["playerMark"] = detail
        return proto_body

    if body.get("removedCount") is not None:
        proto_body["clearAllPlayerMarks"] = {"removedCount": body.get("removedCount")}
        return proto_body

    if body.get("enabled") is not None:
        proto_body["sameServerFilter"] = {"enabled": bool(body.get("enabled"))}
        return proto_body

    if body.get("waypointId") is not None or body.get("waypoint") is not None:
        detail = {}
        if body.get("waypointId") is not None:
            detail["waypointId"] = body.get("waypointId")
        if isinstance(body.get("waypoint"), dict):
            detail["waypoint"] = body.get("waypoint")
        proto_body["tacticalWaypoint"] = detail
        return proto_body

    waypoint_ids = body.get("waypointIds")
    if isinstance(waypoint_ids, list):
        proto_body["waypointsDelete"] = {"waypointIds": [item for item in waypoint_ids if isinstance(item, str) and item]}

    return proto_body


def _convert_refresh_request_body(body: dict[str, Any]) -> dict[str, Any]:
    proto_body = {
        key: value
        for key, value in body.items()
        if key not in {"type", "channel", "battleChunks"}
    }
    battle_chunks = body.get("battleChunks")
    if isinstance(battle_chunks, list):
        proto_body["battleChunks"] = [
            ref
            for item in battle_chunks
            if isinstance(item, str) and item
            for ref in [_battle_chunk_ref_from_sources(item)]
            if ref is not None
        ]
    return proto_body


def _convert_outbound_body(packet_type: str, body: dict[str, Any]) -> tuple[str, int, dict[str, Any]]:
    if packet_type == "handshake":
        channel_name = str(body.get("channel") or "").strip().lower()
        if channel_name == "web_map":
            return "web_map_handshake_request", teamviewer_pb2.WIRE_CHANNEL_WEB_MAP, {
                key: value
                for key, value in body.items()
                if key not in {"type", "channel", "submitPlayerId"}
            }
        if channel_name == "admin":
            return "admin_handshake_request", teamviewer_pb2.WIRE_CHANNEL_ADMIN, {
                key: value
                for key, value in body.items()
                if key not in {"type", "channel", "submitPlayerId"}
            }
        return "player_handshake_request", teamviewer_pb2.WIRE_CHANNEL_PLAYER, {
            key: value
            for key, value in body.items()
            if key not in {"type", "channel"}
        }

    payload_name = _TYPE_TO_PAYLOAD.get(packet_type)
    if payload_name is None:
        raise PacketDecodeError("unsupported_packet_type", packet_type)

    channel_name = str(body.get("channel") or "").strip().lower()
    if packet_type == "web_map_ack" or channel_name == "web_map":
        channel = teamviewer_pb2.WIRE_CHANNEL_WEB_MAP
    elif channel_name == "admin":
        channel = teamviewer_pb2.WIRE_CHANNEL_ADMIN
    else:
        channel = teamviewer_pb2.WIRE_CHANNEL_PLAYER

    if packet_type == "patch":
        return payload_name, channel, _convert_patch_body(body)
    if packet_type == "snapshot_full":
        return payload_name, channel, _convert_snapshot_body(body)
    if packet_type == "digest":
        return payload_name, channel, _convert_digest_body(body)
    if packet_type == "web_map_ack":
        return payload_name, channel, _convert_web_map_ack_body(body)
    if packet_type == "refresh_req":
        return payload_name, channel, _convert_refresh_request_body(body)

    proto_body = {
        key: value
        for key, value in body.items()
        if key not in {"type", "channel"}
    }
    return payload_name, channel, proto_body


class ProtobufMessageCodec:
    def decode(self, payload: bytes | bytearray | memoryview | str) -> dict[str, Any]:
        if isinstance(payload, str):
            raise PacketDecodeError("invalid_payload", "payload must be bytes")
        if isinstance(payload, memoryview):
            raw = payload.tobytes()
        else:
            raw = bytes(payload)
        if not raw:
            raise PacketDecodeError("invalid_payload", "payload must not be empty")

        envelope = teamviewer_pb2.WireEnvelope()
        try:
            envelope.ParseFromString(raw)
        except Exception as exc:
            raise PacketDecodeError("invalid_protobuf", str(exc)) from exc

        payload_name = envelope.WhichOneof("payload")
        if not payload_name:
            raise PacketDecodeError("invalid_payload", "payload must contain a message body")

        payload = getattr(envelope, payload_name)
        decoded = _decode_payload(payload_name, payload)
        wire_channel = _decode_wire_channel(envelope.channel)
        if wire_channel:
            decoded["_wire_channel"] = wire_channel
        return decoded

    def encode(self, packet: dict[str, Any] | Any) -> bytes:
        if hasattr(packet, "model_dump"):
            body = packet.model_dump(exclude_none=True)
        else:
            body = dict(packet)

        packet_type = str(body.get("type") or "").strip()
        if not packet_type:
            raise PacketDecodeError("invalid_payload", "packet type is required")

        payload_name, channel, proto_body = _convert_outbound_body(packet_type, body)
        payload_cls = _PAYLOAD_TO_MESSAGE.get(payload_name)
        if payload_cls is None:
            raise PacketDecodeError("unsupported_packet_type", payload_name)

        message = payload_cls()
        try:
            _populate_message(message, proto_body)
        except Exception as exc:
            raise PacketDecodeError("invalid_payload", str(exc)) from exc

        envelope = teamviewer_pb2.WireEnvelope(channel=channel)
        getattr(envelope, payload_name).CopyFrom(message)
        return envelope.SerializeToString()
