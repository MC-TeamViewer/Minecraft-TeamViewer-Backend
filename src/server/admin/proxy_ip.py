from __future__ import annotations

import ipaddress
import logging
import os
from functools import lru_cache

logger = logging.getLogger("teamviewrelay.admin_proxy_ip")


def parse_bool_env(name: str, default: bool = False) -> bool:
    raw = os.getenv(name)
    if raw is None:
        return default
    return str(raw).strip().lower() in {"1", "true", "yes", "on"}


def get_remote_addr_from_client(client) -> str | None:
    host = getattr(client, "host", None)
    if isinstance(host, str) and host.strip():
        return host
    return None


def normalize_ip_text(value: str | None) -> str | None:
    text = str(value or "").strip().strip('"')
    if not text:
        return None

    if text.startswith("[") and "]" in text:
        text = text[1:text.index("]")]

    try:
        return str(ipaddress.ip_address(text))
    except ValueError:
        pass

    if text.count(":") == 1 and "." in text:
        host, _, _port = text.rpartition(":")
        try:
            return str(ipaddress.ip_address(host))
        except ValueError:
            return None

    return None


def extract_client_ip_from_headers(headers) -> str | None:
    if headers is None:
        return None

    x_forwarded_for = headers.get("x-forwarded-for")
    if isinstance(x_forwarded_for, str):
        for item in x_forwarded_for.split(","):
            normalized = normalize_ip_text(item)
            if normalized:
                return normalized

    x_real_ip = headers.get("x-real-ip")
    if isinstance(x_real_ip, str):
        normalized = normalize_ip_text(x_real_ip)
        if normalized:
            return normalized

    return None


@lru_cache(maxsize=8)
def _parse_trusted_proxy_networks(raw_value: str) -> tuple[ipaddress._BaseNetwork, ...]:
    networks: list[ipaddress._BaseNetwork] = []

    for raw_item in str(raw_value).split(","):
        item = raw_item.strip()
        if not item:
            continue
        try:
            networks.append(ipaddress.ip_network(item, strict=False))
        except ValueError:
            logger.warning("Ignore invalid TEAMVIEWER_TRUSTED_PROXY_CIDRS entry: %s", item)

    return tuple(networks)


def get_trusted_proxy_networks() -> tuple[ipaddress._BaseNetwork, ...]:
    raw_value = os.getenv(
        "TEAMVIEWER_TRUSTED_PROXY_CIDRS",
        "127.0.0.1/32,::1/128,172.16.0.0/12",
    )
    return _parse_trusted_proxy_networks(str(raw_value))


def should_trust_proxy_headers(client_host: str | None) -> bool:
    if not parse_bool_env("TEAMVIEWER_TRUST_PROXY_HEADERS", False):
        return False

    normalized = normalize_ip_text(client_host)
    if normalized is None:
        return False

    ip_obj = ipaddress.ip_address(normalized)
    return any(ip_obj in network for network in get_trusted_proxy_networks())


def get_effective_remote_addr(client, headers=None) -> str | None:
    direct_addr = get_remote_addr_from_client(client)
    if not should_trust_proxy_headers(direct_addr):
        return direct_addr

    forwarded_addr = extract_client_ip_from_headers(headers)
    return forwarded_addr or direct_addr


def get_request_remote_addr(request) -> str | None:
    return get_effective_remote_addr(request.client, request.headers)


def get_websocket_remote_addr(websocket) -> str | None:
    return get_effective_remote_addr(websocket.client, websocket.headers)
