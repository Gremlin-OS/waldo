# Waldo design

This document describes what is built, except where a section starts with
"Not built". `TODO.md` lists what is open and what is tested less than it
should be.

Waldo lets an LLM agent (or a human) operate interactive terminal programs
through messages instead of a keyboard. The model to keep in mind: it is a
terminal, like xterm or tmux, with a JSON endpoint where the keyboard would be
and a socket to send the screen to where the framebuffer would be. It is two
Linux programs:

- `waldo-wrap` hosts one program in a pty, tracks its screen with a terminal
  emulator core, and serves a control API on a unix socket. One process per
  wrapped program. No daemon.
- `waldo-term` is the client: it lists sessions, sends keys, prints screens,
  waits for conditions, and attaches a human to a session.

Sections 1 to 8 are the design. Section 9 is the language and emulator
choice.

`PROTOCOL.md` is the reference for the control API, the attach stream, the
messages and the session directory: every request, response, field and
error, with no reasoning. Where the two disagree, `PROTOCOL.md` is checked
against the host by the tests and this file is not.

## 1. Process model

### 1.1 Startup sequence

`waldo-wrap [options] -- PROGRAM [ARGS...]` does these steps in order, and any
failure before step 7 exits non-zero with a one-line error on stderr:

1. Parse arguments. Resolve `PROGRAM` against `PATH` now, so a missing program
   fails here and not after the session is announced.
2. If `--detach`: fork. The parent waits on a pipe for the child's verdict
   (step 7) and exits. The child calls `setsid`, redirects stdin, stdout and
   stderr to `/dev/null`, and sends its log to `<session dir>/log`. The fork
   happens before any thread exists.
3. Create the session directory (section 1.4), take the `lock`, bind and listen
   on `sock`.
4. Probe the destination by delivering the `hello` message (section 5). Fail
   if it can't be delivered. `--no-probe` skips the wait: `hello` is queued
   like any other message and startup continues.
5. Open the `--record` file if one was asked for (section 3.4). Open the pty
   at the session size. Create the emulator.
6. Fork the child: `setsid`, make the pty slave its controlling terminal, dup
   it to fds 0 to 2, `close_range` everything else, set the environment
   (section 1.3), `execve`. Exec failure is reported back through a
   close-on-exec pipe, so `waldo-wrap` knows the real errno. The host gets a
   pidfd for the child and closes its own copy of the slave.
7. Write `meta.json`, then print the session's coordinates on stdout (through
   the pipe to the original parent when detached). This is the only thing
   printed:

   ```
   session=k3f9q2
   socket=/run/user/1000/waldo/k3f9q2/sock
   dest=file:/run/user/1000/waldo/k3f9q2/events.ndjson
   ```

   `socket` is where input goes and `dest` is where messages, ending with the
   will, come out. `--json` prints the same as one JSON object.

Printing last means that a caller who has the coordinates has a session whose
program really started.

In the foreground (no `--detach`), `waldo-wrap` blocks until the child exits
and then exits with the child's code, or 128 plus the signal number.

### 1.2 Event loop

The host is a single-threaded epoll loop over: the pty master, the child's
pidfd, a signalfd (SIGTERM, SIGINT, SIGHUP), the listening socket, and client
connections. Timers are the epoll timeout: each turn of the loop works out
the nearest moment at which something can happen (a deadline, a settle
window closing, a paused key, a trigger or stream message falling due) and
sleeps until then. A timer is set only for something that time alone can
bring about. Waiting for a program's first output sets none, so an idle host
uses no CPU. All pty and
socket I/O is non-blocking. Input for the child sits in a queue and is written
as the pty accepts it, while the loop keeps reading output; writing input with
a blocking call can deadlock against a child that is itself blocked writing
output.

One extra thread owns delivery to the destination (section 5), so a slow or
blocked receiver (including a `file:` on a slow filesystem) can never stall the
pty.

Pty output is fed to the emulator in chunks of at most 4 KiB. After each chunk
the host answers any terminal queries, collects lines that scrolled off (section
6.5), and evaluates triggers.

### 1.3 The child's terminal

- `TERM=xterm-256color`, `COLORTERM=truecolor`. No custom terminfo entry: the
  child may ssh or sudo somewhere that doesn't have it. Capabilities beyond
  that are discovered by query, which is what current programs do.
- The emulator answers terminal queries and the host writes the answers to the
  pty: cursor position, device attributes, mode queries, and so on. This is
  required. Vim sends five queries before drawing; fish 4.1 and later warns and
  pauses without a DA1 answer; neovim warns without a DSR answer.
- Mode 2026 (synchronized output) is reported as supported and honoured, with a
  1 s cap on a hold.
- Clipboard access (OSC 52) is refused. Title changes are recorded. Mouse
  reporting is tracked but waldo sends no mouse input in v1.
- Images are not handled yet (section 6.4).
- `WALDO_SESSION` and `WALDO_SOCKET` are set in the child's environment.
- Default size is 120x30. Bounds are 2x1 to 500x200; zero is rejected. Width
  costs almost nothing, since trailing whitespace is trimmed from screens,
  and 120 columns stops the truncation seen in `top` at 80. Height is what
  costs, because a full-screen program paints every row, so it is only a
  little more than the traditional 24. This is judgement; token counts were
  not measured across sizes.
- Emulator scrollback is capped at 4 MiB. It serves attach and reflow. It is
  not the record of output; the output buffer is (section 6.5).

### 1.4 Sessions on disk

Each session has a directory holding its socket, its lock, `meta.json` and
its log (`PROTOCOL.md` section 1 has the layout). With the default destination the directory also holds `events.ndjson`, and it
stays after the session ends so the will can be read; `waldo-term gc` removes
it later. With an explicit `--dest`, the host removes the directory when it
exits cleanly after delivering the will.

- The base directory is `$WALDO_DIR`, else `$XDG_RUNTIME_DIR/waldo`, else
  `/tmp/waldo-<uid>`. It must be mode 0700, owned by the caller, and not a
  symlink, or waldo refuses to use it. Agent sandboxes and the Nix build
  sandbox often have no `XDG_RUNTIME_DIR`, so the fallback is a first-class
  path, not an afterthought.
