from __future__ import annotations

from typing import Any, Protocol

from google.protobuf.descriptor import FieldDescriptor
from google.protobuf.json_format import ParseDict
from google.protobuf.message import Message
from pydantic import BaseModel

from .proto_generated.teamviewer.v1 import teamviewer_pb2
from .protocol import PacketDecodeError


class MessageCodec(Protocol):
    def decode(self, payload: bytes | bytearray | memoryview | str) -> dict[str, Any]: ...

    def encode(self, packet: BaseModel | dict[str, Any]) -> bytes: ...


_PAYLOAD_TO_TYPE: dict[str, str] = {
    "handshake_request": "handshake",
    "ping": "ping",
    "resync_request": "resync_req",
    "command_player_mark_set": "command_player_mark_set",
    "command_player_mark_clear": "command_player_mark_clear",
    "command_player_mark_clear_all": "command_player_mark_clear_all",
    "command_same_server_filter_set": "command_same_server_filter_set",
    "command_tactical_waypoint_set": "command_tactical_waypoint_set",
    "players_update": "players_update",
    "tab_players_update": "tab_players_update",
    "tab_players_patch": "tab_players_patch",
    "players_patch": "players_patch",
    "entities_update": "entities_update",
    "entities_patch": "entities_patch",
    "state_keepalive": "state_keepalive",
    "source_state_clear": "source_state_clear",
    "waypoints_update": "waypoints_update",
    "waypoints_delete": "waypoints_delete",
    "waypoints_entity_death_cancel": "waypoints_entity_death_cancel",
    "battle_map_observation": "battle_map_observation",
    "handshake_ack": "handshake_ack",
    "admin_ack": "admin_ack",
    "pong": "pong",
    "snapshot_full": "snapshot_full",
    "patch": "patch",
    "digest": "digest",
    "refresh_request": "refresh_req",
    "report_rate_hint": "report_rate_hint",
}

_TYPE_TO_PAYLOAD = {value: key for key, value in _PAYLOAD_TO_TYPE.items()}

_TYPE_TO_MESSAGE: dict[str, type[Message]] = {
    "handshake": teamviewer_pb2.HandshakeRequest,
    "ping": teamviewer_pb2.Ping,
    "resync_req": teamviewer_pb2.ResyncRequest,
    "command_player_mark_set": teamviewer_pb2.CommandPlayerMarkSet,
    "command_player_mark_clear": teamviewer_pb2.CommandPlayerMarkClear,
    "command_player_mark_clear_all": teamviewer_pb2.CommandPlayerMarkClearAll,
    "command_same_server_filter_set": teamviewer_pb2.CommandSameServerFilterSet,
    "command_tactical_waypoint_set": teamviewer_pb2.CommandTacticalWaypointSet,
    "players_update": teamviewer_pb2.PlayersUpdate,
    "tab_players_update": teamviewer_pb2.TabPlayersUpdate,
    "tab_players_patch": teamviewer_pb2.TabPlayersPatch,
    "players_patch": teamviewer_pb2.PlayersPatch,
    "entities_update": teamviewer_pb2.EntitiesUpdate,
    "entities_patch": teamviewer_pb2.EntitiesPatch,
    "state_keepalive": teamviewer_pb2.StateKeepalive,
    "source_state_clear": teamviewer_pb2.SourceStateClear,
    "waypoints_update": teamviewer_pb2.WaypointsUpdate,
    "waypoints_delete": teamviewer_pb2.WaypointsDelete,
    "waypoints_entity_death_cancel": teamviewer_pb2.WaypointsEntityDeathCancel,
    "battle_map_observation": teamviewer_pb2.BattleMapObservation,
    "handshake_ack": teamviewer_pb2.HandshakeAck,
    "admin_ack": teamviewer_pb2.AdminAck,
    "pong": teamviewer_pb2.Pong,
    "snapshot_full": teamviewer_pb2.SnapshotFull,
    "patch": teamviewer_pb2.Patch,
    "digest": teamviewer_pb2.Digest,
    "refresh_req": teamviewer_pb2.RefreshRequest,
    "report_rate_hint": teamviewer_pb2.ReportRateHint,
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
        "submitPlayerId": message.submit_player_id,
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

        if field.label == FieldDescriptor.LABEL_REPEATED:
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

    if field.label == FieldDescriptor.LABEL_REPEATED:
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


def _decode_payload(payload_name: str, payload: Message) -> dict[str, Any]:
    packet_type = _PAYLOAD_TO_TYPE.get(payload_name, payload_name)
    data = _message_to_plain_dict(payload)
    data["type"] = packet_type

    if payload_name in {"players_patch", "entities_patch", "tab_players_patch"}:
        data["upsert"] = _patch_upserts_to_map(data.get("upsert", []))
        data["delete"] = list(data.get("delete", []))
        return data

    return data


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
        ("battleChunks", "battleChunks"),
        ("playerMarks", "playerMarks"),
    ):
        converted = _scope_patch_to_proto(body.get(old_key))
        if converted:
            proto_body[new_key] = converted

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
    for key in ("players", "entities", "waypoints", "battleChunks", "playerMarks", "tabState", "connections"):
        value = body.get(key)
        if value is not None:
            proto_body[key] = value

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


def _convert_admin_ack_body(body: dict[str, Any]) -> dict[str, Any]:
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


def _convert_outbound_body(packet_type: str, body: dict[str, Any]) -> tuple[str, int, dict[str, Any]]:
    payload_name = _TYPE_TO_PAYLOAD.get(packet_type)
    if payload_name is None:
        raise PacketDecodeError("unsupported_packet_type", packet_type)

    channel_name = str(body.get("channel") or "").strip().lower()
    if packet_type == "admin_ack" or channel_name == "admin":
        channel = teamviewer_pb2.WIRE_CHANNEL_ADMIN
    else:
        channel = teamviewer_pb2.WIRE_CHANNEL_PLAYER

    if packet_type == "patch":
        return payload_name, channel, _convert_patch_body(body)
    if packet_type == "snapshot_full":
        return payload_name, channel, _convert_snapshot_body(body)
    if packet_type == "digest":
        return payload_name, channel, _convert_digest_body(body)
    if packet_type == "admin_ack":
        return payload_name, channel, _convert_admin_ack_body(body)

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
        return _decode_payload(payload_name, payload)

    def encode(self, packet: BaseModel | dict[str, Any]) -> bytes:
        if isinstance(packet, BaseModel):
            body = packet.model_dump(exclude_none=True)
        else:
            body = dict(packet)

        packet_type = str(body.get("type") or "").strip()
        if not packet_type:
            raise PacketDecodeError("invalid_payload", "packet type is required")

        payload_name, channel, proto_body = _convert_outbound_body(packet_type, body)
        payload_cls = _TYPE_TO_MESSAGE.get(packet_type)
        if payload_cls is None:
            raise PacketDecodeError("unsupported_packet_type", packet_type)

        message = payload_cls()
        try:
            normalized_body = _remap_message_dict(proto_body, message.DESCRIPTOR)
            ParseDict(normalized_body, message, ignore_unknown_fields=False)
        except Exception as exc:
            raise PacketDecodeError("invalid_payload", str(exc)) from exc

        envelope = teamviewer_pb2.WireEnvelope(channel=channel)
        getattr(envelope, payload_name).CopyFrom(message)
        return envelope.SerializeToString()
