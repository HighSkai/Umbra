"""Umbra helper for Blink, using blinkpy.

Only blinkpy's OAuth login/refresh and its raw API calls are used. Its higher-level
`Blink.refresh()` is avoided on purpose: it downloads thumbnails and video clips on
every update, which Umbra never wants.

Commands (argv[1]):
  login     - OAuth sign-in with email + password, then the emailed/texted PIN.
  poll      - homescreen (online, Wi-Fi, armed) + recent media metadata (last motion).
  snapshot  - ask a camera for a fresh thumbnail and return the JPEG.
  live      - start Blink's live stream and serve its MPEG-TS on a local TCP port
              for ffmpeg; stops when ffmpeg disconnects or the core says "close".
"""

from __future__ import annotations

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import asyncio  # noqa: E402
import base64  # noqa: E402
import time  # noqa: E402
from types import SimpleNamespace  # noqa: E402
from urllib.parse import urljoin  # noqa: E402

from common import Inbox, emit, error, read_first, run, short, to_ts  # noqa: E402

# Homescreen list -> (id prefix, blinkpy camera_type for API calls)
KINDS = {"cameras": ("cam", ""), "owls": ("mini", "mini"), "doorbells": ("doorbell", "doorbell")}
TYPE_FOR_PREFIX = {v[0]: v[1] for v in KINDS.values()}
NOT_MOTION = {"snapshot", "liveview", "manual", "thumbnail", "live_view"}
DING_SOURCES = {"button_press", "doorbell", "ring"}


class NeedLogin(Exception):
    pass


def emit_token(auth) -> None:
    attrs = {k: v for k, v in auth.login_attributes.items() if k not in ("username", "password")}
    emit({"type": "token", "token": attrs})


def make(session, inp):
    from blinkpy.auth import Auth
    from blinkpy.blinkpy import Blink

    data = dict(inp.get("token") or {})
    data.pop("username", None)
    data.pop("password", None)
    if inp.get("hardware_id"):
        data["hardware_id"] = inp["hardware_id"]
    blink = Blink(session=session)
    blink.auth = Auth(data, no_prompt=True, session=session)
    return blink


async def ensure_token(blink) -> None:
    """Refresh the access token if it expires within 10 minutes; never fall back to a password login."""
    from blinkpy import api

    auth = blink.auth
    if not auth.refresh_token:
        raise NeedLogin()
    if auth.token is None or auth.expiration_date is None or auth.expiration_date - time.time() < 600:
        data = await api.oauth_refresh_token(auth, auth.refresh_token, auth.hardware_id)
        if not data:
            raise NeedLogin()
        await auth._process_token_data(data)
        emit_token(auth)
    if not (auth.host and auth.region_id and auth.account_id):
        auth.tier_info = await auth.get_tier_info()
        auth.extract_tier_info()
        emit_token(auth)
    blink.setup_urls()


def split_id(camera_id: str):
    prefix, network, cid = camera_id.split("-", 2)
    return prefix, network, cid


def find(home: dict, camera_id: str):
    prefix, network, cid = split_id(camera_id)
    for key, (p, _) in KINDS.items():
        if p != prefix:
            continue
        for c in home.get(key) or []:
            if str(c.get("id")) == cid and str(c.get("network_id")) == network:
                return c
    return None


def thumbnail_url(blink, c: dict) -> str | None:
    thumb = c.get("thumbnail")
    if not thumb:
        return None
    try:
        int(thumb)
        path = (
            f"/api/v3/media/accounts/{blink.account_id}/networks/{c.get('network_id')}/"
            f"{c.get('type')}/{c.get('id')}/thumbnail/thumbnail.jpg?ts={thumb}&ext="
        )
    except (TypeError, ValueError):
        path = thumb if str(thumb).endswith("&ext=") else f"{thumb}.jpg"
    return urljoin(blink.urls.base_url, path)


async def login(inp) -> int:
    from aiohttp import ClientSession
    from blinkpy.auth import Auth, BlinkTwoFARequiredError, LoginError

    async with ClientSession() as session:
        data = {"username": inp.get("username", ""), "password": inp.get("password", ""), "hardware_id": inp.get("hardware_id")}
        auth = Auth(data, no_prompt=True, session=session)
        try:
            try:
                await auth.startup()
            except BlinkTwoFARequiredError:
                emit({"type": "need_2fa", "message": "Blink sent you a PIN by text or email. Enter it here."})
                msg = await Inbox().get(timeout=600)
                code = str(msg.get("code", "")).strip()
                if not code or not await auth.complete_2fa_login(code):
                    error("auth_failed", "Blink didn't accept that PIN. Start again to get a new one.")
                    return 1
            if not auth.refresh_token:
                error("auth_failed", "Blink didn't return a token.")
                return 1
            emit_token(auth)
            emit({"type": "done"})
            return 0
        except LoginError:
            error(
                "auth_failed",
                "Blink refused the sign-in. Check the email and password. Since October 2026 Blink has also been rejecting some "
                "non-browser sign-ins (blinkpy issue #1329); if this keeps happening, wait for a blinkpy update.",
            )
        except asyncio.TimeoutError:
            error("timeout", "No PIN was entered within 10 minutes.")
        except Exception as exc:
            error("network", f"Couldn't reach Blink: {short(exc)}")
    return 1


