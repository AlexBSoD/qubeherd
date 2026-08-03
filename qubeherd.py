#!/usr/bin/env python3
"""Push herdr agent-state counts to the Ergohaven Qube dongle screen.

herdr knows which coding agents are working, idle or blocked. The Qube
dongle has a 280x240 screen sitting right under your hands. This bridges
the two: subscribe to herdr's socket API, and on every state change push a
32-byte raw-HID packet the RMK firmware turns into the agent panel.

The firmware expires the counts after 30s, so the heartbeat below is not
optional decoration — without it the screen goes to "NO AGENT FEED".
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import re
import select
import socket
import subprocess
import sys
import time
from pathlib import Path

# Wire format, mirrored in rmk/src/host/via/mod.rs.
PACKET_TYPE = 0xB0
PACKET_VERSION = 0x01
PACKET_LEN = 32
# The header clock uses the same host-data channel. Entropy sends this too, but
# only while its GUI runs — and the screen shows "--:--" until somebody does.
CLOCK_PACKET_TYPE = 0xAA
# Universal Symbols resolve keycodes against the host's active layout. Without
# this packet the firmware keeps its own stale idea of it and types the wrong
# characters, so this is a correctness fix, not decoration.
LAYOUT_PACKET_TYPE = 0xAC
# Layout codes the firmware understands (universal_symbols::HostLayout).
LAYOUT_CODES = ("en", "ru")
# Re-send the layout this often even when nothing changed: unlike the agent
# counts it never expires, so a single lost packet would be silent and lasting.
LAYOUT_REFRESH_SECONDS = 60.0
# Agent states, in the order the firmware reads them out of the packet.
STATES = ("working", "idle", "blocked", "done", "unknown")

# Ergohaven vendor id; the Qube dongle and the wired halves share it.
HID_VENDOR_ID = 0xE126
# Usage page 0xFF60 / usage 0x61 — the QMK-compatible raw HID interface.
RAW_HID_DESCRIPTOR_PREFIX = bytes((0x06, 0x60, 0xFF, 0x09, 0x61))

# Agent states flap between tool calls; coalesce a burst into one packet.
DEBOUNCE_SECONDS = 0.3
# Well under the firmware's 30s expiry, so one dropped packet is harmless.
HEARTBEAT_SECONDS = 10.0
RECONNECT_MIN_SECONDS = 1.0
RECONNECT_MAX_SECONDS = 30.0
# Wait before touching the dongle again after a failed write (usually unplugged).
RETRY_SECONDS = 5.0
REQUEST_TIMEOUT_SECONDS = 2.0

# `pane.agent_status_changed` needs a pane_id, so it would mean tracking
# subscriptions per pane as they come and go. `pane.updated` is global and
# already carries agent_status, which is all this daemon needs.
SUBSCRIPTIONS = (
    "pane.updated",
    "pane.created",
    "pane.closed",
    "pane.exited",
    "pane.agent_detected",
)

log = logging.getLogger("qubeherd")


class HerdrClient:
    """One short-lived request/response call over the herdr socket API."""

    def __init__(self, socket_path: Path):
        self.socket_path = socket_path

    def call(self, method: str, params: dict | None = None) -> dict:
        request = {
            "id": f"qubeherd:{method}",
            "method": method,
            "params": params or {},
        }
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.settimeout(REQUEST_TIMEOUT_SECONDS)
            client.connect(str(self.socket_path))
            client.sendall((json.dumps(request) + "\n").encode())
            buffer = b""
            while b"\n" not in buffer:
                chunk = client.recv(65536)
                if not chunk:
                    raise ConnectionError(f"herdr closed the socket during {method}")
                buffer += chunk
        response = json.loads(buffer.split(b"\n", 1)[0])
        if "error" in response:
            raise ConnectionError(f"{method} failed: {response['error']}")
        return response.get("result", {})


def summarize(agents: list[dict]) -> dict[str, int]:
    """Count agents per state, saturating at the packet's one-byte fields."""
    counts = {state: 0 for state in STATES}
    for agent in agents:
        state = agent.get("agent_status") or "unknown"
        if state not in counts:
            state = "unknown"
        counts[state] = min(counts[state] + 1, 255)
    return counts


