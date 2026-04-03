"""
集成测试 - 旧版本 MessagePack 客户端兼容性

测试服务端能够解析旧版本 MessagePack 握手包并正确拒绝连接

运行方式:
    # 1. 先启动服务端
    cd /path/to/project && uv run src/main.py

    # 2. 然后运行测试
    pytest tests/ -v
"""

import pytest
import asyncio
import msgpack
import websockets
import sys
from pathlib import Path

# 添加 src 目录到 Python 路径
sys.path.insert(0, str(Path(__file__).parent.parent / "src"))


# 配置
DEFAULT_URI = "ws://localhost:8765/mc-client"
DEFAULT_TIMEOUT = 10.0  # 秒


@pytest.fixture
def server_uri():
    """服务端 URI fixture"""
    return DEFAULT_URI


class TestLegacyMsgpackHandshake:
    """旧版本 MessagePack 握手包兼容性测试"""

    @pytest.mark.asyncio
    async def test_legacy_version_rejected(self, server_uri):
        """
        测试旧版本 MessagePack 握手包被正确拒绝

        预期行为:
        1. 服务端能够解析 MessagePack 格式的握手包
        2. 服务端拒绝握手并返回版本不兼容错误
        3. 错误信息提示客户端需要升级
        """
        # 构造旧版本的握手包 (MessagePack 格式)
        legacy_handshake = {
            "type": "handshake",
            "networkProtocolVersion": "0.5.0",
            "localProgramVersion": "teamviewer-client-0.5.2",
            "roomCode": "test-room",
            "submitPlayerId": "test-player-123",
        }

        # 使用 MessagePack 编码
        packed_data = msgpack.packb(legacy_handshake, use_bin_type=True)

        async with websockets.connect(server_uri, open_timeout=DEFAULT_TIMEOUT) as websocket:
            await websocket.send(packed_data)

            # 接收响应
            response_bytes = await websocket.recv()

            # 尝试用 MessagePack 解码响应
            response = msgpack.unpackb(response_bytes, raw=False)

            # 验证响应结构
            assert response.get("type") == "handshake_ack", \
                f"Expected handshake_ack, got {response.get('type')}"

            # 验证握手被拒绝
            assert response.get("ready") is False, \
                "服务端不应该接受旧版本握手"

            # 验证包含错误信息
            error_type = response.get("error", "")
            reject_reason = response.get("rejectReason", "")

            assert error_type, "应该包含错误类型"
            assert reject_reason, "应该包含拒绝原因"

            # 验证错误信息包含版本/协议相关提示
            assert "version" in error_type.lower() or "protocol" in error_type.lower() or \
                   "version" in reject_reason.lower() or "protocol" in reject_reason.lower(), \
                f"错误信息应包含版本/协议提示: error={error_type}, reason={reject_reason}"

    @pytest.mark.asyncio
    async def test_much_old_version_rejected(self, server_uri):
        """
        测试更旧版本的 MessagePack 握手包被拒绝

        测试版本 0.4.x 的兼容性处理
        """
        legacy_handshake = {
            "type": "handshake",
            "networkProtocolVersion": "0.4.7",
            "localProgramVersion": "teamviewer-client-0.4.7",
            "roomCode": "test-room",
        }

        packed_data = msgpack.packb(legacy_handshake, use_bin_type=True)

        async with websockets.connect(server_uri, open_timeout=DEFAULT_TIMEOUT) as websocket:
            await websocket.send(packed_data)
            response_bytes = await websocket.recv()
            response = msgpack.unpackb(response_bytes, raw=False)

            assert response.get("type") == "handshake_ack"
            assert response.get("ready") is False

    @pytest.mark.asyncio
    async def test_connection_closed_for_incompatible(self, server_uri):
        """
        测试某些旧版本会导致连接关闭而非返回握手响应

        部分极旧版本可能不被 MessagePack 解析器支持
        """
        # 使用非常旧的格式
        old_handshake = msgpack.packb({
            "type": "handshake",
            "version": "0.1.0",  # 旧字段名
        }, use_bin_type=True)

        try:
            async with websockets.connect(server_uri, open_timeout=DEFAULT_TIMEOUT) as websocket:
                await websocket.send(old_handshake)

                # 等待响应或连接关闭
                try:
                    response_bytes = await asyncio.wait_for(
                        websocket.recv(),
                        timeout=5.0
                    )
                    # 如果收到响应，验证是拒绝消息
                    response = msgpack.unpackb(response_bytes, raw=False)
                    assert response.get("type") == "handshake_ack"
                    assert response.get("ready") is False
                except asyncio.TimeoutError:
                    pytest.fail("等待响应超时")
        except websockets.exceptions.ConnectionClosedError as e:
            # 连接关闭是可以接受的（表示服务端拒绝了连接）
            assert e.code is not None, "应该包含关闭码"


class TestLegacyMsgpackEdgeCases:
    """旧版本 MessagePack 边界情况测试"""

    @pytest.mark.asyncio
    async def test_missing_required_fields(self, server_uri):
        """
        测试缺少必需字段的握手包

        验证服务端对不完整握手的处理
        """
        incomplete_handshake = {
            "type": "handshake",
            # 缺少 version 和 roomCode
        }

        packed_data = msgpack.packb(incomplete_handshake, use_bin_type=True)

        async with websockets.connect(server_uri, open_timeout=DEFAULT_TIMEOUT) as websocket:
            await websocket.send(packed_data)

            try:
                response_bytes = await asyncio.wait_for(websocket.recv(), timeout=5.0)
                response = msgpack.unpackb(response_bytes, raw=False)

                # 服务端可能拒绝也可能接受但带警告
                assert response.get("type") == "handshake_ack"
            except asyncio.TimeoutError:
                # 超时表示服务端未返回响应（可接受）
                pass
            except websockets.exceptions.ConnectionClosedError:
                # 连接关闭也是可接受的
                pass

    @pytest.mark.asyncio
    async def test_empty_payload(self, server_uri):
        """
        测试空 payload

        验证服务端对空数据的处理 - 服务端可能不返回响应或直接关闭连接
        """
        packed_data = msgpack.packb({}, use_bin_type=True)

        async with websockets.connect(server_uri, open_timeout=DEFAULT_TIMEOUT) as websocket:
            await websocket.send(packed_data)

            try:
                # 使用超时避免无限等待
                response_bytes = await asyncio.wait_for(websocket.recv(), timeout=5.0)
                response = msgpack.unpackb(response_bytes, raw=False)

                # 可能返回错误类型
                assert response.get("type") in ("handshake_ack", "error")
            except asyncio.TimeoutError:
                # 超时表示服务端未返回响应（可接受，因为空包无法解析）
                pass
            except websockets.exceptions.ConnectionClosedError:
                # 连接关闭也是可接受的
                pass
