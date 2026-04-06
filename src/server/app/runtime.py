import asyncio
import logging
import os

from ..admin.sse import AdminSseHub
from ..core.broadcaster import Broadcaster
from ..core.codec import ProtobufMessageCodec
from ..state import ServerState


NETWORK_PROTOCOL_VERSION = "0.6.1"
SERVER_MIN_COMPATIBLE_PROTOCOL_VERSION = "0.6.1"
SERVER_PROGRAM_VERSION = "team-view-relay-server-dev"
LEGACY_PROTOCOL_REJECTION_REASON = (
    "unsupported_protocol_version: "
    "当前服务器仅支持 Protobuf 协议（0.6.1 及以上）。"
    "battleChunks 同步与一致性修正要求客户端升级到 0.6.1。"
    "MessagePack 协议（0.5.x 及更早版本）已不再支持。"
    "请升级到最新版本的客户端后重试。"
)
DEFAULT_ADMIN_DB_PATH = "./data/teamviewer-admin.db"


def configure_logging() -> None:
    if logging.getLogger().handlers:
        return

    level_name = os.getenv("TEAMVIEWER_LOG_LEVEL", "INFO").upper()
    level = getattr(logging, level_name, logging.INFO)
    logging.basicConfig(
        level=level,
        format="%(asctime)s %(levelname)s [%(name)s] %(message)s",
    )


configure_logging()
logger = logging.getLogger("teamviewrelay.main")

message_codec = ProtobufMessageCodec()
state = ServerState()
broadcaster = Broadcaster(state)
broadcast_task: asyncio.Task | None = None
admin_store = None
admin_payload_service = None
admin_retention_task: asyncio.Task | None = None
admin_sse_hub = AdminSseHub()
web_map_connection_meta: dict[str, dict] = {}
admin_runtime_stats = {
    "lastRetentionCleanup": None,
    "apiErrors": 0,
    "sseErrors": 0,
}