- `<id>` is `--name` if given, else six lowercase base32 characters. Short ids
  keep token cost down and keep the socket path under the 108-byte `sun_path`
  limit. If the path would still be too long, waldo binds through a directory
  fd.
- **Discovery**: `waldo-term list` reads the base directory.
- **Liveness**: a session is alive if its `lock` is held. The kernel releases
  the lock however the host dies, including SIGKILL. This distinguishes
  "host is busy" from "host is gone", which a connect attempt cannot. The
  host also releases the lock itself, after the will is out and before it
  answers anyone waiting for `exit`, so a client that is told `exit` can
  remove the session or reuse its name at once. With `--linger` the lock is
  held until the lingering is over.
- **Ended sessions**: a directory whose lock is free belongs to a host that
  has gone. `list` shows it as `exited` if the will is readable in the
  directory, which is the normal end with the default destination; as
  `undelivered` if the will is in `will.json` because it could not be
  delivered; and as `dead` if there is no will at all, because the host
  died without cleaning up. `waldo-term gc` removes all three once they are
  older than a threshold. A live session is `live`.

### 1.5 Shutdown

When the child exits (pidfd readable), the host:

1. Collects the status with `waitid`, which also gives the core-dump flag.
2. Drains the pty: reads until EIO, which the kernel delivers after buffered
   output once every slave fd is closed. If a surviving grandchild still holds
   the slave, EIO never comes, so the drain also ends after 100 ms without
   data.
3. Runs the match triggers one last time and sends the end of the stream
   (sections 3.2 and 3.4).
4. Composes the exit message and delivers it to the destination as the last
   message of the session (section 4).
5. Answers every pending request with reason `exit`. The will goes first so
   that a client that sees `exit` can read it at once.
6. With `--linger SECONDS`, keeps the socket open for that long and answers
   requests for the status, screen, output and triggers, and waits, each
   with reason `exit`. Anything else gets 409 `exited`. A stop request or a signal
   to the host ends the linger early, which is what `waldo-term kill` does.
7. Removes the socket, and with an explicit `--dest` the session directory,
   and exits.

When the host is told to stop (SIGTERM, SIGINT, or `POST /v1/signal` with
`"stop": true`), it sends SIGHUP, then SIGTERM after 2 s, then SIGKILL after
2 more, and reports `killed_by_waldo: true`. Signals go to the child's
process group and to the terminal's foreground process group, which is what
Ctrl-C at a keyboard reaches: with a shell as the child, the running command
is in a group of its own. `POST /v1/signal` with a signal name does the same
with that one signal.

If the host itself dies, the pty master closes and the kernel sends SIGHUP to
the child's foreground process group. Most programs exit on that. Waldo does
not try to guarantee it.

## 2. Control API

HTTP/1.1 over the unix socket. `waldo-term` is a thin client for it, and
`curl --unix-socket` works without it. `PROTOCOL.md` is the reference: the
endpoints, every request and response, and the errors. This section has the
reasons.

The API is the product and `waldo-term` is a convenience. The expected
long-term client is a harness that talks to the socket directly, so every
capability lives in the host behind an API call. `waldo-term` adds only
argument parsing, text formatting, and help. The one thing a client does
outside the API is read the will from the destination after a session has
ended, since by then there is no host to ask.

It is HTTP because every language has a client for it and curl is enough to
try it. Only a small subset is served: `Content-Length` bodies up to 1 MiB,
keep-alive, and `Expect: 100-continue`. Peers are checked with SO_PEERCRED
and must have the host's uid.

Every request that reads the screen can wait first, and every request that
types returns the screen. One call is a whole step: type, wait for the right
moment, look.

### 2.1 Input

Input is a list of items of three kinds, and not one string in one
notation, because agents type a lot of `<` (C includes, HTML, shell
redirects). `text` is never parsed, and `keys` is strict: an unrecognised
`<name>` is an error, not literal text. Nothing is sent if any item of a
request is bad.

- Keys are encoded when they are typed, for the terminal's modes at that
  moment (application cursor keys, keypad mode, modifyOtherKeys, kitty
  keyboard flags), which is why the encoder lives with the emulator.
- A paste is asked for explicitly, and is bracketed only when the program
  has turned that on: bracket markers sent to a program that has not
  enabled them corrupt its input. Vim started with `-u NONE` turns the mode
  on but does not recognise the markers, so a paste into it is mangled;
  `vim --clean` is fine.
- Each kind can take its content from a file. That is the intended route
  for anything long, such as a source file to paste into an editor. The
  host reads the file, which gives the caller read access to whatever the
  host can read. That is no more than a caller with the host's uid already
  has; see section 2.7 for what it means behind a bridge.

**One queue.** Items are written to the pty in order and contiguously: input
from other clients is never interleaved inside one request. Signals and
resizes go through the same queue, so they do not land inside a request
either:

- A signal or resize takes effect after the requests queued before it, and
  after the program has taken in their last input (the screen has reacted
  and gone still, or 300 ms have passed). A signal travels faster than
  input does, so without that wait it would overtake the keys just typed.
- Its response comes when it has happened, not when it was asked for.
- The exception: if the queue is stuck because the program is not reading
  its input and the terminal's buffer is full, signals and resizes are
  carried out at once. An interrupt is most needed exactly then.
- Ending the session never waits.
- A request is atomic in order, not all-or-nothing: if the program exits
  partway through, the rest is dropped. A request that times out is not
  cancelled; what it still has queued is typed afterwards, in one piece.
- The terminal's own answers to the program's queries (a cursor position
  report, for example) are written ahead of queued input and can fall
  between two items of a request.

**Timing.** The host does not write everything at once, because programs
read meaning into timing:

- A lone `<Esc>` is followed by a 30 ms pause before the next byte, unless
  the program has enabled the kitty keyboard protocol, so the program does
  not read `<Esc>` plus the next character as an Alt chord.
- Within one request, a key that follows text is held until the program has
  reacted to the text: the screen has changed since the text was written
  and has been still for 30 ms. The wait is at least 40 ms, and if the
  screen never reacts, as with a password prompt that echoes nothing, the
  key is sent after 300 ms. A person never types text and a key in the same
  instant, and programs that detect a paste by timing take an Enter that
  arrives with the text for a line break inside it. This was found by
  running Claude Code inside waldo: text and an Enter sent together left the
  prompt in its input box with a blank line under it. A fixed 40 ms pause
  was not enough on a freshly started program; waiting for the reaction
  was. The cause was checked only in part against that program (version
  2.1.322): it treats one input event of more than 800 characters as a
  paste and holds a Return that arrives during one, but where a short text
  and an Enter in one burst become a line break was not found.