async def poll(inp) -> int:
    from aiohttp import ClientSession
    from blinkpy import api

    async with ClientSession() as session:
        blink = make(session, inp)
        try:
            await ensure_token(blink)
            await blink.get_homescreen()
            home = blink.homescreen or {}
            networks = {str(n.get("id")): n for n in home.get("networks") or []}
            media = await api.request_videos(blink, time=int(time.time()) - 86400, page=1) or {}
            events: dict[str, list] = {}
            for m in media.get("media") or []:
                ts = to_ts(m.get("created_at"))
                source = str(m.get("source") or "").lower()
                if ts is None or source in NOT_MOTION:
                    continue
                kind = "ding" if source in DING_SOURCES else "motion"
                events.setdefault(str(m.get("device_id")), []).append({"kind": kind, "ts": ts})
            cameras = []
            for key, (prefix, _) in KINDS.items():
                for c in home.get(key) or []:
                    net = networks.get(str(c.get("network_id")), {})
                    status = str(c.get("status") or "").lower()
                    signals = c.get("signals") or {}
                    evs = events.get(str(c.get("id")), [])
                    wifi = c.get("wifi_strength", signals.get("wifi"))
                    cameras.append(
                        {
                            "id": f"{prefix}-{c.get('network_id')}-{c.get('id')}",
                            "name": c.get("name") or "Blink camera",
                            "model": f"Blink {c.get('type') or prefix}",
                            "online": True if status in ("online", "done") else False if status == "offline" else None,
                            "wired": True if prefix == "mini" else None,
                            "battery": None,
                            "wifi_rssi": wifi if isinstance(wifi, int) and wifi < 0 else None,
                            "wifi_bars": wifi if isinstance(wifi, int) and 0 <= wifi <= 5 else None,
                            # Mini-only networks report arming at the network level (blinkpy #1328).
                            "armed": bool(net.get("armed")) if net else c.get("enabled"),
                            "doorbell": prefix == "doorbell",
                            "last_motion": max((e["ts"] for e in evs if e["kind"] == "motion"), default=None),
                            "last_ding": max((e["ts"] for e in evs if e["kind"] == "ding"), default=None),
                            "events": evs,
                        }
                    )
            emit({"type": "result", "cameras": cameras})
            return 0
        except NeedLogin:
            error("auth_expired", "login expired, reconnect")
        except Exception as exc:
            error("api_changed", f"Unexpected response from Blink: {short(exc)}")
    return 1


async def snapshot(inp) -> int:
    from aiohttp import ClientSession
    from blinkpy import api

    async with ClientSession() as session:
        blink = make(session, inp)
        try:
            await ensure_token(blink)
            prefix, network, cid = split_id(inp.get("camera_id", ""))
            resp = await api.request_new_image(blink, network, cid, camera_type=TYPE_FOR_PREFIX.get(prefix, ""))
            if isinstance(resp, dict) and resp.get("id"):
                await api.wait_for_command(blink, resp)
            await asyncio.sleep(6)  # homescreen requests are throttled to one per 5 s
            await blink.get_homescreen()
            cam = find(blink.homescreen or {}, inp.get("camera_id", ""))
            url = thumbnail_url(blink, cam or {})
            if not url:
                error("not_found", "Blink didn't return a thumbnail for that camera.")
                return 1
            r = await api.http_get(blink, url=url, stream=True, json=False)
            if r is None or r.status != 200:
                error("api_error", "Couldn't download the snapshot.")
                return 1
            emit({"type": "snapshot", "jpeg_b64": base64.b64encode(await r.read()).decode()})
            return 0
        except NeedLogin:
            error("auth_expired", "login expired, reconnect")
        except Exception as exc:
            error("api_error", f"Snapshot failed: {short(exc)}")
    return 1


async def live(inp) -> int:
    from aiohttp import ClientSession
    from blinkpy import api
    from blinkpy.livestream import BlinkLiveStream

    async with ClientSession() as session:
        blink = make(session, inp)
        stream = None
        try:
            await ensure_token(blink)
            await blink.get_homescreen()
            camera_id = inp.get("camera_id", "")
            prefix, network, cid = split_id(camera_id)
            cam = find(blink.homescreen or {}, camera_id) or {}
            resp = await api.request_camera_liveview(blink, network, cid, camera_type=TYPE_FOR_PREFIX.get(prefix, ""))
            if not isinstance(resp, dict) or not str(resp.get("server", "")).startswith("immis://"):
                error("unsupported", "Blink didn't offer a live stream this client can read.")
                return 1
            shim = SimpleNamespace(serial=cam.get("serial") or f"{network}-{cid}", network_id=network, sync=SimpleNamespace(blink=blink))
            stream = BlinkLiveStream(shim, resp)
            await stream.start(host="127.0.0.1", port=0)
            emit({"type": "ready", "url": stream.url})
            feed = asyncio.create_task(stream.feed())
            inbox = Inbox()
            while not feed.done():
                try:
                    msg = await inbox.get(timeout=5)
                except asyncio.TimeoutError:
                    continue
                if msg.get("type") in ("close", "eof"):
                    break
            stream.stop()
            try:
                await asyncio.wait_for(feed, 10)
            except Exception:
                pass
            return 0
        except NeedLogin:
            error("auth_expired", "Blink login expired, reconnect")
        except Exception as exc:
            error("stream", f"Live view failed: {short(exc)}")
        finally:
            if stream is not None:
                stream.stop()
            emit({"type": "closed"})
    return 1


COMMANDS = {"login": login, "poll": poll, "snapshot": snapshot, "live": live}


async def main() -> int:
    cmd = sys.argv[1] if len(sys.argv) > 1 else ""
    if cmd not in COMMANDS:
        error("usage", f"unknown command {cmd!r}")
        return 2
    inp = read_first()
    try:
        import blinkpy  # noqa: F401
    except ImportError as exc:
        error("helper_missing", f"blinkpy isn't installed in the helper venv ({short(exc)})")
        return 1
    return await COMMANDS[cmd](inp)


if __name__ == "__main__":
    run(main)
