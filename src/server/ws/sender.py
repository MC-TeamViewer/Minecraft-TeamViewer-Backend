from __future__ import annotations

import asyncio
import logging
from dataclasses import dataclass
from typing import Callable


logger = logging.getLogger("teamviewrelay.ws.sender")


class WebSocketSendError(RuntimeError):
    pass


class SlowWebSocketError(WebSocketSendError):
    pass


@dataclass(slots=True)
class _SendItem:
    payload: bytes
    on_sent: Callable[[], None] | None
    completion: asyncio.Future[None] | None = None


class _ConnectionSender:
    def __init__(
        self,
        websocket,
        *,
        control_capacity: int,
        send_timeout_sec: float,
        on_coalesced: Callable[[], None],
        on_slow_disconnect: Callable[[], None],
    ) -> None:
        self.websocket = websocket
        self._control: asyncio.Queue[_SendItem] = asyncio.Queue(maxsize=control_capacity)
        self._state_slot: _SendItem | None = None
        self._wake = asyncio.Event()
        self._closed = False
        self._send_timeout_sec = send_timeout_sec
        self._on_coalesced = on_coalesced
        self._on_slow_disconnect = on_slow_disconnect
        self._task = asyncio.create_task(self._run(), name=f"ws-writer-{id(websocket)}")

    @property
    def state_pending(self) -> bool:
        return self._state_slot is not None

    @property
    def closed(self) -> bool:
        return self._closed or self._task.done()

    async def send_control(self, item: _SendItem, *, wait: bool) -> None:
        if self.closed:
            raise WebSocketSendError("websocket writer is closed")
        if wait:
            item.completion = asyncio.get_running_loop().create_future()
        try:
            self._control.put_nowait(item)
        except asyncio.QueueFull as exc:
            self._on_slow_disconnect()
            await self._abort(SlowWebSocketError("websocket control queue is full"))
            raise SlowWebSocketError("websocket control queue is full") from exc
        self._wake.set()
        if item.completion is not None:
            await item.completion

    def send_state(self, item: _SendItem) -> bool:
        if self.closed:
            raise WebSocketSendError("websocket writer is closed")
        replaced = self._state_slot is not None
        if replaced:
            self._on_coalesced()
        self._state_slot = item
        self._wake.set()
        return replaced

    async def close(self) -> None:
        await self._abort(WebSocketSendError("websocket writer closed"), close_socket=False)

    async def _run(self) -> None:
        try:
            while not self._closed:
                item = await self._next_item()
                if item is None:
                    continue
                try:
                    await asyncio.wait_for(
                        self.websocket.send_bytes(item.payload),
                        timeout=self._send_timeout_sec,
                    )
                except asyncio.TimeoutError as exc:
                    self._on_slow_disconnect()
                    error = SlowWebSocketError(
                        f"websocket send blocked for {self._send_timeout_sec:.1f}s"
                    )
                    self._finish(item, error)
                    await self._abort(error)
                    logger.warning("Disconnecting slow websocket id=%s: %s", id(self.websocket), error)
                    return
                except Exception as exc:
                    error = WebSocketSendError(str(exc))
                    self._finish(item, error)
                    await self._abort(error, close_socket=False)
                    return
                if item.on_sent is not None:
                    item.on_sent()
                self._finish(item, None)
        except asyncio.CancelledError:
            await self._abort(WebSocketSendError("websocket writer cancelled"), close_socket=False)
            raise

    async def _next_item(self) -> _SendItem | None:
        while not self._closed:
            try:
                return self._control.get_nowait()
            except asyncio.QueueEmpty:
                pass
            if self._state_slot is not None:
                item = self._state_slot
                self._state_slot = None
                return item
            self._wake.clear()
            if not self._control.empty() or self._state_slot is not None:
                self._wake.set()
                continue
            await self._wake.wait()
        return None

    async def _abort(self, error: Exception, *, close_socket: bool = True) -> None:
        if self._closed:
            return
        self._closed = True
        self._wake.set()
        if self._state_slot is not None:
            self._finish(self._state_slot, error)
            self._state_slot = None
        while True:
            try:
                self._finish(self._control.get_nowait(), error)
            except asyncio.QueueEmpty:
                break
        if close_socket:
            try:
                await asyncio.wait_for(
                    self.websocket.close(code=1013, reason="slow_client_send_timeout"),
                    timeout=1.0,
                )
            except Exception:
                pass

    @staticmethod
    def _finish(item: _SendItem, error: Exception | None) -> None:
        completion = item.completion
        if completion is None or completion.done():
            return
        if error is None:
            completion.set_result(None)
        else:
            completion.set_exception(error)


class WebSocketSendHub:
    """Serialize all writes and isolate state backpressure per connection."""

    def __init__(self, *, control_capacity: int = 32, send_timeout_sec: float = 2.0) -> None:
        self._control_capacity = control_capacity
        self._send_timeout_sec = send_timeout_sec
        self._senders: dict[int, _ConnectionSender] = {}
        self._coalesced_state_updates = 0
        self._slow_client_disconnects = 0

    def state_pending(self, websocket) -> bool:
        sender = self._senders.get(id(websocket))
        return bool(sender is not None and sender.websocket is websocket and sender.state_pending)

    async def send(
        self,
        websocket,
        payload: bytes,
        *,
        coalesce_state: bool,
        wait: bool,
        on_sent: Callable[[], None] | None = None,
    ) -> bool:
        sender = self._get_sender(websocket)
        item = _SendItem(payload=payload, on_sent=on_sent)
        if coalesce_state:
            return sender.send_state(item)
        await sender.send_control(item, wait=wait)
        return False

    async def unregister(self, websocket) -> None:
        sender = self._senders.pop(id(websocket), None)
        if sender is not None and sender.websocket is websocket:
            await sender.close()

    async def close(self) -> None:
        senders = list(self._senders.values())
        self._senders.clear()
        await asyncio.gather(*(sender.close() for sender in senders), return_exceptions=True)

    def snapshot(self) -> dict[str, int | float]:
        return {
            "activeWriters": sum(1 for sender in self._senders.values() if not sender.closed),
            "coalescedStateUpdates": self._coalesced_state_updates,
            "slowClientDisconnects": self._slow_client_disconnects,
            "sendTimeoutSec": self._send_timeout_sec,
            "controlQueueCapacity": self._control_capacity,
        }

    def _get_sender(self, websocket) -> _ConnectionSender:
        key = id(websocket)
        sender = self._senders.get(key)
        if sender is not None and sender.websocket is websocket and not sender.closed:
            return sender
        sender = _ConnectionSender(
            websocket,
            control_capacity=self._control_capacity,
            send_timeout_sec=self._send_timeout_sec,
            on_coalesced=self._record_coalesced,
            on_slow_disconnect=self._record_slow_disconnect,
        )
        self._senders[key] = sender
        return sender

    def _record_coalesced(self) -> None:
        self._coalesced_state_updates += 1

    def _record_slow_disconnect(self) -> None:
        self._slow_client_disconnects += 1


websocket_send_hub = WebSocketSendHub()