- The first input of a session is held until the screen has settled for the
  first time, or until 2 s after the session started, whichever comes
  first. A shell echoes what arrives before its first prompt a second time:
  once by the terminal, and once by the shell when it draws its prompt.
- Beyond those, the caller decides. A `pause` item puts a delay at one
  point, and `key_delay_ms` puts one between all the keys of a request.
  Programs differ in how fast they take keys, and the host cannot know: in
  the tests against tmux, htop missed a `q` sent right after an Escape, and
  NetHack dropped keys sent in the same call as the one that opened its
  travel prompt, under both tools. The host does not guess; it gives the
  caller the control.

### 2.2 Waiting

The wait conditions other than `settle` exist because settle is a guess
(section 3.1): `match` for text that says the thing is done, `change` for
"anything at all", `exit` for a program that runs to completion.

A request may name several conditions and returns on whichever is met
first. This came from a person and an agent sharing a session: the agent
waited for a line on the screen and so did not hear the person, because a
broadcast is delivered when a request returns. The fix considered first
was for a broadcast to end any wait of a client that had named itself. It
was rejected: in a session of three, an agent waiting on a long build would
be woken by the other two talking. Naming the conditions leaves the choice
with the caller, and it does more: a marker or the program's exit, a prompt
or an error. `after` is a length of time as a condition, so that "this
text or five seconds" has no failure in it; the deadline remains the thing
that means something went wrong.

`exit` means the program being typed into: whatever had the terminal's
foreground when the request began. For a session that runs one program that
is the session's end. For vim started from a shell it is vim. It used to
mean the session only, and in the tests against tmux every agent, in every
round, waited on it after `:wq` or `q` and sat through a timeout; a written
warning did not stop that. The tool now means what its users took it to
mean. It is decided from the foreground process group, a fact the kernel
reports, not from a guess about the program.

- With input, `match` ignores text that was already on screen when the
  request began, so a wait for the prompt after a command is not satisfied
  by the prompt the command was typed at. Without input, text already on
  screen counts.
- Every request has a deadline. Reaching it is not an error: the response
  is still 200 with the current screen and reason `timeout`, because the
  screen is what the caller needs in order to decide what to do next.
- A child exit ends any wait early, with reason `exit`.

### 2.3 Responses

- `gen` counts changes to a hash of the visible cells, cursor, title and
  size, recomputed after each batch of output. Clients use it with `until:
  change` and to notice that nothing happened.
- Screen text has trailing whitespace trimmed from each row and trailing
  blank rows dropped. Width then costs almost nothing in tokens.
- `changed` says whether anything on screen is different from when the
  request's first input was written. Programs swallow keys without a sign:
  a prompt is pending, a move ran into a wall. The host cannot know why,
  so it reports the fact and does not guess a cause. It is a field and a
  word in the status line, not a hint, because input that changes nothing
  is sometimes right, as with a password that is not echoed.
- `interleaved` says another client's input reached the pty during the
  request. Each request's input is typed whole, so it never means mixed-up
  lines; it means the screen may hold more than the request caused, such as
  the echo of what the other client typed.
- Screens are plain by default, because styled ones cost more tokens.
  `style=ansi` keeps the colour codes: installers and dialogs show which
  button or menu entry is selected only with colour, and an agent can read
  the codes (section 6.2).
- The text format (one status line, then the raw screen) is cheaper for an
  agent to read than JSON, so `waldo-term` prints it by default and the
  host offers it too.
- An error is a stable snake_case code and a sentence. The sentence is for
  whoever reads it, and says what to do next.

### 2.4 waldo-term

`waldo-term help commands` lists the commands, and `PROTOCOL.md` section 10
says what the client does beyond sending a request and printing the answer.
The choices worth recording:

- `-s` may be omitted when exactly one session is live, which is the common
  case for an agent.
- `--text -`, `--keys -` and `--paste -` read from stdin, which keeps
  secrets out of `argv`.
- The exit status tells the caller's shell what happened without parsing:
  0 if the wait condition was met, 124 on the deadline, 3 if the program
  exited first, 4 if there is no such session.
- A run-to-completion program may finish before the agent's next call. If
  the session used the default destination, `waldo-term` finds the will
  there, prints the exit details and exits 3; `output` prints the will's
  output and exits 0. With an explicit `--dest` the session directory is
  gone, the agent gets status 4, and the will is wherever `--dest` pointed.
- `kill` sends the stop request, waits until the host has gone, and prints
  one line saying how the program ended. The will is still written, so
  `output` works afterwards.

### 2.5 Shell commands

Settle cannot tell a finished command from a quiet one, and a screen does not
carry an exit status. Waldo has no special command for this. The caller puts
a marker after the command and waits for the marker:

```
waldo-term send -s sh --text 'make test; echo "rc=$?"' --keys '<Enter>' \
    --until match --pattern '^rc=[0-9]+$' --timeout 120000
```

- The screen in the response shows the command's output and then the marker
  with its exit status. Output longer than the screen is in `GET /v1/output`.
- The marker should be something the output will not contain, or the wait
  returns early.
- `foreground` in every response (`fg=` in the text format) names the
  program that owns the terminal: the shell at its prompt, the command while
  one is running. A shell builtin, or a shell reached through ssh, does not
  show there; the marker does.
- `waldo-term help shell` teaches this.

There was a `run` command that did this itself. It typed the command inside
a wrapper that printed an invisible mark, or used the shell's own OSC 133
marks when it had seen them, and returned the output and the exit status. It
was removed on 2026-11-21:

- Agents given the same nine shell tasks without it got them all right, in
  as many calls and tokens as agents that had it, and two of them worked out
  the marker by themselves.
- It was the only command that typed something the caller did not write.
  Most of the open problems, and the two worst bugs found by the soak runs,
  came from that: a wrapper typed into a program that was not a shell, text
  left on the input line, an interrupt that lost the mark, and one run at a
  time per session.

