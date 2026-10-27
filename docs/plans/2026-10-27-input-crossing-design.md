# orrery milestone 5: input crossing (design)

Fills in section 4 of `2026-10-26-orrery-design.md`. Agreed 2026-10-27.

## Focus decides

- The session's focus marker (milestone 4) now also decides where input goes. With the marker on the machine the user
  is at, input works as always; with it elsewhere, that machine's keyboard and pointer input travels over the link
  and the receiver runs it through the same path as its own input (binds, stacks, clients). `workspace N` (keys or
  msg) moves the marker, and with it the keyboard.
- A bind that changes the session, typed on a satellite that has the input, goes to the primary (which routes it as
  in milestone 4). Binds that act on the satellite's own windows run there.
- Only the primary forwards in this milestone (it is the machine the user sits at in the common case); a satellite's
  own keyboard keeps working locally.

## Keyboard

- Raw keycodes travel. At handoff the sender's keymap (as an xkb keymap string) goes along once, and the layout it is
  on at every handoff, so the user's layout follows them.
- Handoff is explicit: the machine losing input releases every key it had down (to its seat and clients); the one
  gaining input is told which modifiers are held. No stuck keys, no missing Super.
- Key repeat happens on the receiver (clients repeat from the seat's repeat info); nothing repeats over the network.

## Pointer

- Screens sit in a row, left to right, in session monitor order (the primary's, then each satellite's in join order).
  Pushing the pointer past the right edge of a machine's last screen crosses to the next machine's first; past the
  left edge of a machine's first screen, to the previous machine's last. With three or more machines the pointer
  walks the whole row both ways.
- Crossing out: the sender hides its cursor and holds the pointer (nested: the host's pointer lock; headless: nothing
  to hold) and sends relative motion, buttons and high-resolution scroll. The receiver moves its own cursor, entering
  at the same height (scaled to its screen). A satellite knows only its own screens: when its pointer leaves them it
  tells the primary which edge and at what height (InputExit), and the primary hands the pointer to the machine
  beside that edge (itself included). Leaving past either end of the row goes nowhere. (As built: while it has the
  primary's input the receiver draws the pointer, the client's cursor surface or a built-in square for a named
  cursor; with its own input the host's cursor stands for it.)
- Motion is coalesced: if the link stalls, pending movement merges into one step and is never replayed late. Keys and
  buttons are never merged or dropped. (As built: the receiver acknowledges the input it ran and the sender keeps at
  most 32 input messages unacknowledged; kernel buffers alone cannot bound this, a stalled receiver's kernel keeps
  taking data.)

## Safety

- `Super+Escape` (bind action `reclaim`) is handled locally and never forwarded: it pulls keyboard and pointer back to
  the machine the user is at, and the marker with them.
- A link drop while input is away: the sender reclaims at once (releasing what the receiver had down on the receiver
  side happens there, on its own drop handling).
- Keystrokes travel only inside the encrypted link and are never logged (plain typing never appears in `last_key`).

## State and testing

- `state.session.input`: where keyboard and pointer go now; `latency_ms` as in milestone 4.
- Headless pairs over loopback with input injected into one (a test-only `inject` request, refused unless built for
  tests or started with a debug flag) and checked at a test client on the other: handoff mid-keypress, held
  modifiers, a link drop with keys down, the escape key, the pointer crossing and crossing back, coalesced motion.
- Live test on one machine: a nested primary and a nested satellite side by side in the host.
