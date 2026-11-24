"""Umbra helper for Ring, using python-ring-doorbell.

Commands (argv[1]):
  login  - email + password, then a 2FA code; emits the token. The password is
           used once and never stored.
  poll   - device list, health (battery, Wi-Fi) and recent history (dings and
           motion). These are cloud reads; they don't wake the doorbell.
  live   - WebRTC signaling relay for an on-demand live view. Media flows
           directly between Ring and the browser.
"""

from __future__ import annotations

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import asyncio  # noqa: E402
import uuid  # noqa: E402

from common import Inbox, emit, error, read_first, run, short, to_ts  # noqa: E402

USER_AGENT = "Umbra/0.1"


def on_token(token):
    emit({"type": "token", "token": token})


def make_auth(inp, token=None):
    from ring_doorbell import Auth

    return Auth(USER_AGENT, token, on_token, hardware_id=inp.get("hardware_id") or None)


async def login(inp) -> int:
    from ring_doorbell import AuthenticationError, Requires2FAError

    auth = make_auth(inp)
    user, password = inp.get("username", ""), inp.get("password", "")
    try:
        try:
            await auth.async_fetch_token(user, password)
        except Requires2FAError:
            emit({"type": "need_2fa", "message": "Ring sent you a verification code by text, email or your authenticator app. Enter it here."})
            msg = await Inbox().get(timeout=600)
            code = str(msg.get("code", "")).strip()
            if not code:
                error("need_login", "No code was entered.")
                return 1
            try:
                await auth.async_fetch_token(user, password, code)
            except Requires2FAError:
                error("auth_failed", "Ring didn't accept that code. Start again to get a new one.")
                return 1
        emit({"type": "done"})
        return 0
    except AuthenticationError as exc:
        error("auth_failed", f"Ring didn't accept the sign-in ({short(exc)}).")
    except asyncio.TimeoutError:
        error("timeout", "No code was entered within 10 minutes.")
    except Exception as exc:
        error("network", f"Couldn't reach Ring: {short(exc)}")
    finally:
        await auth.async_close()
    return 1


def camera_info(dev) -> dict:
    attrs = getattr(dev, "_attrs", {}) or {}
    health = getattr(dev, "_health_attrs", {}) or {}
    conn = (attrs.get("alerts") or {}).get("connection")
    try:
        model = dev.model
    except Exception:
        model = attrs.get("kind", "")
    battery = None
    try:
        battery = dev.battery_life
    except Exception:
        pass
    wired = None
    if attrs.get("external_connection") is not None:
        wired = bool(attrs.get("external_connection"))
    if health.get("ac_power") in (1, True, "1"):
        wired = True
    family = getattr(dev, "family", "")
    return {
        "id": str(dev.device_api_id),
        "name": dev.name,
        "model": model or "",
        "online": None if conn is None else conn == "online",
        "battery": battery,
        "wired": wired,
        "wifi_rssi": health.get("latest_signal_strength"),
        "doorbell": family in ("doorbots", "authorized_doorbots"),
    }


async def poll(inp) -> int:
    from ring_doorbell import AuthenticationError, Requires2FAError, Ring, RingError, RingTimeout

    if not inp.get("token"):
        error("need_login", "Not signed in to Ring.")
        return 1
    auth = make_auth(inp, inp["token"])
    ring = Ring(auth)
    try:
        await ring.async_update_devices()
        cameras = []
        for dev in ring.video_devices():
            try:
                await dev.async_update_health_data()
            except Exception:
                pass
            info = camera_info(dev)
            events, last_ding, last_motion = [], None, None
            try:
                history = await dev.async_history(limit=30) or []
            except Exception:
                history = []
            for h in history:
                kind, ts = h.get("kind"), to_ts(h.get("created_at"))
                if ts is None:
                    continue
                if kind == "ding":
                    events.append({"kind": "ding", "ts": ts})
                    last_ding = max(last_ding or 0, ts)
                elif kind == "motion":
                    events.append({"kind": "motion", "ts": ts})
                    last_motion = max(last_motion or 0, ts)
            info.update(last_ding=last_ding, last_motion=last_motion, events=events)
            cameras.append(info)
        emit({"type": "result", "cameras": cameras})
        return 0
    except (AuthenticationError, Requires2FAError):
        error("auth_expired", "login expired, reconnect")
    except RingTimeout:
        error("network", "Ring didn't answer in time")
    except RingError as exc:
        error("api_error", f"Ring API error: {short(exc)}")
    except Exception as exc:
        error("api_changed", f"Unexpected response from Ring: {short(exc)}")
    finally:
        await auth.async_close()
    return 1


async def live(inp) -> int:
    from ring_doorbell import AuthenticationError, Requires2FAError, Ring

    auth = make_auth(inp, inp.get("token"))
    ring = Ring(auth)
    session_id = None
    dev = None
    try:
        await ring.async_update_devices()
        dev = ring.get_device_by_api_id(int(inp.get("camera_id", "0")))
        if dev is None or not hasattr(dev, "generate_async_webrtc_stream"):
            error("not_found", "That Ring device doesn't support live view.")
            return 1
        # Ring's own STUN servers only (no third-party STUN).
        ice = [s for s in dev.get_ice_servers() if "google" not in s]
        emit({"type": "ready", "ice_servers": ice})

        def on_message(m):
            if m.answer:
                emit({"type": "answer", "sdp": m.answer})
            elif m.candidate:
                emit({"type": "candidate", "candidate": m.candidate, "mline": m.sdp_m_line_index or 0})
            elif m.error_code or m.error_message:
                emit({"type": "error", "code": "stream", "message": m.error_message or str(m.error_code)})

        inbox = Inbox()
        pending = []
        while True:
            try:
                msg = await inbox.get(timeout=120)
            except asyncio.TimeoutError:
                break  # the core stopped sending keepalives
            kind = msg.get("type")
            if kind == "offer" and session_id is None:
                sdp = msg.get("sdp", "")
                session_id = str(uuid.uuid4())
                await dev.generate_async_webrtc_stream(sdp, session_id, on_message, keep_alive_timeout=120)
                for cand, mline in pending:
                    await dev.on_webrtc_candidate(session_id, cand, mline)
                pending.clear()
            elif kind == "candidate":
                cand, mline = msg.get("candidate", ""), int(msg.get("mline", 0))
                if session_id is None:
                    pending.append((cand, mline))
                else:
                    try:
                        await dev.on_webrtc_candidate(session_id, cand, mline)
                    except Exception:
                        pass
            elif kind == "keepalive" and session_id:
                await dev.keep_alive_webrtc_stream(session_id)
            elif kind in ("close", "eof"):
                break
        return 0
    except (AuthenticationError, Requires2FAError):
        error("auth_expired", "Ring login expired, reconnect")
    except Exception as exc:
        error("stream", f"Live view failed: {short(exc)}")
    finally:
        if dev is not None and session_id:
            try:
                await dev.close_webrtc_stream(session_id)
            except Exception:
                pass
        await auth.async_close()
        emit({"type": "closed"})
    return 1


COMMANDS = {"login": login, "poll": poll, "live": live}


async def main() -> int:
    cmd = sys.argv[1] if len(sys.argv) > 1 else ""
    if cmd not in COMMANDS:
        error("usage", f"unknown command {cmd!r}")
        return 2
    inp = read_first()
    try:
        import ring_doorbell  # noqa: F401
    except ImportError as exc:
        error("helper_missing", f"ring_doorbell isn't installed in the helper venv ({short(exc)})")
        return 1
    return await COMMANDS[cmd](inp)


if __name__ == "__main__":
    run(main)