### 2.6 Help and guidance

An agent arrives knowing nothing about waldo, so the tool has to teach itself.
These are requirements, not polish.

- `waldo-term help` prints a guide of about forty lines: start a session, run
  a command, send keys, wait for a pattern, read the will. Every example is a
  complete command that works as written.
- `waldo-term help TOPIC` covers `keys` (the notation and every key name),
  `wait`, `shell`, `messages` and `exit-status`. `waldo-wrap --help` and each
  subcommand's `--help` end with two or three examples.
- Every error's `text` says what to do next, as the `unknown_key` message
  does.
- Responses carry a `hint` when the host can see a likely mistake. The text
  format prints it as a last line starting with `hint:`. The hints:
  - A settle that returns while the shell has a foreground job that this
    request's input started (the terminal's foreground process group is
    not the child's own, and not what it was before the input was sent):
    `hint: 'make' is still running; settle only means the screen went quiet. To wait for it to end, see 'waldo-term help shell'.`
    Later requests to the same program carry no hint: by then it is the
    thing being driven.
  - The mirror case: the program the input was sent to has left the
    foreground by the time the screen settles, and the session's own
    program has it again:
    `hint: 'vim' is no longer in the foreground; input now goes to 'bash'.`
  - A match wait with input that times out while the pattern matches text
    that was already there: the hint says that only a new match counts for
    a request that sends input, and to use `wait --until match` instead.
  - A wait for `exit` that times out: the hint names the program that was
    being waited for.
  - A timeout while the screen was still changing:
    `hint: the screen never went quiet; wait for specific text with --until match.`
  - Unparsed text that looks like key notation sent as `text`:
    `hint: '<Esc>' was sent as literal text; use --keys for key names.`
- `child` in every response includes `foreground`, the name of the process
  that currently owns the terminal, so the agent can see whether it is talking
  to the shell or to something the shell started.

### 2.7 TCP

`waldo-wrap` has no TCP listener. The unix socket is the only control channel.
The clients we expect are ones we write ourselves, which can open a unix
socket, and anything else can be bridged from outside:

```
socat TCP-LISTEN:7333,bind=127.0.0.1,reuseaddr,fork UNIX-CONNECT:"$SOCK"
ssh -L 7333:/run/user/1000/waldo/k3f9q2/sock host     # from another machine
```

A bridge removes the socket's protection. The file mode and the peer-uid check
only see the bridge process, so whoever can reach the port can type into the
program, and file items let them read any file the host can. That is the
operator's decision to make when they start the bridge.

If bridged use becomes common, the fix is a `--token-file` option that makes
the host require an `Authorization` header on every request, including over
the unix socket. It is not planned until there is a need.

## 3. Trigger semantics

### 3.1 Settle

The screen is settled when all of these hold:

1. `gen` has not changed for the quiet window (`--quiet-ms`, default 200).
2. All queued input has been written to the pty.
3. No synchronized-output hold (mode 2026) is open.
4. The screen has shown text, or input has been sent. A blank screen that
   stays blank is a program that has not drawn yet, not a settled one.
   Output alone does not count, because programs such as `top` set the
   terminal up well before their first frame. A program that never draws
   runs the wait to its deadline, with a hint.

The window is measured from the later of the last screen change and the last
input write. It shrinks to 40 ms when the shell has emitted an OSC 133
prompt-start mark since the last input, which is a positive signal that the
shell is waiting.

Settle means "nothing is being drawn right now". It does not mean the command
finished. Two known ways it is wrong, both stated in the response so the agent
can react:

- **Early**: a command that is silent for longer than the window (a compiler
  between steps) settles while still running. Use `until: match` on the
  expected prompt or output, or `until: exit` for run-to-completion programs.
- **Never**: a clock, spinner or progress animation keeps the screen changing,
  so the request runs to its deadline and returns `timeout`. The screen is
  still returned.

As an asynchronous trigger, `settle` emits one message each time the screen
settles after having changed. It is off unless the session was started with
`waldo-wrap --on settle`.

### 3.2 Match

A match trigger is a regex with an id, added with `POST /v1/triggers` or
`waldo-term match add`. Its matches go to the destination as `match`
messages. Ids are 1 to 32 characters from letters, digits, `_` and `-`; one
is made up (`t1`, `t2`, ...) if none is given. Rules:

- The subject is one logical line: physical rows joined where the terminal
  soft-wrapped them. Patterns never span logical lines. Lines longer than 4096
  cells are truncated for matching.
- The regex engine is linear-time (no backtracking), so a pattern cannot stall
  the event loop. At most 32 triggers per session.
- Lines are examined in two places: the visible screen, at each evaluation; and
  each line once as it scrolls off the primary screen, so fast output that
  never rests on screen is still seen. The cursor's own line is included, which
  is what makes prompts without a trailing newline (`Password: `) matchable.
- The visible screen is looked at after output, at most every 25 ms, never
  inside a synchronized-output hold, and always at a settle point. Lines that
  scroll off are looked at as they go, without that limit.
- **Edge triggering**: a match is identified by its line's absolute row number
  (rows scrolled off so far plus the screen row) and its matched text. The
  trigger fires when a match appears that is not already known. A known match
  is forgotten only when it is absent at a settle point. A program that clears
  and repaints the same content between settles therefore does not re-fire;
  the same text appearing again after the screen has settled without it does.
- Text that is already on screen when a trigger is added is a match, and
  fires.
- A resize reflows lines and changes row numbers. Matches present immediately
  after a resize are recorded without firing.
- `mode` is `edge` (the default) or `once` (fires one time, then removes
  itself).

A `match` wait (section 2.2) uses the same line and pattern rules, with one
difference: it forgets a known match as soon as it is gone, not at a settle
point. A wait is answered once, so a repaint cannot make it fire twice.

### 3.3 Exit

Fires exactly once, after the pty is drained. See section 4.

### 3.4 Stream

Opt-in, with `waldo-wrap --on stream`. Emits rendered output lines as they
are committed (scroll off the primary screen), batched, at most one message
per 250 ms. It is a `tail -f` of the output. It carries text, not escape
sequences, and emits nothing while a full-screen program is on the alternate
screen; `settle` covers that case. When the program exits, the lines still on
screen are sent too, so the stream as a whole is the will's `output` before
any capping. If lines arrive faster than they are sent, at most 512 KiB wait;
the oldest beyond that are skipped and counted in `skipped_lines`.

