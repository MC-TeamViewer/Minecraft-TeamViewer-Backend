from __future__ import annotations

import asyncio

from uvicorn.config import Config
from uvicorn.protocols.websockets.websockets_impl import WebSocketProtocol
from uvicorn.server import ServerState

from ..admin.traffic import infer_traffic_channel_from_path, record_websocket_traffic_nowait


class _TrafficCountingTransport:
    def __init__(self, transport: asyncio.Transport, protocol: "TeamViewerWebSocketProtocol") -> None:
        self._transport = transport
        self._protocol = protocol

    def write(self, data: bytes) -> None:
        self._protocol.record_outbound_wire_bytes(data)
        self._transport.write(data)

    def writelines(self, lines: list[bytes]) -> None:
        payload = b"".join(lines)
        self._protocol.record_outbound_wire_bytes(payload)
        self._transport.writelines(lines)

    def __getattr__(self, name: str):
        return getattr(self._transport, name)


class TeamViewerWebSocketProtocol(WebSocketProtocol):
    def __init__(
        self,
        config: Config,
        server_state: ServerState,
        app_state: dict[str, object],
        _loop: asyncio.AbstractEventLoop | None = None,
    ) -> None:
        self._traffic_channel: str | None = None
        self._wire_counting_enabled = False
        super().__init__(config=config, server_state=server_state, app_state=app_state, _loop=_loop)

    def connection_made(self, transport: asyncio.Transport) -> None:
        super().connection_made(_TrafficCountingTransport(transport, self))

    async def process_request(self, path: str, request_headers) -> object | None:
        path_portion, _, _query_string = path.partition("?")
        self._traffic_channel = infer_traffic_channel_from_path(path_portion)
        return await super().process_request(path, request_headers)

    async def ws_handler(self, protocol, path: str) -> object:
        self._wire_counting_enabled = self._traffic_channel is not None
        try:
            return await super().ws_handler(protocol, path)
        finally:
            self._wire_counting_enabled = False

    def data_received(self, data: bytes) -> None:
        self.record_inbound_wire_bytes(data)
        super().data_received(data)

    def connection_lost(self, exc: Exception | None) -> None:
        self._wire_counting_enabled = False
        super().connection_lost(exc)

    def record_inbound_wire_bytes(self, data: bytes) -> None:
        self._record_wire_bytes(direction="ingress", data=data)

    def record_outbound_wire_bytes(self, data: bytes) -> None:
        self._record_wire_bytes(direction="egress", data=data)

    def _record_wire_bytes(self, *, direction: str, data: bytes) -> None:
        if not self._wire_counting_enabled or self._traffic_channel is None:
            return
        size = len(data)
        if size <= 0:
            return
        record_websocket_traffic_nowait(
            layer="wire",
            channel=self._traffic_channel,
            direction=direction,
            byte_count=size,
        )
