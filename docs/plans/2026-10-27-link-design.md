# orrery milestone 4: the link and satellites (design)

Fills in section 3 of `2026-10-26-orrery-design.md`. Input crossing is milestone 5: here the keyboard and pointer stay
on the machine they are plugged into. Agreed 2026-10-27.

## Config

- Primary: `listen "ADDR:PORT" secret-file="PATH"`. It listens only with this line.
- Satellite: `primary "HOST:PORT" secret-file="PATH"`.
- `name "..."` names the machine; default: the hostname. Names are unique in a session (a duplicate is refused at
  the hello).
- The secret file must not be readable by group or others (refused otherwise, with the path in the error); its
  contents are never logged or shown in `state`.

## The link

- TCP, one connection per satellite, opened by the satellite.
- Encrypted and authenticated with Noise (`Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s`) on the pre-shared secret: a wrong
  secret fails the handshake and nothing is sent in clear. Messages are length-prefixed frames.
- **Hello** (satellite → primary): protocol version, machine name, its screens (name, size) and workspaces. A
  version mismatch or duplicate name is refused with a clear message, shown on both ends.
- **Then:** the satellite sends its workspace state as it changes; the primary sends the session's workspace
  table and the commands `orrery msg` gives for that machine.
- **Latency:** the link pings every 2 s; `state` shows the round trip per satellite.
- **Drop:** the satellite keeps working as a normal window manager for its own windows and reconnects (backoff up to
  5 s). The primary drops the satellite's monitors from the session and frees their workspace numbers.

## The session

- Monitors: the primary's screens first (its own numbering), then each satellite's screens in the order they join.
- Workspaces are numbered across the session: workspace k is locked to monitor k while it exists.
- **Numbers never change on a drop:** a satellite that loses the link keeps every workspace number it has; the
  primary only frees those numbers. On a (re)join a satellite's workspaces keep their numbers where they are free;
  a number already taken in the session moves to the lowest free one (the first join is the common case).
- `workspace N` for a workspace on another machine: that machine shows it, and the session's focus marker moves
  there (the keyboard stays where it is until milestone 5). A workspace that does not exist is created on the
  focused monitor, as today.
- `move-to-workspace N` to a workspace on another machine is refused with a message: windows never cross machines.
- `orrery msg --to MACHINE <request>`: run a request on that machine (through the primary). `state` on the primary
  shows the session: machines, their monitors and workspaces, link latency.

## Testing

Two (or three) headless orrerys on one machine, linked over loopback with temp secret files: hello, wrong secret,
version mismatch, duplicate name, session numbering, `workspace N` across machines, `--to`, refusal of cross-machine
moves, drop and reconnect with numbers kept and freed, collisions on rejoin. The real two-machine run is by hand.
