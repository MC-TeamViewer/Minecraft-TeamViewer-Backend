import time

from fastapi.responses import JSONResponse

from . import runtime


async def health_check():
    return JSONResponse({"status": "ok"})


async def snapshot(roomCode: str | None = None):
    current_time = time.time()

    player_connection_ids = runtime.state.get_player_connection_ids()
    connections_by_room: dict[str, list[str]] = {}
    for player_id in player_connection_ids:
        if not isinstance(player_id, str) or not player_id:
            continue
        room = runtime.state.get_player_room(player_id)
        connections_by_room.setdefault(room, []).append(player_id)

    for room in list(connections_by_room.keys()):
        connections_by_room[room].sort()

    active_rooms = sorted({
        runtime.state.get_player_room(source_id)
        for source_id in runtime.state.connections
        if isinstance(source_id, str) and source_id
    })
    requested_room = runtime.state.normalize_room_code(roomCode) if roomCode is not None else None
    selected_room = requested_room if requested_room is not None else runtime.state.DEFAULT_ROOM_CODE
    selected_sources = runtime.state.get_active_sources_in_room(selected_room)
    selected_player_sources = selected_sources.intersection(player_connection_ids)

    selected_state = runtime.state.resolve_states_for_sources(selected_sources, selected_room)
    selected_players = selected_state["players"]
    selected_entities = selected_state["entities"]
    selected_waypoints = selected_state["waypoints"]
    selected_battle_chunks = selected_state["battleChunks"]

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
            "connections": sorted(player_connection_ids),
            "connections_count": len(player_connection_ids),
            "activeRooms": active_rooms,
            "connectionsByRoom": connections_by_room,
            "requestedRoomCode": requested_room,
            "selectedRoomCode": selected_room,
            "roomView": {
                "roomCode": selected_room,
                "connections": sorted(selected_player_sources),
                "connections_count": len(selected_player_sources),
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