Raw byte recording is separate: `waldo-wrap --record FILE` writes the
program's output as it arrived to an asciicast v2 file, with a resize event
for each resize, which `asciinema play` replays. If the file cannot be
created the session does not start; if writing fails later the recording
stops and the session carries on.

## 4. Messages

Asynchronous messages go to the destination as one JSON object each.
`PROTOCOL.md` section 8 has the envelope and every field. The choices:

- **`seq` and `dropped`.** A gap in `seq` means messages were lost, and the
  next message that arrives says how many. A receiver never has to guess.
- **`hello`** is sent before the child is spawned, because it is also the
  destination probe.
- **`match` carries the screen**, so a receiver can act on it without
  calling back.
- **`stream` carries rendered lines, not bytes.** It is a `tail -f` of the
  output. Raw bytes are what `--record` is for.
- **The will's `output` is rendered, not raw:** lines that scrolled off plus
  the final primary screen, with wrapped lines joined, so a carriage-return
  progress bar collapses to its final state. It is capped by keeping the
  first quarter and the last three quarters with a marker line between, so a
  will shows both the command and its last errors and is never megabytes on
  one line. There is no other copy: what the cap cuts is gone. Output drawn
  on the alternate screen is not part of it; the final `screen` shows what
  was last visible.
- The will is the last message the session sends.

## 5. Destinations

Every session has a destination. If `--dest URI` is not given, the host uses
`file:<session dir>/events.ndjson` and reports it at startup (section 1.1). An
agent with only a Bash tool can ignore it until the end; the will is its last
line. By default only `hello` and `exit` are sent, so the file stays small.

By default the host probes the destination before spawning the child and
refuses to start if `hello` can't be delivered, so a session never runs with
nowhere to report. `--no-probe` skips that, for a receiver that comes up
after the session does.

The three schemes (`file:`, `unix:`, `http:`) and their delivery rules are
in `PROTOCOL.md` section 8.7. The choices behind them:

- `https:` is out of scope; put a local proxy in front if needed.
- `http:` opens a connection for each message. That is slower than keeping
  one open, and it cannot go stale.
- Delivery has its own thread and a bounded queue, so a slow or blocked
  receiver can never stall the pty. When the queue is full, what is dropped
  is what matters least: a waiting `settle` gives way to a newer one,
  `stream` messages merge, then the oldest `match` or `stream` goes. `hello`
  and `exit` are never dropped.
- `file:` and `unix:` deliver at most once. `http:` retries, so it delivers
  at least once, and receivers de-duplicate on `(session, seq)`.
- The log says when delivery starts failing and when it works again, not
  once per message.
- If the will cannot be delivered, it is written to `will.json` in the
  session directory, and the directory is left in place.

**Detecting a dead host.** The host cannot announce its own SIGKILL or OOM
kill. Receivers can tell by:

- `unix:`: end of stream with no `exit` message.
- `file:`, `http:`, or anyone else: the session directory still exists, its
  `lock` is free, and it holds no `will.json`. `waldo-term list` reports
  this state as `dead`.
- `http:` receivers with no filesystem access can ask for `--heartbeat
  SECONDS` and treat silence as death.

## 6. Screen and output model

### 6.1 Screen text

Produced by the emulator's plain-text formatter over the active screen,
trailing whitespace trimmed. Soft-wrapped rows stay as separate rows in
`screen.text`, because the screen is a picture of what a person would see;
they are joined in `output` and for matching, because those are about content.

### 6.2 Styled screen text

`?style=ansi`, `"style": "ansi"` in a request body, or `waldo-term ...
--style ansi` returns `screen.text` with SGR sequences. The host builds each
row from cell styles itself.

- Each run of cells with the same style starts with one complete sequence
  that begins with a reset, such as `ESC[0;1;31m`, so a run can be read
  without knowing what came before it. A row that set any style ends with
  `ESC[0m`. Nothing else is emitted: no cursor movement, no other escapes.
- Colours are written as the program chose them: the 16 named colours as
  30 to 37, 40 to 47, 90 to 97 and 100 to 107; other palette entries as
  `38;5;N`; true colour as `38;2;R;G;B`.
- Attributes: bold, faint, italic, underline, blink, inverse, invisible,
  strikethrough. Underline styles and underline colour are not distinguished.
- Trailing cells are dropped only if they are blank and unstyled, so a
  highlighted bar that runs to the edge of the screen keeps its width.
- Plain remains the default. Messages to the destination are always plain.

In `htop`'s sort menu the selected entry reads
`ESC[0;30;46mPERCENT_CPU   ESC[0m`, and moves when an arrow key is sent.

### 6.3 Screen capture to PNG

Not built. The font to vendor is not chosen.

`GET /v1/screen?format=png` returns a picture of the current screen as
`image/png`. It takes the same wait parameters as any screen request, so
`until=settle` captures a finished frame. The status line that the text format
prints is sent in a `Waldo-Status` response header.

- The host draws. Any client gets a picture with one request, whether that is
  `waldo-term`, curl, or a harness. `waldo-term screencap -o FILE.png` only
  saves the bytes.
- Parameters: `scale` (default 1) and `font`, an absolute path to an OpenType
  or TrueType font that the host reads, as it does for file items. A monospace
  font is vendored and compiled into `waldo-wrap` as the default. Characters
  the font lacks are drawn as a box.
- Rendering uses a font-parsing and rasterising crate and a PNG crate
  (`ab_glyph` and `png` are the candidates; neither has been evaluated),
  with no dependency on a display, fontconfig or system font paths, none of
  which can be assumed on NixOS or in a sandbox.
- The image is `cols` by `rows` cells at the font's cell size times `scale`.
  It shows text, foreground and background colours, bold, italic, underline,
  reverse video, wide characters, the cursor, and images the program has
  drawn (section 6.4).
- Drawing must not disturb the session. The event loop copies the cells and
  hands them to a short-lived thread that rasterises and encodes, so the pty
  keeps being served meanwhile. A panic in the rendering code is caught there
  and becomes a 500 `render_failed`; it does not take the host down.
