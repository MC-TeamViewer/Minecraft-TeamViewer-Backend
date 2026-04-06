from __future__ import annotations

import importlib
from pathlib import Path
import sys

import pytest

BACKEND_SRC = Path(__file__).resolve().parents[1] / "src"
if str(BACKEND_SRC) not in sys.path:
    sys.path.insert(0, str(BACKEND_SRC))

import main as main_module
from server.app import runtime as app_runtime
from server.ws import io as ws_io


def _reload_main(monkeypatch: pytest.MonkeyPatch, tmp_path: Path):
    monkeypatch.setenv("TEAMVIEWER_ADMIN_USERNAME", "admin")
    monkeypatch.setenv("TEAMVIEWER_ADMIN_PASSWORD", "secret")
    monkeypatch.setenv("TEAMVIEWER_DB_PATH", str(tmp_path / "compat.db"))
    return importlib.reload(main_module)


@pytest.mark.asyncio
async def test_main_entrypoint_keeps_runtime_lifecycle_accessible(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    main = _reload_main(monkeypatch, tmp_path)

    assert not hasattr(main, "send_packet")
    assert app_runtime.admin_store is None
    assert app_runtime.admin_payload_service is None

    async with main.app.router.lifespan_context(main.app):
        assert app_runtime.admin_store is not None
        assert app_runtime.admin_payload_service is not None

    assert app_runtime.admin_store is None
    assert app_runtime.admin_payload_service is None


@pytest.mark.asyncio
async def test_ws_io_send_packet_writes_encoded_payload(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    _reload_main(monkeypatch, tmp_path)
    sent_payloads: list[bytes] = []
    marker_packet = {"type": "test"}
    encoded_payload = b"encoded-test-packet"

    monkeypatch.setattr(
        app_runtime.message_codec,
        "encode",
        lambda packet: encoded_payload if packet == {**marker_packet, "channel": "player"} else b"",
    )

    class _StubWebSocket:
        async def send_bytes(self, payload: bytes) -> None:
            sent_payloads.append(payload)

    marker_ws = _StubWebSocket()
    await ws_io.send_packet(marker_ws, marker_packet, channel="player")

    assert sent_payloads == [encoded_payload]
