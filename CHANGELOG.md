# Changelog

All notable changes to waldo are recorded in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Broadcasts: a client can say a line to the other clients of a session
  without typing it into the program. `waldo-term broadcast TEXT` sends
  one; a client that names itself with `--as NAME` (or `WALDO_AS`) is given
  what the others said, above the status line of its next response; and
  `--until broadcast` waits to be spoken to. Each broadcast also goes to
  the session's destination. See `waldo-term help sharing` and PROTOCOL.md
  section 4.10.

## [0.1.0] - 2026-11-21

The first release.

### Added

- `waldo-wrap`: runs a program in a pty with a terminal emulator
  (libghostty-vt 1.0.1, linked statically) and serves a control API, HTTP
  over a unix socket, until the program exits. Options for detaching,
  naming the session, the terminal size, lingering after exit, and
  recording the output as an asciicast.
- `waldo-term`: the command-line client. `send`, `screen`, `output`,
  `wait`, `status`, `list`, `resize`, `signal`, `kill`, `gc`, `match`,
  `attach` and `help`.
- Input as text, named keys, bracketed pastes and file contents, in any
  order in one request. A request is typed whole, even when several
  clients type into one session. Signals and resizes queue behind input.
- Caller-controlled timing: `--pause MS` between items and `--key-delay MS`
  between keys.
- Waits: `settle` (the screen going still), `match` (a pattern appears),
  `change`, `exit` (the program being typed into exits) and `none`.
- Responses that say what happened: the reason the wait ended, the
  foreground program, hints on timeouts, `unchanged` when typing changed
  nothing on screen, and `interleaved` when another client typed during
  the request.
- Screens as plain text or with ANSI styling, and output that has scrolled
  off.
- Messages to a destination (a file, a unix socket or an HTTP endpoint):
  pattern matches, settles, stream lines, heartbeats, and a final message
  with how the program ended and its output.
- `attach`: a live view of a session in a terminal, optionally driving it.
- `PROTOCOL.md`: a reference sufficient to write another client or a
  harness integration. Its error table and endpoints are checked by tests.
- Help written for agents: `waldo-term help`, and the topics `tips`,
  `keys`, `wait`, `shell`, `messages`, `exit-status` and `commands`.
- Builds with Nix (`nix build`) or without (`scripts/build-vt.sh`, then
  `cargo build`). Release tarballs for x86_64 and aarch64 Linux.
- Licensed under MIT or Apache 2.0, at your option.

[Unreleased]: https://github.com/Gremlin-OS/waldo/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Gremlin-OS/waldo/releases/tag/v0.1.0
