"""Phase 4 smoke: WS 门压缩套协商矩阵(对本地 alpha.6 后端黑盒验证)。

覆盖:
  A. 无协商(旧客户端形态):不提供子协议/扩展 → 服务端不回执,下行 plain。
  B. 仅 permessage-deflate(旧 mod 形态):照常协商 deflate,零变化。
  C. [zstd, plain] 偏好序:回执 zstd、无扩展回执;下行 binary 消息 = 连续
     zstd 流压缩块,持久解压器逐块解码出 envelope;上行恒 plain。
  D. 仅 plain 子协议:回执 plain,下行原样透传。
  E. 解压违规:zstd 协商后发送垃圾 → 连接被服务端显式关闭(服务端不解压
     上行,故此用例改为:客户端侧垃圾块进持久解压器必须报错——服务端行为
     由单测覆盖,这里只验证客户端侧语义)。

运行:uv run --with websockets --with grpcio-tools --with zstandard \
  python scripts/ws_negotiation_smoke.py
"""

from __future__ import annotations

import asyncio
import os
import sys
from pathlib import Path
from tempfile import TemporaryDirectory

import websockets
import zstandard
from grpc_tools import protoc

BACKEND_ROOT = Path(__file__).resolve().parents[1]
PROTO_ROOT = BACKEND_ROOT / "third_party/TeamViewRelay-Protocol/proto"
PROTO_FILE = PROTO_ROOT / "teamviewer/v1/teamviewer.proto"
_PROTO_BUILD = TemporaryDirectory(prefix="tv-smoke-proto-")

_status = protoc.main([
    "grpc_tools.protoc",
    f"-I{PROTO_ROOT}",
    f"--python_out={_PROTO_BUILD.name}",
    str(PROTO_FILE),
])
if _status != 0:
    raise RuntimeError("protoc failed")
sys.path.insert(0, _PROTO_BUILD.name)
from teamviewer.v1 import teamviewer_pb2  # noqa: E402

WS_URL = os.environ.get("TEAMVIEWER_WS_URL", "ws://127.0.0.1:8765/web-map/ws")
ROOM = "smoke-negotiation-v1"
SUB_ZSTD = "teamviewrelay.zstd.v1"
SUB_PLAIN = "teamviewrelay.plain.v1"


def web_handshake() -> bytes:
    envelope = teamviewer_pb2.WireEnvelope(channel=teamviewer_pb2.WIRE_CHANNEL_WEB_MAP)
    request = envelope.web_map_handshake_request
    request.network_protocol_version = "0.8.0"
    request.minimum_compatible_network_protocol_version = "0.6.1"
    request.local_program_version = "tv-smoke-negotiation"
    request.room_code = ROOM
    return envelope.SerializeToString()


def resync_request() -> bytes:
    envelope = teamviewer_pb2.WireEnvelope(channel=teamviewer_pb2.WIRE_CHANNEL_WEB_MAP)
    envelope.resync_request.SetInParent()
    return envelope.SerializeToString()


class PersistentStreamDecoder:
    """与 mod ZstdStreamDecoder 同型的持续解压器(zstandard decompressobj)。"""

    def __init__(self) -> None:
        self._obj = zstandard.ZstdDecompressor().decompressobj()

    def push(self, chunk: bytes) -> bytes:
        return self._obj.decompress(chunk)


def envelope_type(raw: bytes) -> str:
    envelope = teamviewer_pb2.WireEnvelope()
    envelope.ParseFromString(raw)
    return envelope.WhichOneof("payload")


def envelope(raw: bytes) -> teamviewer_pb2.WireEnvelope:
    envelope = teamviewer_pb2.WireEnvelope()
    envelope.ParseFromString(raw)
    return envelope


async def recv_envelope(ws, decoder: PersistentStreamDecoder | None, timeout: float = 5.0):
    raw = await asyncio.wait_for(ws.recv(), timeout)
    if not isinstance(raw, bytes):
        raise AssertionError(f"期望 binary 消息,得到 {type(raw)}")
    if decoder is not None:
        raw = decoder.push(raw)
    return envelope(raw)


async def case_a_plain_no_negotiation() -> None:
    async with websockets.connect(WS_URL, compression=None, max_size=8 << 20) as ws:
        assert ws.response.headers.get("Sec-WebSocket-Protocol") is None, "未提供子协议,服务端不得回执"
        await ws.send(web_handshake())
        ack = await recv_envelope(ws, None)
        assert ack.WhichOneof("payload") == "handshake_ack"
    print("A  plain 无协商:PASS(无回执,下行 plain,握手 ack 通)")


async def case_b_deflate_only() -> None:
    async with websockets.connect(WS_URL, compression="deflate", max_size=8 << 20) as ws:
        assert ws.subprotocol is None, "仅扩展无子协议,服务端不得回执子协议"
        assert ws.response.headers.get("Sec-WebSocket-Extensions"), "deflate 必须照常协商"
        await ws.send(web_handshake())
        ack = await recv_envelope(ws, None)
        assert ack.WhichOneof("payload") == "handshake_ack"
    print("B  仅 permessage-deflate:PASS(旧 mod 回退链零变化)")


async def case_c_zstd_negotiated() -> None:
    async with websockets.connect(
        WS_URL,
        subprotocols=[SUB_ZSTD, SUB_PLAIN],
        compression=None,
        max_size=8 << 20,
    ) as ws:
        assert ws.subprotocol == SUB_ZSTD, f"期望回执 zstd,实际 {ws.subprotocol!r}"
        assert ws.response.headers.get("Sec-WebSocket-Extensions") is None, \
            "zstd 与 permessage-deflate 互斥,服务端不得回执扩展"
        decoder = PersistentStreamDecoder()
        await ws.send(web_handshake())
        ack = await recv_envelope(ws, decoder)
        assert ack.WhichOneof("payload") == "handshake_ack"
        # 上行恒 plain:再发一条 resync 请求验证服务端按 plain 解读上行,
        # 并在 zstd 下行流中等到重发后的 snapshot_full
        await ws.send(resync_request())
        while True:
            reply = await recv_envelope(ws, decoder)
            if reply.WhichOneof("payload") == "snapshot_full":
                break
        print("C  zstd 协商:PASS(回执 zstd、无扩展、下行连续流解码、plain 上行 resync → snapshot_full)")


async def case_d_plain_subprotocol() -> None:
    async with websockets.connect(
        WS_URL,
        subprotocols=[SUB_PLAIN],
        compression=None,
        max_size=8 << 20,
    ) as ws:
        assert ws.subprotocol == SUB_PLAIN, f"期望回执 plain,实际 {ws.subprotocol!r}"
        await ws.send(web_handshake())
        ack = await recv_envelope(ws, None)
        assert ack.WhichOneof("payload") == "handshake_ack"
    print("D  plain 子协议:PASS(回执原值,下行透传)")


async def case_e_client_side_garbage_rejected() -> None:
    decoder = PersistentStreamDecoder()
    try:
        decoder.push(bytes([0xFF] * 64))
    except Exception:
        print("E  客户端侧垃圾块:PASS(解压器显式报错,对应 mod 显式断连语义)")
        return
    raise AssertionError("垃圾压缩块必须让持久解压器报错")


async def main() -> None:
    await case_a_plain_no_negotiation()
    await case_b_deflate_only()
    await case_c_zstd_negotiated()
    await case_d_plain_subprotocol()
    await case_e_client_side_garbage_rejected()
    print("WS 门协商矩阵全部通过")


if __name__ == "__main__":
    asyncio.run(main())