- `GET /v1/screen?format=cells` returns what the renderer starts from: each
  row as runs of `{"text", "fg", "bg", "attrs"}` with colours resolved to RGB,
  plus the cursor. It is there for a client that wants to draw its own.

### 6.4 Images

Not built, and nothing here has been tried: no image has been sent through
the emulator. The Sixel translation in particular is an idea to spike first.

Programs draw pictures in a terminal with one of three protocols. Waldo is a
terminal, so it should take them, and with PNG capture a client with vision
can then see a plot or a preview that a program drew.

| Protocol | In the emulator core | Plan |
|---|---|---|
| Kitty graphics | Implemented: images are stored, pixels and placements can be read back | Supported |
| Sixel | Not implemented. The core only knows the name of the capability | Supported by translation, later |
| iTerm2 inline images (OSC 1337) | Sequence is parsed, images are not implemented | Not planned |

**Kitty graphics.** The host turns on the emulator's image storage with a
size limit (default 64 MiB per session) and lets the emulator answer the
protocol's support query, so programs detect it the normal way.

- PNG capture composites each placed image over the cells it covers.
- Text and JSON screens cannot show a picture. They list what is there
  instead: `"images": [{"id": 3, "row": 2, "col": 1, "cols": 40, "rows": 12}]`,
  and the text format adds a line such as
  `images: #3 at 2,1 40x12 cells`. That tells an agent to ask for a PNG.
- Only image data sent inline is accepted. The protocol also lets a program
  name a file or shared-memory object for the terminal to read; that stays
  off, and programs fall back to inline data.
- Animations are stored but a capture shows the current frame only.

**Sixel.** The core does not decode it, and tracking where an image sits as
text scrolls around it is the emulator's job, not something to duplicate
beside it. So the host translates: a filter in front of the emulator spots a
Sixel sequence in the pty output, decodes it to pixels, and feeds the
emulator the same picture as a Kitty graphics command. Placement, scrolling
and capture then work as for any other image. Until that exists, the terminal
does not claim Sixel support in its device attributes, and well-behaved
programs do not send it.

**Attach.** A human watching through `waldo-term attach` sees the text. Images
are shown as an outlined box with the image id. Re-sending the picture to the
human's own terminal is possible if that terminal supports the Kitty
protocol, and is left for later.

### 6.5 Output buffer

The emulator has no callback for lines leaving the screen, so the host keeps a
tracked reference (a handle the emulator moves as the grid scrolls, reflows or
prunes) at the first row not yet collected. After each chunk of output, rows
between that reference and the top of the active screen have become history;
they are joined on their wrap flags, appended to the output buffer, checked
against match triggers, and the reference moves forward.

The output buffer lives in memory and is bounded by `--output-cap`: a fixed
head that fills once, and a ring holding the most recent lines. It feeds the
will's `output`, `GET /v1/output` and the `stream` trigger.

If output arrives faster than history can hold between two chunks, the
reference reports that its row was discarded. The host then records a
`[... output lost ...]` marker and restarts from the oldest surviving row.
With 4 KiB chunks and 4 MiB of scrollback this should not happen in practice.

## 7. Attach, resize, and shared input

### 7.1 Rendering

`waldo-term attach` re-renders from screen state. It does not pass the child's
bytes through.

Raw passthrough fails in three ways: the human's terminal would answer the
child's queries a second time and those answers would be injected as input;
the human's terminal is rarely the session's size; and a client that attaches
late needs the current state, not the byte history.

So the host sends frames: on attach every row of the screen as styled text
built from emulator state, then the rows that changed, at most one frame
every 16 ms and none while the program has a synchronized-output hold open.
`waldo-term` puts its own terminal in raw mode on the alternate screen and
draws frames inside a viewport with a one-line status bar. It does not link
the emulator; only the host does.

The attach stream is an HTTP upgrade on the control socket, carrying one
JSON object per line in both directions, so it can be read with a few lines
of any language (`PROTOCOL.md` section 7).

When the session ends, the viewer leaves its screen, prints the will, and
exits 3, as any other command does.

### 7.2 Size

The session owns its size. Attaching never resizes it. The host crops, since
it knows cell widths: the viewer says how big it is, in the request's query
and then in `viewport` messages, and receives rows already cut to fit. A
smaller viewer sees the part that holds the cursor, and its status bar says
`(showing 40x9)`. A larger one sees the session in the top left corner with
the rest blank; there is no border.
`attach --resize` opts in to adopting the viewer's size and restores the old
size when that viewer detaches. `POST /v1/resize` sets it explicitly. A resize
sends SIGWINCH through the pty, reflows the primary screen, counts as a screen
change, and restarts any settle window. The response waits for the screen to
settle, so it shows what the program redrew, but for at most 1 s: after that
it answers with reason `none` and the screen as it is. A `wait` object in
the request replaces that, and `{"until": "none"}` answers at once.

### 7.3 Human and agent input

- Attach has two modes, viewing and interacting, and starts in viewing
  unless `--drive` is given. It keeps one key for itself, Ctrl-], which
  switches between them. The rule for every other key is that whoever has
  the keyboard gets it. While interacting that is the program, Ctrl-C
  included. While viewing it is the viewer, so Ctrl-C leaves, as it would
  leave any program, and `s` says something to the other clients. Leaving
  ends only the viewer; the session carries on.
- There used to be three reserved keys, Ctrl-] to detach, Ctrl-\ to switch
  typing on and off and Ctrl-_ to speak, and the first two could not be
  typed into the program at all. A key only has to be reserved while the
  program has the keyboard, and one is enough for that. "Detach" was
  tmux's word; what it means is that the viewer quits.
- The viewer's keystrokes are sent as its terminal encoded them. So that they
  are encoded the way the program expects, the viewer's terminal is put in
  the same cursor-key, keypad and bracketed-paste modes as the session. The
  kitty keyboard protocol and modifyOtherKeys are not mirrored, so a program
  that relies on them gets the traditional encoding from a viewer.
- All input goes through one queue. Each API request is atomic in it; a human's
  keystrokes are queued between requests, never inside one. A viewer's
  `--resize`, and the size coming back when it leaves, are queued too.
- There is no lock. An agent's request still works while a human is driving,
  and its response carries `interleaved: true` if anyone else's input landed
  during it, so the agent knows the screen may not be the result of its keys
  alone.
