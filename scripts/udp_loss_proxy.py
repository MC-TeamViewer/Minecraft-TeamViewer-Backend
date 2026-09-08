"""用户态 QUIC 弱网代理:在 loopback 上模拟丢包/延迟/抖动/限带宽(无需 root)。

逐包独立判定:以 drop 概率丢弃,否则延迟 delay±jitter 后原样转发。
双向独立;每个客户端地址一条上游 UDP 管道。
`--rate-kib-per-s` 对上游→客户端方向(服务端外部出口带宽)加令牌桶
限速,桶容量 = 1 秒配额;设为 0(默认)不限速。限速仅作用于转发,
不代表端点知晓任何带宽约束——拥塞控制须自行探测。

用法:
  python3 udp_loss_proxy.py --listen 127.0.0.1:8768 \
      --upstream 127.0.0.1:8767 --drop 0.05 --delay-ms 40 --jitter-ms 15
  python3 udp_loss_proxy.py --listen 127.0.0.1:8768 \
      --upstream 127.0.0.1:8767 --rate-kib-per-s 100
"""
import argparse
import asyncio
import random
import time


class RateLimiter:
    """令牌桶:按字节数计费,桶容量 1 秒配额,允许秒内突发。"""

    def __init__(self, rate_bytes_per_sec: float):
        self.rate = rate_bytes_per_sec
        self.tokens = 0.0
        self.updated = time.monotonic()

    async def take(self, cost: int) -> None:
        while True:
            now = time.monotonic()
            self.tokens = min(
                self.tokens + (now - self.updated) * self.rate, self.rate
            )
            self.updated = now
            if self.tokens >= cost:
                self.tokens -= cost
                return
            await asyncio.sleep((cost - self.tokens) / self.rate)


class Pipe:
    """一个客户端地址 ↔ 上游 socket 的双向管道。"""

    def __init__(self, server: "Server", client_addr):
        self.server = server
        self.client_addr = client_addr
        self.transport = None
        # 上游 socket 就绪前到达的包在此排队,就绪后按序补发。
        self.pending: list[bytes] = []
        # 上游→客户端方向的限速队列:单消费者,保序转发
        self.rate_queue: asyncio.Queue[bytes | None] = asyncio.Queue()
        if server.rate_limiter is not None:
            server.loop.create_task(self._rate_pump())

    async def _rate_pump(self) -> None:
        assert self.server.rate_limiter is not None
        while True:
            data = await self.rate_queue.get()
            if data is None:
                return
            await self.server.rate_limiter.take(len(data))
            self.server.relay(
                lambda d=data, a=self.client_addr: self.server.transport.sendto(d, a)
            )

    def client_to_upstream(self, data: bytes) -> None:
        if random.random() < self.server.args.drop:
            return
        if self.transport is None:
            self.pending.append(data)
            return
        self.server.relay(lambda d=data: self.transport.sendto(d))

    def flush_pending(self) -> None:
        for data in self.pending:
            self.server.relay(lambda d=data: self.transport.sendto(d))
        self.pending.clear()

    def upstream_to_client(self, data: bytes) -> None:
        if random.random() < self.server.args.drop:
            return
        if self.server.rate_limiter is not None:
            # 限速路径:入队保序,由 _rate_pump 计费后转发
            self.rate_queue.put_nowait(data)
            return
        self.server.relay(
            lambda d=data, a=self.client_addr: self.server.transport.sendto(d, a)
        )


class UpstreamProtocol(asyncio.DatagramProtocol):
    def __init__(self, pipe: Pipe):
        self.pipe = pipe

    def connection_made(self, transport):
        self.pipe.transport = transport
        self.pipe.flush_pending()

    def datagram_received(self, data, addr):
        self.pipe.upstream_to_client(data)


class Server(asyncio.DatagramProtocol):
    def __init__(self, args, upstream_addr, loop):
        self.args = args
        self.upstream_addr = upstream_addr
        self.loop = loop
        self.transport = None
        self.pipes = {}
        rate = args.rate_kib_per_s * 1024 if args.rate_kib_per_s > 0 else 0
        self.rate_limiter = RateLimiter(rate) if rate > 0 else None

    def connection_made(self, transport):
        self.transport = transport

    def relay(self, send) -> None:
        delay = max(
            0.0,
            self.args.delay_ms + random.uniform(-self.args.jitter_ms, self.args.jitter_ms),
        ) / 1000.0
        if delay > 0:
            self.loop.call_later(delay, send)
        else:
            send()

    def datagram_received(self, data, addr):
        pipe = self.pipes.get(addr)
        if pipe is None:
            pipe = Pipe(self, addr)
            self.pipes[addr] = pipe
            asyncio.ensure_future(
                self.loop.create_datagram_endpoint(
                    lambda: UpstreamProtocol(pipe),
                    remote_addr=self.upstream_addr,
                )
            )
        pipe.client_to_upstream(data)


async def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--listen", default="127.0.0.1:8768")
    parser.add_argument("--upstream", default="127.0.0.1:8767")
    parser.add_argument("--drop", type=float, default=0.05)
    parser.add_argument("--delay-ms", type=float, default=40.0)
    parser.add_argument("--jitter-ms", type=float, default=15.0)
    parser.add_argument(
        "--rate-kib-per-s",
        type=float,
        default=0.0,
        help="上游→客户端方向令牌桶限速(KiB/s);0 = 不限速",
    )
    args = parser.parse_args()
    listen_host, listen_port = args.listen.rsplit(":", 1)
    up_host, up_port = args.upstream.rsplit(":", 1)
    loop = asyncio.get_running_loop()
    transport, _ = await loop.create_datagram_endpoint(
        lambda: Server(args, (up_host, int(up_port)), loop),
        local_addr=(listen_host, int(listen_port)),
    )
    rate_text = (
        f"rate={args.rate_kib_per_s:g}KiB/s" if args.rate_kib_per_s > 0 else "rate=off"
    )
    print(
        f"UDP 弱网代理 {args.listen} -> {args.upstream} "
        f"drop={args.drop:.0%} delay={args.delay_ms}ms±{args.jitter_ms}ms {rate_text}",
        flush=True,
    )
    try:
        await asyncio.Event().wait()
    finally:
        transport.close()


if __name__ == "__main__":
    asyncio.run(main())
