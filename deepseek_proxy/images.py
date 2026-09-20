"""Bounded asynchronous image fetching with public-network URL policy."""

import base64
import ipaddress
import socket

import aiohttp

from .errors import ProxyError


class PublicResolver(aiohttp.abc.AbstractResolver):
    def __init__(self):
        self.resolver = aiohttp.resolver.DefaultResolver()

    async def resolve(self, host, port=0, family=socket.AF_INET):
        records = await self.resolver.resolve(host, port, family)
        if any(not ipaddress.ip_address(r["host"]).is_global for r in records):
            raise ValueError("Image URL must resolve to a public address")
        return records

    async def close(self):
        await self.resolver.close()


async def load_image(messages, max_bytes):
    urls = [
        p["image_url"]["url"]
        for m in messages
        if isinstance(m.content, list)
        for p in m.content
        if p.get("type") == "image_url"
    ]
    if not urls:
        return None
    if len(urls) > 1:
        raise ProxyError(
            "Only one image per turn is supported", 400, "unsupported_parameter", "messages"
        )
    url = urls[0]
    try:
        if url.startswith("data:image/"):
            header, encoded = url.split(",", 1)
            if not header.endswith(";base64") or len(encoded) > (max_bytes + 2) // 3 * 4:
                raise ValueError("Invalid or oversized image data")
            data = base64.b64decode(encoded, validate=True)
        else:
            from urllib.parse import urlsplit

            parsed = urlsplit(url)
            if parsed.scheme not in ("http", "https") or parsed.username or not parsed.hostname:
                raise ValueError("Invalid image URL")
            try:
                address = ipaddress.ip_address(parsed.hostname)
            except ValueError:
                address = None
            if address and not address.is_global:
                raise ValueError("Image URL must be public")
            connector = aiohttp.TCPConnector(resolver=PublicResolver())
            async with aiohttp.ClientSession(
                connector=connector, timeout=aiohttp.ClientTimeout(total=10)
            ) as session:
                async with session.get(url, allow_redirects=False) as response:
                    if response.status != 200:
                        raise ValueError("Image download failed or redirected")
                    data = bytearray()
                    async for chunk in response.content.iter_chunked(65536):
                        data.extend(chunk)
                        if len(data) > max_bytes:
                            raise ValueError("Image exceeds size limit")
                    data = bytes(data)
        if not data or len(data) > max_bytes:
            raise ValueError("Empty or oversized image")
        return data
    except (ValueError, aiohttp.ClientError, TimeoutError) as exc:
        raise ProxyError("Invalid or unavailable image", 400, "invalid_image", "messages") from exc