- The status bar shows when an agent request is in flight.

Not built: mouse input from the viewer, scrolling back through history while
attached, and images, which are shown as nothing.

### 7.5 Broadcasts

Several clients can already use one session, and the host already keeps
their input whole and tells each what the others did. What they could not
do was say anything to each other except through the program. A broadcast
is that: a line from one client to the rest, which the program never sees.
The reference is PROTOCOL.md section 4.10.

- **Why it belongs here.** A person can tell an agent "wrong branch"
  while it works. An agent can ask a person to type a password and wait to
  be told it is done, so the secret never enters the agent's context.
  Agents can hand a shell to each other. A harness could do all of this,
  but the session is already the shared place, and this needs no harness.
- **Callers name themselves.** The host cannot tell clients apart: each
  request is a new connection from the same user. So a client that wants
  broadcasts says who it is, and is given each one once. A client that
  gives no name gets none, and nothing about its responses changes.
- **Delivery rides on responses.** An agent has no open connection to push
  to; it has its next request. What others said arrives with whatever it
  asked for, so an agent is steered without polling. `until: broadcast` is
  for the agent that should stop and wait to be spoken to.
- **A new name is given what is kept.** Someone who joins late sees the
  recent conversation. The cost is that the first request under a new name
  can carry up to 32 lines.
- **Where it is printed.** Above the status line. The screen is the
  program's to fill, and so is everything under it that looks like the
  screen; the lines before the status line are only ever waldo's.
- **It is a relay and no more.** One line, no addressing, no threads, no
  history beyond the last 32, nothing across sessions. The destination
  gets every broadcast, so a record exists for whoever wants one.
- **A person attached reads and writes them in a panel** over the bottom
  of the screen, opened with `s`. It began as one line in the status bar
  that showed a broadcast for twelve seconds. The first person to use it
  asked for two messages to be repeated: a message that goes away is not
  much of a message. So the newest unread one now stays in the bar until
  the panel is opened, the panel keeps the conversation, and a viewer who
  arrives late is sent what the host has kept. It stops there: no
  scrolling past what the host keeps, no timestamps, no private messages.
  The screen behind the panel is not dimmed; rows arrive already styled,
  and a heading line marks the panel well enough.
- **They reach a viewer on an ordinary frame**, as one more field, so a
  viewer that does not know about them draws the frame as before. A person
  who is only watching can still speak: saying is not typing.
- **On by default, with a switch to turn it off.** Hearing is already a
  choice each client makes by naming itself, and saying needs no access
  that typing does not. The use that matters most is unplanned: an agent
  meets a password prompt and asks for help, which only works if nobody
  had to foresee it. But whoever starts a session may need to promise that
  nothing reaches its agent except the program, so `waldo-wrap
  --no-broadcast` removes the channel, and `status` says so.
- **How a broadcast arrived is recorded** (`via`): over an attach stream,
  or as a request. The host cannot know who is a person, but it does know
  that, and a person at a terminal uses the first.
- **Names are not checked.** Every client is the same user and can already
  type into the program; a false name gains nothing that typing does not.

## 8. Failure modes

| Situation | Behaviour |
|---|---|
| Program not found or exec fails | `waldo-wrap` exits non-zero with the errno text. No socket path is printed. If exec fails after the probe, the destination receives an `exit` with `spawn_error` |
| Destination unreachable at start | Exit non-zero before spawning the child. With `--no-probe`, start anyway and queue |
| Destination fails later | Queue fills, messages coalesce or drop with `dropped` counts; the pty is unaffected |
| Destination unreachable at exit | Retry 5 s, then write `will.json` in the session directory and keep the directory |
| Agent calls `waldo-term` after the session ended | Default destination: the will is read from it, status 3. Explicit `--dest`: status 4, and the will is at that destination |
| A request arrives during `--linger` | Status, screen, wait, output and triggers are answered with reason `exit`; anything else gets 409 `exited` |
| The `--record` file cannot be written | At startup: the session does not start. Later: the recording stops, the session continues, the log says so |
| Child exits with output unread | Pty drained before the will is composed (section 1.5) |
| Grandchild keeps the pty open after the child exits | Drain ends after 100 ms of silence; the will is sent; the grandchild is left alone and gets SIGHUP when the host exits |
| Child exits during a request | Request returns `reason: exit` with the exit object |
| Request reaches its deadline | 200 with `reason: timeout` and the current screen |
| Screen never settles | Same as a deadline. Use `match` |
| Client disconnects mid-request | Request is cancelled. Input already queued is still sent |
| Unknown key name | 400 `unknown_key`. Nothing is sent, including earlier items of the same request |
| Host killed (SIGKILL, OOM) | No will. `lock` is released; `unix:` receivers see end of stream; the child gets SIGHUP. `list` shows `dead` |
| Host receives SIGTERM | Kills the child (HUP, TERM, KILL), sends a will with `killed_by_waldo: true` |
| Emulator library aborts on a bug | Same as host killed. Mitigated by building the library with safety checks on, so a bug is an abort and not memory corruption |
| Output faster than it can be collected | Marker line in the output; the session continues |
| Stale session directory | Detected by free `lock`; removed by `gc` |
| Session path too long for `sun_path` | Bind through a directory fd |
| Caller's sandbox kills the whole cgroup or pid namespace when the Bash call ends | `--detach` cannot survive this. Documented limitation; `systemd-run --user --scope` is the workaround where available |
| Second `waldo-wrap --name X` while X is alive | Refused: the lock is held |

Security notes:

- Access control is the socket's mode, its 0700 parent, and the SO_PEERCRED uid
  check. Anyone who can reach the socket can type into the program.
- Screen text is untrusted input to whatever reads it. A wrapped program can
  print text that looks like instructions to an agent. Waldo cannot prevent
  that; consumers should treat screens as data.
- Key payloads are not logged.
- The host follows symlinks. Against the callers it admits that changes
  nothing: a caller with the host's uid can name any file the host can read.
  Where it matters is a predictable name in a directory other users can
  write to. The session base directory is checked for that (section 1.4),
  and everything waldo creates itself lives inside it. A `--dest file:` or
  `unix:` path, or a `--record` file, is opened where the operator points
  it, so keep those out of shared directories such as `/tmp`: another user
  could plant a symlink under the name and have the host write through it.

