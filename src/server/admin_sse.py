from __future__ import annotations

import asyncio
from dataclasses import dataclass, field


@dataclass(slots=True, eq=False)
class AdminSseSubscriber:
    queue: asyncio.Queue[str]
    audit_limit: int = 100
    audit_event_type: str | None = None
    audit_actor_types: tuple[str, ...] = field(default_factory=tuple)
    audit_success: bool | None = None


@dataclass(slots=True)
class _ScheduledEvent:
    task: asyncio.Task
    event_name: str


class AdminSseHub:
    def __init__(self, *, queue_size: int = 32) -> None:
        self._queue_size = max(4, queue_size)
        self._subscribers: set[AdminSseSubscriber] = set()
        self._lock = asyncio.Lock()
        self._scheduled: dict[str, _ScheduledEvent] = {}

    async def subscribe(
        self,
        *,
        audit_limit: int = 100,
        audit_event_type: str | None = None,
        audit_actor_types: tuple[str, ...] = (),
        audit_success: bool | None = None,
    ) -> AdminSseSubscriber:
        subscriber = AdminSseSubscriber(
            queue=asyncio.Queue(maxsize=self._queue_size),
            audit_limit=audit_limit,
            audit_event_type=audit_event_type,
            audit_actor_types=audit_actor_types,
            audit_success=audit_success,
        )
        async with self._lock:
            self._subscribers.add(subscriber)
        return subscriber

    async def unsubscribe(self, subscriber: AdminSseSubscriber) -> None:
        async with self._lock:
            self._subscribers.discard(subscriber)

    async def broadcast(self, event_name: str) -> None:
        async with self._lock:
            subscribers = list(self._subscribers)

        for subscriber in subscribers:
            self._enqueue(subscriber.queue, event_name)

    def schedule_broadcast(self, event_name: str, *, delay_sec: float = 1.0) -> None:
        existing = self._scheduled.get(event_name)
        if existing is not None and not existing.task.done():
            return

        task = asyncio.create_task(self._delayed_broadcast(event_name, delay_sec))
        self._scheduled[event_name] = _ScheduledEvent(task=task, event_name=event_name)

    async def close(self) -> None:
        scheduled = list(self._scheduled.values())
        self._scheduled.clear()
        for item in scheduled:
            item.task.cancel()
        for item in scheduled:
            try:
                await item.task
            except asyncio.CancelledError:
                pass

        async with self._lock:
            self._subscribers.clear()

    async def _delayed_broadcast(self, event_name: str, delay_sec: float) -> None:
        try:
            await asyncio.sleep(max(0.0, delay_sec))
            await self.broadcast(event_name)
        except asyncio.CancelledError:
            raise
        finally:
            current = self._scheduled.get(event_name)
            if current is not None and current.task is asyncio.current_task():
                self._scheduled.pop(event_name, None)

    @staticmethod
    def _enqueue(queue: asyncio.Queue[str], event_name: str) -> None:
        try:
            queue.put_nowait(event_name)
            return
        except asyncio.QueueFull:
            pass

        drained: list[str] = []
        while True:
            try:
                drained.append(queue.get_nowait())
            except asyncio.QueueEmpty:
                break

        for existing in drained:
            if existing != event_name:
                try:
                    queue.put_nowait(existing)
                except asyncio.QueueFull:
                    break

        if queue.full():
            try:
                queue.get_nowait()
            except asyncio.QueueEmpty:
                pass

        try:
            queue.put_nowait(event_name)
        except asyncio.QueueFull:
            pass
