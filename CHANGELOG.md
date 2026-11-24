# Changelog

All notable changes to waldo are recorded in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- A key sent right after `<Esc>` in one request could be lost when the
  program was busy as the two arrived: it read them together and took them
  for an Alt chord. htop did this at each of its refreshes. What follows an
  Escape now waits until the program has read the Escape.
- The status line had no `fg=` while a pipeline ran whose first program
  had exited, such as `seq 1 100 | less`, and the hint called it 'a
  command'. Such a job is now named after the one program left in it. When
  several are left it has no name, and the hint says so.

### Changed

- The hint for a program that left the foreground during a request now
  says that what was typed after the keys that ended it may have been
  lost, and a wait for a pattern that runs out in that state gives it too.
- The hint for a settle while a command still has the terminal is
  reworded: "'make' is running in the foreground. If you do not expect to
  interact with it, wait for it to end: waldo-term wait --until-exit". It
  used to give advice that was wrong for a pager or an editor, and pointed
  at a help topic where it now names the command.
- `waldo-term help tips` no longer advises a pause after Escape. Items 7
  and 8 say what does lose keys: a prompt that discards keys that are not
  its answers, and a program that is exiting.

### Removed

- `waldo-term signal --stop`. `signal` sends a signal and nothing else.
  `waldo-term kill` ends a session and waits until it has ended, which
  `--stop` did not: a `gc`, or a new session of the same name, right after
  it could find the old one still running. `--stop` was also one dash away
  from `signal STOP`, which suspends the program. `waldo-term` says to use
  `kill` if given `--stop`, and `waldo-wrap` names `kill` when a session
  name is taken. The API's stop request is unchanged.

## [0.2.0] - 2026-11-22

Several clients on one session can now talk to each other, and a wait can
be for more than one thing. The way wait conditions are written has
changed, in the command line and in the API, with no compatibility for the
old form.

### Added

- **Broadcasts.** A client can say a line to the other clients of a
  session without typing it into the program. See `waldo-term help
  sharing` and PROTOCOL.md section 4.10.
  - `waldo-term broadcast TEXT` (or `say`) sends one.
  - A client that names itself with `--as NAME` or `WALDO_AS` is given
    what the others said, above the status line of its next response. A
    name is given each broadcast once, never its own, and the last five
    the first time the host hears from it.
  - `--until-broadcast` waits to be spoken to, and
    `--until-broadcast-from NAME` for one sender.
  - Each broadcast goes to the session's destination and records how it
    arrived (`via`: `attach` or `api`). `GET /v1/broadcasts` lists the
    last 32.
  - `waldo-wrap --no-broadcast` starts a session without any of this.
- **A conversation panel in `waldo-term attach`.** While viewing, `s` opens
  a panel with the recent broadcasts and a line to write on. Names are
  coloured, long lines wrap, and the arrow and page keys scroll back. The
  newest unread broadcast stays in the status bar until the panel is
  opened, and a viewer who arrives late is given the conversation so far.
- **Several wait conditions at once.** A wait returns on whichever is met
  first, and the status line says which. `--until-match '^rc='
  --until-broadcast` waits for a marker while still hearing other clients.
- `--until-match` may be given several times, for any of several texts.
- `--until-match-new RE`: a pattern for which text already on screen never
  counts, even on a request that types nothing.
- `--until-elapsed MS`: a length of time as a wait condition. The reason is
  `elapsed` and the exit status 0, where running into `--timeout` is a
  failure.
- `waldo-term status`: what the session is, from the host's status
  endpoint. The 0.1.0 notes listed this command, but it did not exist.

### Changed

- **Wait conditions are spelled differently, in the command line and in
  the API.** Each is an option of its own that takes its own argument:
  `--until-match RE`, `--until-settle`, `--until-change`, `--until-exit`,
  `--until-broadcast`, `--until-elapsed MS`, `--until-none`. In the API
  they are the wait parameters `until_match` and so on. `--until NAME`,
  `--pattern`, and the parameters `until`, `pattern` and `since_gen` are
  gone. `waldo-term` says what to use if given `--until` or `--pattern`,
  and the host refuses the old `until` parameter with `bad_until`.
- The error `unknown_until` is now `bad_until`. `missing_pattern` is only
  for triggers; a wait with an empty pattern gets `bad_pattern`.
- **`waldo-term attach` has two modes and keeps one key for itself.**
  Ctrl-] switches between viewing and typing, and Ctrl-C leaves the viewer
  while viewing. Before, Ctrl-] left from either mode and Ctrl-\ switched
  typing on and off; Ctrl-\ now goes to the program like any other key,
  and `q` no longer leaves. The message on leaving is "left 'ID'; it is
  still running".

### Fixed

- A request that typed input could be answered before all of it was typed,
  when its pattern matched the echo of its own text. The rest, usually the
  Enter, was then typed during the next request.

## [0.1.0] - 2026-11-21

The first release.

### Added

- `waldo-wrap`: runs a program in a pty with a terminal emulator
  (libghostty-vt 1.0.1, linked statically) and serves a control API, HTTP
  over a unix socket, until the program exits. Options for detaching,
  naming the session, the terminal size, lingering after exit, and
  recording the output as an asciicast.
- `waldo-term`: the command-line client. `send`, `screen`, `output`,
  `wait`, `list`, `resize`, `signal`, `kill`, `gc`, `match`, `attach` and
  `help`. (The notes published with this release also listed `status`.
  There was no such command until the next release.)
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

[Unreleased]: https://github.com/Gremlin-OS/waldo/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/Gremlin-OS/waldo/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/Gremlin-OS/waldo/releases/tag/v0.1.0
