# qubeherd

Shows how many coding agents are working, idle or blocked — and how much of the
Claude Code limits is already spent — on the screen of an Ergohaven Qube dongle.

```
herdr.sock ──events.subscribe (+ agent.list)──┐
                                              ├─▶ qubeherd ──raw HID 0xB0/0xB1──▶ Qube dongle ──▶ screen
status line ──rate_limits─────────────────────┘
```

[herdr](https://herdr.dev) tracks the state of every agent pane it manages.
This daemon subscribes to its socket API and pushes the aggregate into the
keyboard firmware, which renders it as a 2×2 grid between the two battery
gauges:

```
┌────────────────────────────────┐
│ 5H ▓▓▓▓░░░░░░░░░░░░░░░░   24%  │
│ 7D ▓░░░░░░░░░░░░░░░░░░░    2%  │
│ ▎  WORKING      BLOCKED     ▎  │
│ ▎     1                     ▎  │
│ ▎  IDLE         DONE        ▎  │
│ ▎     2            1        ▎  │
└────────────────────────────────┘
```

A zero count draws no number, so an idle screen reads as quiet. The gauges on
the edges are the per-half batteries — position says which half, so they carry
no `L`/`R` letter. The two bars on top are the Claude Code limit windows, the
5-hour session over the 7-day one.

## Requirements

- **Firmware with the agent and usage packets.** Stock RMK ignores packet types
  `0xB0` and `0xB1`;
  you need a build from the `feat/k04-agent-status` branch of
  [AlexBSoD/rmk](https://github.com/AlexBSoD/rmk) or later.
- **Write access to the raw HID node.** The Ergohaven udev rules already grant
  it (`/dev/hidraw*` for vendor `0xE126`, group `input`). Check with
  `qubeherd --once`: it prints which node it picked.
- **A running herdr server** — the socket defaults to
  `$HERDR_SOCKET_PATH`, falling back to `~/.config/herdr/herdr.sock`.
- **A status line that harvests the limits**, for the two bars only. Claude
  Code hands its status-line command the session JSON on every render; a
  harvester writes the `rate_limits` object to
  `$XDG_RUNTIME_DIR/claude-usage.json`, which is what this daemon reads. Claude
  Code has exactly one status-line slot, so the harvesting belongs in whatever
  script already owns it:

  ```bash
  jq -c '{five_hour:  .rate_limits.five_hour,
         seven_day:  .rate_limits.seven_day,
         written_at: (now | floor)}' <<<"$input" > "$tmp"
  mv -f "$tmp" "${XDG_RUNTIME_DIR:-/tmp}/claude-usage.json"
  ```

  Write through a temporary file and rename, so a read never catches a half
  written one, and skip the write when `rate_limits` is absent — it is missing
  before a session's first API response and for accounts without a Pro or Max
  subscription, and blanking the cache there would blank the bars on every
  start. Without a harvester the daemon falls back to `~/.claude.json` and
  flags what it finds as stale.

## Usage

```console
$ qubeherd --once --verbose      # push the current state once, then exit
[INFO ] qubeherd 0.2.0
[INFO ] writing to /dev/hidraw3 (Ergohaven Qube)
[INFO ] layout: ru
[INFO ] agents: 1 working, 2 idle, 0 blocked, 0 done, 0 unknown
[INFO ] limits: 5h 24%, 7d 2%

$ qubeherd                       # stay running
$ nix run .                      # …or straight from the flake
```

| Flag              | Effect                                                      |
| ----------------- | ----------------------------------------------------------- |
| `--socket <path>` | herdr API socket, overriding the default discovery          |
| `--device <path>` | write to this hidraw node instead of searching for one      |
| `--once`          | push the current state once and exit                        |
| `--no-clock`      | leave the header clock to Entropy                           |
| `--no-usage`      | leave the Claude Code limit bars empty                       |
| `--usage-harvest` | read the harvested limits from this file                     |
| `--claude-config` | fallback config, when no status line has run yet             |
| `--no-layout`     | do not sync the keyboard layout (Universal Symbols need it) |
| `--verbose`       | log every packet                                            |

`RUST_LOG` is honoured on top of the flags, so detail can be raised on a
running service without editing the unit — `RUST_LOG=qubeherd::layout=debug`
narrows it to one module instead of drowning the journal in zbus internals.

Build it with `cargo build --release`, or `nix develop` for a shell with the
toolchain. As a home-manager module:

```nix
{
  inputs.qubeherd.url = "github:AlexBSoD/qubeherd";
  # ...
  imports = [ inputs.qubeherd.homeModules.default ];
  services.qubeherd.enable = true;
}
```

The unit is `Type=notify` with `WatchdogSec=90`: a wedged process is otherwise
indistinguishable from an idle one, and `Restart=on-failure` alone would never
hear about it. It is bound to `graphical-session.target`, because the layout
source lives on the desktop session bus.

## How it behaves

- **Events are hints, `agent.list` is the truth.** Any pane event triggers a
  fresh `agent.list` call rather than incremental bookkeeping, so a missed or
  reordered event cannot leave the screen wrong.
- **Debounced by 300 ms.** Agent state flaps between tool calls, and herdr
  emits `pane.updated` for scrolling too; without this the screen would flicker.
  A burst pulls the next beat in, it never pushes it out.
- **Heartbeat every 10 s.** The firmware expires the counts after 30 s, so a
  dead daemon shows `NO AGENT FEED` instead of a stale number. A packet is only
  sent when the counts change or the heartbeat comes due.
- **Every socket and bus call is bounded (5 s).** A unix socket has no read
  timeout of its own and zbus applies none either, so a peer that accepts a
  connection without answering — or a name owned by a wedged kwin — would park
  the whole event loop with no error to recover from.
- **Reconnects with backoff** (1 s → 30 s) if herdr restarts, and reopens the
  HID node if the dongle is unplugged and plugged back in. A session that
  lasted a minute resets the backoff, so a boot race that saturated it does not
  keep charging 30 s for a one-second herdr restart hours later.
- **Picks the hidraw node deterministically.** Nodes are matched on the raw-HID
  report descriptor rather than a product id (so Qube/mini/micro all work) and
  sorted numerically, because a lexicographic listing puts `hidraw10` before
  `hidraw2` and would silently flip the pick between replugs. A wired half
  attached alongside the dongle matches just as well, so several matches are
  logged as a warning — `--device` settles it.
- **Reads the Claude Code limits off disk** (packet `0xB1`), from the file the
  status line harvests. Claude Code has no command that reports them and no
  pollable endpoint — `/api/oauth/usage` answers 429 to anything that tries
  ([claude-code#31637](https://github.com/anthropics/claude-code/issues/31637)).
  What it does have is a status-line command that receives `rate_limits` on
  every render, so that is where the numbers come from. `~/.claude.json` is
  kept as a fallback for a machine where no session has rendered yet, and it is
  marked stale on sight: its `cachedUsageUtilization` block only refreshes when
  `/usage` is actually run.
- **Says when a reading stopped moving.** The percentages only advance while a
  local Claude Code session renders; usage spent in a browser or on another
  machine never reaches this daemon. A harvest older than 30 minutes is sent
  with the stale flag, and the screen dims the bars rather than dropping them.
  A window whose own `resets_at` has passed is reported as `0` instead —
  an empty window is a fact, not a guess. Genuinely unreadable goes out as
  `0xFF` and draws `--`, never a confident `0%`.
- **Sends the header clock too** (packet `0xAA`, on every minute rollover and
  after reopening the dongle). Entropy sends the same packet, but only while
  its GUI is running — without either, the header sits at `--:--`. Pass
  `--no-clock` to leave the clock to Entropy.
- **Syncs the host keyboard layout** (packet `0xAC`), following KDE's
  `org.kde.KeyboardLayouts` D-Bus signals. This one is not cosmetic: Universal
  Symbols resolve their keycodes against the layout the firmware believes is
  active, so without it the keyboard types the wrong characters — see
  [ergohaven/entropy#140](https://github.com/ergohaven/entropy/issues/140).
  `--no-layout` opts out; a layout the firmware has no code for (a third one
  such as `de`) leaves the last known one in place.
- **Follows edits to the layout list, not just switches.** An index is a
  position in that list, so a reorder that keeps the same length would map `ru`
  onto the `en` code and stay wrong until a restart.
- **Survives starting before the desktop session.** A missing layout service is
  one warning, not a failure: the clock and the agent counts keep running while
  the daemon looks again every 30 s, so starting before Plasma — or outliving a
  Plasma restart — costs nothing.
- **Replays clock and layout on every (re)open of the device.** A keyboard that
  just rebooted starts from `HostLayout::English` and an empty clock, and
  nothing on the firmware side expires either — so a reflash or a replug would
  otherwise leave Universal Symbols wrong until the next layout switch. The
  layout is also refreshed every 10 s as a backstop against a lost packet.
- **Says what it has been doing every 10 minutes.** A healthy daemon is
  otherwise silent, which makes silence useless as a signal: a spin, a freeze
  and an idle afternoon all read the same in the journal. The summary reports
  wake-ups, herdr events, packets, device opens and session restarts as rates —
  a few hundred wake-ups an hour is normal, millions is a bug the watchdog
  cannot catch, because a spinning loop pings it very cheerfully.

## Wire format

32-byte output report on the QMK-compatible raw HID interface (usage page
`0xFF60`, usage `0x61`), preceded by a `0x00` report id byte for hidraw:

| Byte | Meaning                     |
| ---- | --------------------------- |
| 0    | `0xB0` — agent summary      |
| 1    | protocol version (`0x01`)   |
| 2    | agents working              |
| 3    | agents idle                 |
| 4    | agents blocked              |
| 5    | agents done                 |
| 6    | agents in an unknown state  |
| 7    | reserved flags, must be `0` |

The limit bars are their own packet:

| Byte | Meaning                                        |
| ---- | ---------------------------------------------- |
| 0    | `0xB1` — subscription limits                    |
| 1    | protocol version (`0x01`)                       |
| 2    | 5-hour window, percent used, `0xFF` if unknown  |
| 3    | 7-day window, same encoding                     |
| 4    | flags: bit 0 = nothing refreshed this lately     |

The firmware swallows a packet whose version byte it does not know rather than
misreading it, and keeps the unknown count without giving it a cell on screen.

Two packets predate this daemon and are reused as-is: `0xAA` (`[1]` hour,
`[2]` minute) for the header clock and `0xAC` (`[1]` layout: `0` = English,
`1` = Russian) for Universal Symbols.

The firmware side lives in `rmk/src/host/via/mod.rs` (packet parsing),
`rmk/src/host_data.rs` (the 30 s expiry, shared by both feeds) and
`keyboards/k04/src/qube_display.rs` (the screen itself).
