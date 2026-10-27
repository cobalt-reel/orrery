# orrery milestone 4: the link and satellites (design)

Fills in section 3 of `2026-10-26-orrery-design.md`. Input crossing is milestone 5: here the keyboard and pointer stay
on the machine they are plugged into. Agreed 2026-10-27.

## Config

- Primary: `listen "ADDR:PORT" secret-file="PATH"`. It listens only with this line.
- Satellite: `primary "HOST:PORT" secret-file="PATH"`.
- `name "..."` names the machine; default: the hostname. Names are unique in a session (a duplicate is refused at
  the hello).
- The secret file holds 32 to 4096 bytes (whitespace at either end aside) and gives group and others no permission
  (refused otherwise, with the path in the error); its contents are never logged or shown in `state`.
- `listen`, `primary`, `name` and the `secret-file` path take effect when orrery starts (a reload says so). A reload
  reads the secret file's contents again: a different secret drops every link, and satellites get back in once they
  have it too; one that cannot be read is the `config_error`, and the old one stays.

## The link

- TCP, one connection per satellite, opened by the satellite.
- Encrypted and authenticated with Noise (`Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s`) on the pre-shared secret: a wrong
  secret fails the handshake and nothing is sent in clear. Messages are length-prefixed frames.
- **Hello** (satellite → primary): protocol version, machine name, its screens (name, number) and workspace numbers.
  Sizes do not cross: nothing on the primary needs them until milestone 5. A version mismatch or duplicate name is
  refused with a clear message, shown on both ends (either end can send Refused, then closes).
- **Welcome** (primary → satellite): the renames for its numbers and the numbers other machines own (its foreign
  numbers). No session table crosses: a machine needs only what it may not use.
- **Then:** the satellite reports its screens and workspaces as they change; the primary answers each report (and
  any change in the session) with Renames: renames and foreign numbers. The primary reports its own to its session
  too, so a number made on two machines at once heals on the next report. The primary sends Commands (a routed
  `workspace N`, `orrery msg --to`); the satellite answers each, in one frame (about 64 KB; one too big is answered
  with an error saying so).
- **Latency:** each end pings every 2 s, one ping in flight at most; `state` shows the round trip per satellite,
  unknown while a ping is overdue. A link silent for 10 s is dropped.
- **Limits:** a connection must be open (handshake and Hello) within 5 s; the primary takes at most 8 connections
  still joining, 2 from one IP address. A Hello naming a satellite whose link is silent for 4 s takes its place
  instead of being refused.
- **Drop:** the satellite keeps working as a normal window manager for its own windows and reconnects (1 s, backing
  off to 5 s). The primary drops the satellite's monitors from the session and frees their workspace numbers.

## The session

- Monitors: the primary's screens first, then each satellite's screens in the order they join. A screen's number is
  its workspace's, kept or renamed with it.
- Workspaces are numbered across the session: workspace k is locked to monitor k while it exists.
- **Numbers never change on a drop:** a satellite that loses the link keeps every workspace number it has; the
  primary only frees those numbers. On a (re)join a satellite's workspaces keep their numbers where they are free;
  a number already taken in the session moves to the lowest free one (the first join is the common case).
- `workspace N` on the primary for a workspace on another machine: that machine shows it, and the session's focus
  marker moves there (the keyboard stays where it is until milestone 5). A workspace that does not exist is created
  on the machine holding the marker, on its focused monitor. A satellite does not route: `workspace N` there for
  another machine's number is refused (switch to it from the primary).
- `move-to-workspace N` to a workspace on another machine is refused with a message: windows never cross machines.
- `orrery msg --to MACHINE <request>`: run a request on that machine (through the primary; an error after 5 s
  without an answer). On a satellite only the satellite itself can be named. `state` on the primary shows the
  session: machines, their monitors and workspaces, link latency, the focus marker; on a satellite, itself.

## Testing

Two (or three) headless orrerys on one machine, linked over loopback with temp secret files: hello, wrong secret,
version mismatch, duplicate name, session numbering, `workspace N` across machines, `--to`, refusal of cross-machine
moves, drop and reconnect with numbers kept and freed, collisions on rejoin. The real two-machine run is by hand.
