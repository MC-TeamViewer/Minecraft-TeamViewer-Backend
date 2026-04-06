import time

from fastapi.responses import JSONResponse

from . import runtime


async def health_check():
    return JSONResponse({"status": "ok"})


async def snapshot(roomCode: str | None = None):
    current_time = time.time()

    connections_by_room: dict[str, list[str]] = {}
    for player_id in runtime.state.connections.keys():
        if not isinstance(player_id, str) or not player_id:
            continue
        room = runtime.state.get_player_room(player_id)
        connections_by_room.setdefault(room, []).append(player_id)

    for room in list(connections_by_room.keys()):
        connections_by_room[room].sort()

    active_rooms = sorted(connections_by_room.keys())
    requested_room = runtime.state.normalize_room_code(roomCode) if roomCode is not None else None
    selected_room = requested_room if requested_room is not None else runtime.state.DEFAULT_ROOM_CODE
    selected_sources = runtime.state.get_active_sources_in_room(selected_room)

    selected_players = runtime.state.filter_state_map_by_sources(runtime.state.players, selected_sources)
    selected_entities = runtime.state.filter_state_map_by_sources(runtime.state.entities, selected_sources)
    selected_waypoints = runtime.state.filter_waypoint_state_by_sources_and_room(
        runtime.state.waypoints,
        selected_sources,
        selected_room,
    )
    selected_battle_chunks = runtime.state.filter_battle_chunk_state_by_sources_and_room(
        runtime.state.battle_chunks,
        selected_sources,
        selected_room,
    )

    room_digests = {
        "players": runtime.state.state_digest(selected_players),
        "entities": runtime.state.state_digest(selected_entities),
        "waypoints": runtime.state.state_digest(selected_waypoints),
        "battleChunks": runtime.state.state_digest(selected_battle_chunks),
    }

    return JSONResponse(
        {
            "server_time": current_time,
            "players": dict(runtime.state.players),
            "entities": dict(runtime.state.entities),
            "waypoints": dict(runtime.state.waypoints),
            "battleChunks": dict(runtime.state.battle_chunks),
            "playerMarks": dict(runtime.state.player_marks),
            "tabState": runtime.state.build_web_map_tab_snapshot(selected_room),
            "connections": list(runtime.state.connections.keys()),
            "connections_count": len(runtime.state.connections),
            "activeRooms": active_rooms,
            "connectionsByRoom": connections_by_room,
            "requestedRoomCode": requested_room,
            "selectedRoomCode": selected_room,
            "roomView": {
                "roomCode": selected_room,
                "connections": sorted(selected_sources),
                "connections_count": len(selected_sources),
                "players": dict(selected_players),
                "entities": dict(selected_entities),
                "waypoints": dict(selected_waypoints),
                "battleChunks": dict(selected_battle_chunks),
                "tabState": runtime.state.build_web_map_tab_snapshot(selected_room),
                "digests": room_digests,
            },
            "broadcastHz": runtime.state.broadcast_hz,
            "digests": runtime.state.build_digests(),
        }
    )


def register_app_routes(app) -> None:
    app.get("/health")(health_check)
    app.get("/snapshot")(snapshot)