## 9. Language and emulator core

### 9.1 The choice

**Rust, with libghostty-vt 1.0.x as the emulator, through its C ABI.**

- libghostty-vt reached 1.0.0 on 2026-11-06 and 1.0.1 on 2026-11-11. The C ABI
  is declared stable for 1.x with semver, a versioned soname and an ABI check
  in upstream CI. It is a complete emulator, not just parsers.
- It is the only candidate that has everything this design uses in one place:
  query replies through a write-back callback, soft-wrap flags, a plain-text
  formatter with unwrap and trim, mode 2026 state, OSC 133 prompt marks, a key
  encoder that follows the terminal's modes including the kitty protocol,
  bracketed-paste encoding with sanitising, reflow on resize, tracked grid
  references, and a formatter that emits escape sequences for attach.
- The binding is a small hand-written FFI layer over the C header, for the
  roughly thirty functions used. The C ABI is the stable surface.
- The 1.0.1 release source is vendored and linked statically (section 9.2).
- Rust gives direct C calls, a fork that works before any thread exists, and
  control of the event loop and signal mask, with a linear-time regex engine
  available as a dependency-free crate. No async runtime: one epoll loop and
  one delivery thread.

### 9.2 Dependencies

The goal is few external dependencies, not none: a well-used crate is taken
where the alternative is writing and securing our own parser or rasteriser.

| Dependency | Kind | Used for |
|---|---|---|
| libghostty-vt 1.0.1 | Vendored source, built with Zig, linked statically | The emulator |
| Zig 0.17.x | Build tool, from nixpkgs | Building the library. Not present at run time |
| `libc` | Crate | Declarations for the Linux system calls |
| `regex-lite` | Crate | Linear-time regex for match triggers. No dependencies of its own, unlike `regex` |
| `serde`, `serde_json` | Crates | JSON for the API and messages |
| `httparse` | Crate | Parsing HTTP requests and responses |

At run time the binaries need only glibc and libgcc.

**Vendoring libghostty-vt.** The release tarball (4.5 MB, 26 MB unpacked) and
the tarball of its own five build dependencies are committed under `vendor/`
with their checksums, and the flake builds from those. The build then needs
no network, and survives upstream moving its hosting. The library is built
static and safety-checked (`ReleaseSafe`), so a bug in it stops the session
with a message instead of corrupting memory.

SIMD stays on. In 1.0.1 the no-SIMD library is built without libc and
exports stub `calloc` and `free` functions; linked statically into a program
that uses a shared glibc, the stubs replace the real ones and the program's
own allocations fail. The SIMD build links libc and has no stubs. Its C++
(simdutf and Highway) is compiled by Zig itself without the C++ standard
library, so it adds no build tool and no run-time library.

**Crates** are pinned by `Cargo.lock` and fetched by hash in the Nix build.
They are not committed to the repository.

**Why these and not larger ones:**

- `httparse` only parses; it does no I/O. The host keeps its own epoll loop
  and its own sockets. A full HTTP server crate would bring an async runtime
  or a thread pool, and both conflict with forking for `--detach` and with a
  single loop that owns the pty.

**What we still write ourselves:** the FFI declarations for the thirty or so
library functions used, by hand against the stable C header (no `bindgen`,
which would pull in clang); argument parsing; RFC 3339 timestamps; base32 ids.

No async runtime and no test framework beyond the standard one.

### 9.3 Risks of this choice

- The library is young: 1.0.0 is from 2026-11-06. Resize and reflow are
  where its bugs have been reported, including one after 1.0.1 where
  resizing with the viewport scrolled fails an internal integrity check.
  That one cannot happen here, since the host never scrolls the viewport,
  but resize is the area to test hardest.
- A Zig toolchain (0.17.x) is needed to build the library. It comes from
  nixpkgs and is the one build dependency we cannot vendor sensibly.
- The Ghostty project has said it will move off GitHub, with no destination
  given. Vendoring the source removes that as a build risk; it remains a
  question of where updates will come from.
- Keep the emulator behind one internal module (feed bytes, resize, screen
  text, cursor and modes, encode key, rows since watermark, repaint) so the
  core can be swapped without touching the rest.

### 9.4 Alternatives

| Option | For | Against |
|---|---|---|
| Rust + `alacritty_terminal` 0.26 | Pure cargo build, mature, answers queries, reflow, mode 2026, kitty flags | No OSC 133, no scroll-off hook, no tracked references, zero-width handling only (no grapheme clusters); key encoding and paste sanitising would be ours to write. The fallback if libghostty-vt proves too young |
| Zig + libghostty-vt Zig module | Same core, no FFI, how zmx does it | The Zig API is explicitly unstable, so we'd either chase it or call the C API from Zig anyway; no regex in the standard library; Zig 0.17's I/O interface is new |
| Rust + `wezterm-term` | Broadest features after libghostty | Not on crates.io (git dependency), last tagged release 2024, host must implement the 2026 hold |
| Rust + `avt` or `vt100` | Small, have scroll-off callbacks | Neither answers terminal queries, neither handles mode 2026 or the kitty protocol; `vt100` does not reflow. `ht` uses `avt` and inherits exactly these gaps |
| Go + `charmbracelet/x/vt` | Simple deployment | No wrap flag, no reflow, no 2026, no kitty keyboard |
| C + libvterm | Host-owned scrollback with a push callback | Standalone fork archived; neovim itself is moving to libghostty-vt |
| Node + `@xterm/headless` | Complete and battle-tested | Needs a Node runtime per session |

### 9.5 Prior art worth knowing

`coder/agent-tty`, `microsoft/tui-test`, `pilotty`, `termwright` and `ht` all
solve nearby problems. The closest is agent-tty (one detached host per session,
JSON over a unix socket, libghostty-vt for snapshots). What waldo adds over
them: a synchronous send-wait-return call inside the host, push delivery to a
destination, an exit message with rendered full output, and regex triggers on
rendered lines including scrolled-off ones. Their settle heuristics are all a
fixed quiet window (250 ms in tui-test) or a text wait; none uses mode 2026 as
a frame boundary. zmx and tmux both use a sentinel appended to the command to
detect completion; waldo leaves that to the caller (section 2.5).
