# qubeherd

Shows how many coding agents are working, idle or blocked on the screen of an
Ergohaven Qube dongle.

```
herdr.sock ──events.subscribe──▶ qubeherd ──raw HID 0xB0──▶ Qube dongle ──▶ screen
              (+ agent.list)
```

[herdr](https://herdr.dev) tracks the state of every agent pane it manages.
This daemon subscribes to its socket API and pushes the aggregate into the
keyboard firmware, which renders it as the middle panel of the dongle UI:

```
┌──────────────────────────────┐
│ 12:34                  BASE  │
│   ●  1   WORKING             │
│   ▲  0   BLOCKED             │
│   ○  2   IDLE                │
│ L 87%   R 92%                │
└──────────────────────────────┘
```

## Requirements

- **Firmware with the agent packet.** Stock RMK ignores packet type `0xB0`;
  you need a build from the `feat/k04-herdr-agent-status` branch of
  [AlexBSoD/rmk](https://github.com/AlexBSoD/rmk) or later.
- **Write access to the raw HID node.** The Ergohaven udev rules already grant
  it (`/dev/hidraw*` for vendor `0xE126`, group `input`). Check with
  `qubeherd --once`: it prints which node it picked.
- **A running herdr server** — the socket defaults to
  `$HERDR_SOCKET_PATH`, falling back to `~/.config/herdr/herdr.sock`.

## Usage

```console
$ qubeherd --once --verbose      # push the current state once, then exit
[INFO ] writing to /dev/hidraw3
[INFO ] layout: ru
[INFO ] agents: 1 working, 2 idle, 0 blocked, 0 done, 0 unknown

$ qubeherd                       # stay running
$ nix run .                      # …or straight from the flake
```

Build it with `cargo build --release`, or `nix develop` for a shell with the
toolchain. As a home-manager module:

```nix
{
  inputs.qubeherd.url = "path:/home/uzz/projects/qubeherd";
  # ...
  imports = [ inputs.qubeherd.homeModules.default ];
  services.qubeherd.enable = true;
}
```

## How it behaves

- **Events are hints, `agent.list` is the truth.** Any pane event triggers a
  fresh `agent.list` call rather than incremental bookkeeping, so a missed or
  reordered event cannot leave the screen wrong.
- **Debounced by 300 ms.** Agent state flaps between tool calls, and herdr
  emits `pane.updated` for scrolling too; without this the screen would flicker.
- **Heartbeat every 10 s.** The firmware expires the counts after 30 s, so a
  dead daemon shows `NO AGENT FEED` instead of a stale number. A packet is only
  sent when the counts change or the heartbeat comes due.
- **Reconnects with backoff** (1 s → 30 s) if herdr restarts, and reopens the
  HID node if the dongle is unplugged and plugged back in.
- **Sends the header clock too** (packet `0xAA`, on every minute rollover and
  after reopening the dongle). Entropy sends the same packet, but only while
  its GUI is running — without either, the header sits at `--:--`. Pass
  `--no-clock` to leave the clock to Entropy.
- **Syncs the host keyboard layout** (packet `0xAC`), following KDE's
  `org.kde.KeyboardLayouts` D-Bus signal. This one is not cosmetic: Universal
  Symbols resolve their keycodes against the layout the firmware believes is
  active, so without it the keyboard types the wrong characters — see
  [ergohaven/entropy#140](https://github.com/ergohaven/entropy/issues/140).
  `--no-layout` opts out; non-KDE sessions warn once and carry on.
- **Replays clock and layout on every (re)open of the device.** A keyboard
  that just rebooted starts from `HostLayout::English` and an empty clock, and
  nothing on the firmware side expires either — so a reflash or a replug would
  otherwise leave Universal Symbols wrong until the next layout switch. The
  layout is also refreshed every 10 s as a backstop against a lost packet.

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

Two packets predate this daemon and are reused as-is: `0xAA` (`[1]` hour,
`[2]` minute) for the header clock and `0xAC` (`[1]` layout: `0` = English,
`1` = Russian) for Universal Symbols.

The firmware side lives in `rmk/src/host/via/mod.rs` and `rmk/src/host_data.rs`.
