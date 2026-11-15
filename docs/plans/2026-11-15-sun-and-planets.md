# Sun and planets (the names of the linked machines)

The linked machines were a *session* of a *primary* and *satellites*. "Session" already means a login session and a
Wayland session, and the program is named after a model of the solar system. So the names follow it: orrery on
several machines makes an **orrery**, which has a **sun** (the machine whose keyboard and mouse you use) and
**planets**.

## Design

- **Only the linking terms change.** Slots, tabs, stacks, workspaces and layouts keep their plain names: they are
  what people type and search for.
- **A planet is any machine that joined a sun**, on bare metal or nested: the name is the role in the link, which is
  all the code distinguishes. The topology stays a flat star, so nothing orbits a planet and there is no third name.
- **What users and scripts see changes with it:**
  - config: `sun "HOST:PORT" secret-file=...` on a planet (was `primary`); `listen` and `name` are unchanged. The old
    keyword is an error that names the new one.
  - `state`: the `orrery` object (was `session`), `"role": "sun" | "planet"`, and a planet's `sun` address (was
    `primary`).
  - the event stream: the `orrery` event (was `session`).
  - messages and errors: "the sun", "a planet".
- **In the code** the whole is `System` (module `system`), as in solar system, because `Orrery` is one machine's
  compositor state; `Role::Sun` and `Role::Planet`. Fields and variants that are serialized are named for what a
  reader sees (`orrery`).
- **The link protocol** renames one message (`orrery-request`), so its version goes from 3 to 4: every machine of an
  orrery must run the same build, as before.
- **Other meanings are left alone:** a seat or login session, the host's session of a nested orrery, the primary GPU
  and the primary plane.

The earlier notes in this directory keep the words they were written with: read "session", "primary" and
"satellite" there as "orrery", "sun" and "planet".

## Plan and tests

1. Rename across the source, tests, examples and the manual, word by word in context (comments and strings as
   prose, identifiers as code), leaving the other meanings above.
2. The config keyword with its error for the old name, and a test for it.
3. The serialized names, the kitty bar that reads them, and their tests.
4. README, manual and checklists read through again by hand.

## As built

As designed. The protocol error for a message sent the wrong way now reads `` `input-exit` sent to a planet ``
rather than "a input-exit to a planet".