def build_packet(counts: dict[str, int]) -> bytes:
    payload = bytearray(PACKET_LEN)
    payload[0] = PACKET_TYPE
    payload[1] = PACKET_VERSION
    for index, state in enumerate(STATES):
        payload[2 + index] = counts[state]
    return bytes(payload)


def build_clock_packet(hour: int, minute: int) -> bytes:
    payload = bytearray(PACKET_LEN)
    payload[0] = CLOCK_PACKET_TYPE
    payload[1] = hour
    payload[2] = minute
    return bytes(payload)


def build_layout_packet(code: int) -> bytes:
    payload = bytearray(PACKET_LEN)
    payload[0] = LAYOUT_PACKET_TYPE
    payload[1] = code
    return bytes(payload)


def normalize_layout(xkb_name: str) -> int | None:
    """Map an xkb layout name onto the firmware's layout code."""
    name = re.split(r"[-_.:(@]", xkb_name.strip())[0].lower()
    if name in ("en", "us", "gb", "uk", "au", "ca"):
        return LAYOUT_CODES.index("en")
    if name.startswith("russian") or name == "ru":
        return LAYOUT_CODES.index("ru")
    return None


class KdeLayoutSource:
    """Follows the active keyboard layout via KDE's D-Bus interface.

    Uses `busctl` rather than a D-Bus binding to keep this daemon on the
    standard library; `busctl monitor --json=short` emits one JSON object per
    signal, which is all the parsing this needs.
    """

    SERVICE = ("org.kde.keyboard", "/Layouts", "org.kde.KeyboardLayouts")

    def __init__(self) -> None:
        self.monitor: subprocess.Popen[str] | None = None
        self.names: list[str] = []

    def _call(self, member: str) -> str | None:
        service, path, interface = self.SERVICE
        try:
            result = subprocess.run(
                ["busctl", "--user", "call", service, path, interface, member],
                capture_output=True,
                text=True,
                timeout=REQUEST_TIMEOUT_SECONDS,
            )
        except (OSError, subprocess.TimeoutExpired) as err:
            log.debug("busctl %s failed: %s", member, err)
            return None
        return result.stdout.strip() if result.returncode == 0 else None

    def available(self) -> bool:
        return self._call("getLayout") is not None

    def _refresh_names(self) -> None:
        # Reply looks like: a(sss) 2 "us" "" "English (US)" "ru" "" "Russian"
        reply = self._call("getLayoutsList")
        if reply is None:
            return
        quoted = re.findall(r'"([^"]*)"', reply)
        self.names = quoted[::3]

    def code_for_index(self, index: int) -> int | None:
        if index >= len(self.names):
            self._refresh_names()
        if index >= len(self.names):
            return None
        return normalize_layout(self.names[index])

    def current(self) -> int | None:
        reply = self._call("getLayout")
        if reply is None:
            return None
        match = re.search(r"\bu\s+(\d+)", reply)
        return self.code_for_index(int(match.group(1))) if match else None

    def start_monitor(self) -> None:
        _, _, interface = self.SERVICE
        self.monitor = subprocess.Popen(
            [
                "busctl",
                "--user",
                "monitor",
                "--json=short",
                # A positional argument here would be read as a service name.
                f"--match=type='signal',interface='{interface}'",
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )

    def fileno(self) -> int | None:
        if self.monitor is None or self.monitor.stdout is None:
            return None
        return self.monitor.stdout.fileno()

    def read_signal(self) -> int | None:
        """Consume one signal line; returns the new layout code, if any."""
        if self.monitor is None or self.monitor.stdout is None:
            return None
        line = self.monitor.stdout.readline()
        if not line:
            raise ConnectionError("busctl monitor exited")
        try:
            signal = json.loads(line)
        except json.JSONDecodeError:
            return None
        if signal.get("member") == "layoutListChanged":
            self._refresh_names()
            return self.current()
        if signal.get("member") != "layoutChanged":
            return None
        data = signal.get("payload", {}).get("data") or []
        return self.code_for_index(int(data[0])) if data else None

    def close(self) -> None:
        if self.monitor is not None:
            self.monitor.terminate()
            self.monitor = None


def find_raw_hid_device() -> Path | None:
    """Locate the keyboard's raw-HID node by walking sysfs.

    Matching on the report descriptor rather than a fixed product id keeps
    this working across the Qube/mini/micro variants, which differ in pid.
    """
    for entry in sorted(Path("/sys/class/hidraw").glob("hidraw*")):
        uevent = entry / "device" / "uevent"
        descriptor = entry / "device" / "report_descriptor"
        try:
            hid_id = uevent.read_text()
            report_descriptor = descriptor.read_bytes()
        except OSError:
            continue
        match = re.search(r"HID_ID=[0-9A-Fa-f]+:([0-9A-Fa-f]+):", hid_id)
        if not match or int(match.group(1), 16) != HID_VENDOR_ID:
            continue
        if not report_descriptor.startswith(RAW_HID_DESCRIPTOR_PREFIX):
            continue
        return Path("/dev") / entry.name
    return None


class QubeWriter:
    """Writes packets to the dongle, reopening it across unplug/replug."""

    def __init__(self) -> None:
        self.device: int | None = None
        self.path: Path | None = None

    def _open(self) -> bool:
        path = find_raw_hid_device()
        if path is None:
            return False
        try:
            self.device = os.open(path, os.O_WRONLY)
        except OSError as err:
            log.warning("cannot open %s: %s", path, err)
            return False
        self.path = path
        log.info("writing agent status to %s", path)
        return True

    def close(self) -> None:
        if self.device is not None:
            os.close(self.device)
        self.device = None
        self.path = None

    def write(self, payload: bytes) -> bool:
        if self.device is None and not self._open():
            return False
        # The interface has no report id, so hidraw wants a leading 0x00.
        try:
            os.write(self.device, b"\x00" + payload)
            log.debug("sent %s", payload[: 2 + len(STATES)].hex(" "))
            return True
        except OSError as err:
            log.warning("write to %s failed (%s); will reopen", self.path, err)
            self.close()
            return False


def default_socket_path() -> Path:
    from_env = os.environ.get("HERDR_SOCKET_PATH")
    if from_env:
        return Path(from_env)
    config_home = os.environ.get("XDG_CONFIG_HOME") or (Path.home() / ".config")
    return Path(config_home) / "herdr" / "herdr.sock"


def run(socket_path: Path, once: bool, clock: bool, layout: bool) -> int:
    client = HerdrClient(socket_path)
    writer = QubeWriter()
    layouts = KdeLayoutSource() if layout else None
    if layouts is not None and not layouts.available():
        log.warning("no KDE layout service on the bus; Universal Symbols will not be synced")
        layouts = None

    if once:
        counts = summarize(client.call("agent.list").get("agents", []))
        log.info("agents: %s", counts)
        sent = writer.write(build_packet(counts))
        if clock:
            local = time.localtime()
            sent = writer.write(build_clock_packet(local.tm_hour, local.tm_min)) and sent
        if layouts is not None:
            code = layouts.current()
            if code is not None:
                log.info("layout: %s", LAYOUT_CODES[code])
                sent = writer.write(build_layout_packet(code)) and sent
        return 0 if sent else 1

    backoff = RECONNECT_MIN_SECONDS
    last_counts: dict[str, int] | None = None
    last_clock: tuple[int, int] | None = None
    last_layout: int | None = None
    last_layout_sent = 0.0
    last_sent = time.monotonic() - HEARTBEAT_SECONDS
    if layouts is not None:
        layouts.start_monitor()
    while True:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as events:
                events.connect(str(socket_path))
                subscribe = {
                    "id": "qubeherd:subscribe",
                    "method": "events.subscribe",
                    "params": {"subscriptions": [{"type": name} for name in SUBSCRIPTIONS]},
                }
                events.sendall((json.dumps(subscribe) + "\n").encode())
                buffer = b""
                while b"\n" not in buffer:
                    chunk = events.recv(65536)
                    if not chunk:
                        raise ConnectionError("herdr closed the socket during subscribe")
                    buffer += chunk
                acknowledgement, buffer = buffer.split(b"\n", 1)
                reply = json.loads(acknowledgement)
                if "error" in reply:
                    raise ConnectionError(f"subscribe rejected: {reply['error']}")
                log.info("subscribed to herdr at %s", socket_path)
                backoff = RECONNECT_MIN_SECONDS

                # Push once up front: the screen should be right before the
                # first agent happens to change state.
                deadline = time.monotonic()
                while True:
                    now = time.monotonic()
                    timeout = max(0.0, deadline - now)
                    layout_fd = layouts.fileno() if layouts is not None else None
                    watched = [events] + ([layout_fd] if layout_fd is not None else [])
                    readable, _, _ = select.select(watched, [], [], timeout)

                    if layout_fd is not None and layout_fd in readable:
                        # Layout changes must reach the keyboard immediately —
                        # every keystroke until then produces wrong symbols.
                        code = layouts.read_signal()
                        if code is not None and writer.write(build_layout_packet(code)):
                            if code != last_layout:
                                log.info("layout: %s", LAYOUT_CODES[code])
                            last_layout, last_layout_sent = code, now
                        continue

                    if events in readable:
                        chunk = events.recv(65536)
                        if not chunk:
                            raise ConnectionError("herdr closed the event stream")
                        buffer += chunk
                        # Any event is just a hint; the authoritative state
                        # comes from agent.list below, so the exact payloads
                        # do not matter — only that something moved.
                        moved = b"\n" in buffer
                        buffer = buffer.rsplit(b"\n", 1)[-1] if moved else buffer
                        if moved:
                            deadline = min(deadline, now + DEBOUNCE_SECONDS)
                        continue

                    # Nothing expires the layout on the firmware side, so a lost
                    # packet would stay lost: refresh it periodically and after
                    # every reopen of the dongle.
                    if layouts is not None and (
                        now - last_layout_sent >= LAYOUT_REFRESH_SECONDS or writer.device is None
                    ):
                        code = layouts.current()
                        if code is not None and writer.write(build_layout_packet(code)):
                            if code != last_layout:
                                log.info("layout: %s", LAYOUT_CODES[code])
                            last_layout, last_layout_sent = code, now

                    # The firmware keeps the clock until something replaces it,
                    # so this only needs to fire when the minute rolls over —
                    # or after a reopen, where the dongle has forgotten it.
                    if clock:
                        local = time.localtime()
                        # `device is None` means the next write reopens the
                        # dongle, which by then has lost the time we set.
                        if (local.tm_hour, local.tm_min) != last_clock or writer.device is None:
                            if writer.write(build_clock_packet(local.tm_hour, local.tm_min)):
                                last_clock = (local.tm_hour, local.tm_min)
                            else:
                                last_clock = None

                    counts = summarize(client.call("agent.list").get("agents", []))
                    now = time.monotonic()
                    # herdr emits pane.updated for scrolling and resizes too,
                    # so most wake-ups carry no news: only spend a USB packet
                    # on a real change or on keeping the firmware's TTL alive.
                    if counts != last_counts or now - last_sent >= HEARTBEAT_SECONDS:
                        if counts != last_counts:
                            log.info("agents: %s", counts)
                        if writer.write(build_packet(counts)):
                            last_counts = counts
                            last_sent = now
                        else:
                            # Dongle unplugged or busy; back off instead of
                            # spinning on a device that is not there.
                            deadline = now + RETRY_SECONDS
                            continue
                    deadline = last_sent + HEARTBEAT_SECONDS
        except (ConnectionError, OSError, json.JSONDecodeError) as err:
            writer.close()
            if layouts is not None:
                layouts.close()
                layouts.start_monitor()
            log.warning("herdr connection lost (%s); retrying in %.0fs", err, backoff)
            time.sleep(backoff)
            backoff = min(backoff * 2, RECONNECT_MAX_SECONDS)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--socket",
        type=Path,
        default=default_socket_path(),
        help="herdr API socket (default: $HERDR_SOCKET_PATH or ~/.config/herdr/herdr.sock)",
    )
    parser.add_argument(
        "--once",
        action="store_true",
        help="send a single packet from the current agent list and exit",
    )
    parser.add_argument("--verbose", action="store_true", help="log every packet")
    parser.add_argument(
        "--no-clock",
        dest="clock",
        action="store_false",
        help="leave the header clock to Entropy instead of sending it",
    )
    parser.add_argument(
        "--no-layout",
        dest="layout",
        action="store_false",
        help="do not sync the host keyboard layout (Universal Symbols need it)",
    )
    args = parser.parse_args()

    logging.basicConfig(
        level=logging.DEBUG if args.verbose else logging.INFO,
        format="%(levelname)s %(message)s",
    )
    try:
        return run(args.socket, args.once, args.clock, args.layout)
    except KeyboardInterrupt:
        return 0


if __name__ == "__main__":
    sys.exit(main())
