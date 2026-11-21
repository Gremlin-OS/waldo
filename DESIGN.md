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
bring about. Waiting for a program's first output, or for a command that has
not finished, sets none, so an idle host uses no CPU. All pty and
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

Each session has a directory:

```
$XDG_RUNTIME_DIR/waldo/<id>/
  sock         control socket, mode 0600
  lock         flock held by waldo-wrap for its whole life
  meta.json    id, name, argv, cwd, child pid, host pid, start time, size,
               destination
  log          host log (never contains key payloads)
  will.json    only if the will could not be delivered (section 5)
```

With the default destination the directory also holds `events.ndjson`, and it
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
   message of the session (section 4.5).
5. Answers every pending request with reason `exit`. The will goes first so
   that a client that sees `exit` can read it at once.
6. With `--linger SECONDS`, keeps the socket open for that long and answers
   `GET` requests for the status, screen, output and triggers, each with
   reason `exit`. Anything else gets 409 `exited`. A stop request or a signal
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
`curl --unix-socket` works without it.

The API is the product and `waldo-term` is a convenience. The expected
long-term client is a harness that talks to the socket directly, so every
capability lives in the host behind an API call. `waldo-term` adds only
argument parsing, text formatting, and help. The one thing a client does
outside the API is read the will from the destination after a session has
ended, since by then there is no host to ask.

Accepted subset: `Content-Length` bodies only (chunked requests get 411),
`Expect: 100-continue` is answered, bodies are capped at 1 MiB, one request per
connection is fine and keep-alive is supported. Peers are checked with
SO_PEERCRED and must have the host's uid.

Rows and columns are 1-based. Columns count terminal cells, not characters, so
a wide character occupies two.

| Method and path | Purpose |
|---|---|
| `GET /v1/status` | Session metadata, child state, current `gen` |
| `GET /v1/screen` | Current screen. Accepts the wait parameters below. `?format=png` and `?format=cells` are not built (section 6.3) |
| `POST /v1/input` | Send input, then wait, then return the screen |
| `POST /v1/wait` | Wait without sending input |
| `POST /v1/run` | Run a shell command and wait for it to finish (section 2.5) |
| `GET /v1/output` | Rendered output so far, from the capped buffer: `?tail=N` lines |
| `GET /v1/triggers` | List match triggers (section 3.2) |
| `POST /v1/triggers` | Add one: `{"pattern": RE, "id": NAME, "mode": "edge" or "once"}`; only `pattern` is required |
| `DELETE /v1/triggers/<id>` | Remove one |
| `POST /v1/resize` | `{"cols": 100, "rows": 30}` |
| `POST /v1/signal` | `{"signal": "SIGINT"}` to the child's process group, or `{"stop": true}` |
| `GET /v1/attach` | Upgrade to the attach stream (section 7) |

### 2.1 Input

```json
POST /v1/input
{
  "input": [
    {"keys": "<Esc>ggO"},
    {"text": "#include <stdio.h>"},
    {"keys": "<Esc>:w<Enter>"}
  ],
  "wait": {"until": "settle"},
  "timeout_ms": 10000
}
```

Items are written to the pty in order and contiguously: input from other
clients is never interleaved inside one request. Signals and resizes go
through the same queue, so they do not land inside a request either:

- A signal or resize takes effect after the requests queued before it, and
  after the program has taken in their last input (the screen has reacted
  and gone still, or 300 ms have passed). A signal travels faster than
  input does, so without that wait it would overtake the keys just typed.
- Its response comes when it has happened, not when it was asked for.
- The exception: if the queue is stuck because the program is not reading
  its input and the terminal's buffer is full, signals and resizes are
  carried out at once. An interrupt is most needed exactly then.
- Ending the session (`{"stop": true}`, `kill`) never waits.
- A request is atomic in order, not all-or-nothing: if the program exits
  partway through, the rest is dropped. A request that times out is not
  cancelled; what it still has queued is typed afterwards, in one piece.
- The terminal's own answers to the program's queries (a cursor position
  report, for example) are written ahead of queued input and can fall
  between two items of a request.

Input is a list of items of three kinds, and not one string in one
notation, because agents type a lot of `<` (C includes, HTML, shell
redirects). `text` is never parsed, and `keys` is strict:

- `text`: literal. Never parsed. A newline is sent as carriage return, as a
  keyboard would.
- `keys`: vim-style notation inside otherwise literal text. `<Esc>`, `<Enter>`,
  `<Tab>`, `<BS>`, `<Del>`, `<Space>`, `<Up>` and the other arrows, `<Home>`,
  `<End>`, `<PageUp>`, `<PageDown>`, `<F1>` to `<F12>`, modifiers as `<C-x>`,
  `<M-x>`, `<S-Tab>`, `<C-S-Left>`, and `<lt>` for a literal `<`. An
  unrecognised `<name>` is an error, not literal text. Keys are encoded
  according to the terminal's current modes (application cursor keys, keypad
  mode, modifyOtherKeys, kitty keyboard flags), which is why the encoder lives
  with the emulator.
- `paste`: sent as a bracketed paste if the program has enabled mode 2004,
  otherwise as text. The payload is sanitised so it cannot close the bracket
  early. Vim started with `-u NONE` turns the mode on but does not recognise
  the markers, so a paste into it is mangled; `vim --clean` is fine.

Each kind can take its content from a file instead: `{"text_file": PATH}`,
`{"keys_file": PATH}`, `{"paste_file": PATH}`. This is the intended route for
anything long, such as a source file to paste into an editor. The agent or its
harness writes the file and the request names it.

- The host reads the file, so the path must be absolute and readable by the
  host's uid. `waldo-term` makes relative paths absolute before sending.
- It must be a regular file of at most 1 MiB. Its content is used exactly as
  is, so a trailing newline in a `text_file` presses Enter.
- All files in a request are read before any byte is sent. An unreadable file
  is a 400 `file_unreadable` and nothing is sent.
- File items give the caller read access to whatever the host can read. That
  is no more than a caller with the host's uid already has; see section 2.7
  for what it means behind a bridge.

A lone `<Esc>` is followed by a 30 ms pause before the next byte, unless the
program has enabled the kitty keyboard protocol, so the program doesn't read
`<Esc>` plus the next character as an Alt chord.

Within one request, a key that follows text is held until the program has
reacted to the text: the screen has changed since the text was written and
has been still for 30 ms. The wait is at least 40 ms, and if the screen never
reacts, as with a password prompt that echoes nothing, the key is sent after
300 ms. A person never types text and a key in the same instant, and programs
that detect a paste by timing take an Enter that arrives with the text for a
line break inside it. This was found by running Claude Code inside waldo:
text and an Enter sent together left the prompt in its input box with a
blank line under it. A fixed 40 ms pause was not enough on a freshly started
program; waiting for the reaction was. The cause was checked only in part
against that program (version 2.1.322): it treats one input event of more
than 800 characters as a paste and holds a Return that arrives during one,
but where a short text and an Enter in one burst become a line break was not
found.

The first input of a session is held until the screen has settled for the
first time, or until 2 s after the session started, whichever comes first.
A shell echoes what arrives before its first prompt a second time: once by
the terminal, and once by the shell when it draws its prompt. A program that
draws nothing gets its first input at the 2 s mark at the latest.

For curl, a body whose content type is not `application/json` is treated as one
`keys` item, and `wait` and `timeout_ms` may be query parameters:

```
curl --unix-socket "$S" -d ':q!<Enter>' 'http://waldo/v1/input?until=settle'
```

### 2.2 Waiting

`wait.until` is one of:

| Value | Returns when |
|---|---|
| `settle` (default) | The screen is settled (section 3.1) |
| `match` | `wait.pattern` matches a rendered line (section 3.2 rules) |
| `change` | `gen` differs from `wait.since_gen`, or from its value when the request began if that is not given |
| `exit` | The child has exited |
| `done` | The command started by `run` has finished (section 2.5) |
| `none` | Immediately |

With input, `match` ignores text that was already on screen when the request
began, so a wait for the prompt after a command is not satisfied by the
prompt the command was typed at. Without input (`POST /v1/wait`), text
already on screen counts.

Every request has a deadline: `timeout_ms`, default 10 000, maximum 600 000.
On the deadline the response is still 200 with the current screen and
`"reason": "timeout"`. A child exit ends any wait early with
`"reason": "exit"`.

### 2.3 Response

```json
{
  "v": 1,
  "session": "k3f9q2",
  "reason": "settle",
  "waited_ms": 212,
  "interleaved": false,
  "child": {"state": "running", "pid": 48211, "foreground": "vim"},
  "screen": {
    "gen": 41,
    "cols": 120,
    "rows": 30,
    "cursor": {"row": 1, "col": 19, "visible": true},
    "alt_screen": true,
    "title": "main.c + - VIM",
    "text": "#include <stdio.h>\n~\n~\n\n\"main.c\" 1L, 19B written"
  }
}
```

- `reason`: `settle`, `match`, `change`, `exit`, `done`, `none`, or `timeout`.
- `gen` increases whenever the rendered screen changes (text, style or
  cursor). Clients use it with `until: change` and to notice that nothing
  happened. It is a count of changes to a hash of the visible cells, cursor,
  title and size, recomputed after each batch of output.
- `text` has trailing whitespace trimmed from each row and trailing blank rows
  dropped. `rows` gives the real height.
- `interleaved` is true if another client's input reached the pty during this
  request. The text format shows it as the word `interleaved` in the status
  line. Each request's input is typed whole, so it never means mixed-up
  lines; it means the screen, and a `run`'s output, may hold more than the
  request caused, such as the echo of what the other client typed.
- `?style=ansi` (or `"style": "ansi"` in a request body) returns `text` with
  SGR colour and attribute escape sequences left in, and no other escapes.
  Plain text cannot show which button or menu entry is selected; installers
  and dialogs signal that only with colour, and an agent can read the codes.
  The default is plain, because styled screens cost more tokens.
- When `reason` is `match`, a `match` object is added with the fields of
  section 4.3. When it is `exit`, an `exit` object with the fields of section
  4.5, with `output` reduced to its tail.

`Accept: text/plain` or `?format=text` returns one status line followed by the
raw screen text, which is cheaper for an agent to read:

```
[k3f9q2 settle gen=41 120x30 cursor=1,19 alt running fg=vim]
#include <stdio.h>
~
```

`fg` is the terminal's foreground process, the `foreground` of the JSON.

Errors use HTTP status codes and a body like
`{"error": "unknown_key", "text": "unknown key name <Entr> at offset 4; use <lt> for a literal <"}`.
The `error` values are snake_case and stable; `text` is for people.

### 2.4 waldo-term

```
waldo-term list
waldo-term help   [TOPIC]
waldo-term run    [-s ID] [--timeout MS] [--shell sh|fish] COMMAND
waldo-term send   [-s ID] [--text S | --keys S | --paste S]... [--until U] [--timeout MS]
waldo-term screen [-s ID] [--until U] [--json]
waldo-term wait   [-s ID] --until match --pattern RE | --until exit | ...
waldo-term output [-s ID] [--tail N]
waldo-term match add [-s ID] [--id NAME] [--once] PATTERN
waldo-term match rm [-s ID] NAME
waldo-term match list [-s ID]
waldo-term resize [-s ID] [--until none] COLSxROWS
waldo-term attach [-s ID] [--drive] [--resize]
waldo-term kill [ID]
waldo-term signal [-s ID] NAME | --stop
waldo-term gc [--all]
```

`waldo-term screencap` is not built (section 6.3).

- `kill` sends the stop request of section 1.5, waits until the host has
  gone, and prints one line saying how the program ended. It exits 0 if the
  session is over when it returns, including when it had already ended. The
  will is still written, so `output` works afterwards.

- `-s` takes an id, a name, a unique prefix, or a socket path. With exactly one
  live session it may be omitted.
- `--text-file PATH`, `--keys-file PATH` and `--paste-file PATH` send file
  items. `--text -`, `--keys -` and `--paste -` read from stdin, which keeps
  secrets out of `argv`.
- Output is the text format by default.
- Exit status: 0 if the wait condition was met, 124 on deadline, 3 if the child
  exited before the condition, 4 if there is no such session, 5 if a `run`
  command finished with a non-zero status, 2 for usage errors, 1 for anything
  else. The screen is printed in the first three cases and for 5.
- A run-to-completion command may finish before the agent's next call. If the
  session used the default destination, `waldo-term` finds the will there,
  prints the exit details and exits 3; `output` prints the will's output and
  exits 0. If it used an explicit `--dest`, the
  session directory is gone, the agent gets status 4, and the will is wherever
  it pointed `--dest`.

### 2.5 Run: a shell command, start to finish

Settle cannot tell a finished command from a quiet one. `run` can. It is for a
session whose program is a shell sitting at its prompt.

```
$ waldo-term run -s sh 'make test'
[sh done exit=2 4.1s gen=212 120x30 cursor=24,3 running]
cc -c foo.c
foo.c:12: error: expected ';' before 'return'
make: *** [Makefile:8: foo.o] Error 1
```

- `POST /v1/run` with `{"command": "make test", "timeout_ms": 120000}`. The
  default deadline is 120 s, longer than for other requests.
- The host types the command followed by a sentinel: a `printf` of a private
  OSC escape sequence carrying `$?` and a one-time nonce. The sequence draws
  nothing; the emulator hands it to the host, which then knows the command
  finished and with what status. What is typed is `{ COMMAND`, then on a new
  line `}; printf` of the mark: the group stops a command that ends in `&`
  or a comment from swallowing the mark.
- If the shell reports the end of each command itself, with OSC 133 marks
  that carry the exit status, the host uses those and types the command
  alone. It does so once it has seen such a mark in the session, and only
  for a one-line command, because each line of several ends with a mark of
  its own. The output then starts where the shell's output-start mark was.
- Once the end is known, `run` waits for the screen to settle, so the
  response shows the new prompt.
- The response has `"reason": "done"`, `command_exit_code`, the command's
  `output` (the rendered lines between the command and its end, capped head
  and tail like the will's), and the screen. The echoed sentinel is removed
  from `output`; it remains visible on the screen line where it was typed.
- On the deadline the command is still running. The response says `timeout`
  with the output so far, and `waldo-term wait --until done` picks it up
  again.
- An interrupt ends the command. A shell that is interrupted drops the rest
  of the line it was given, and the end mark with it. So once an interrupt
  has been sent (Ctrl-C through `send`, or `signal SIGINT`), the command
  counts as ended when the screen has settled with the shell in the
  foreground again, with exit code 130. A command that ignores the
  interrupt keeps the foreground and ends in the usual way.
- One `run` at a time per session; a second gets 409 `run_in_progress`.
  A run that has ended but is still waiting for the screen to settle is
  answered at once when the next `run` arrives, so each caller gets its
  own result.
- The line is sent as a paste when the shell has enabled bracketed paste, so
  tabs and newlines in the command are taken literally.
- It assumes `;` and `$?`, which holds for sh, bash and zsh. `--shell fish`
  uses `$status`. If the foreground program is not a shell at its prompt the
  result is whatever typing that line into it does. `run` cannot check that
  for certain. It notes the foreground program when it types. A shell
  either finishes the command or puts something else in the foreground, so
  if the same program is still there at the deadline, and its name is not a
  known shell's, the timeout carries a hint saying so.

### 2.6 Help and guidance

An agent arrives knowing nothing about waldo, so the tool has to teach itself.
These are requirements, not polish.

- `waldo-term help` prints a guide of about forty lines: start a session, run
  a command, send keys, wait for a pattern, read the will. Every example is a
  complete command that works as written.
- `waldo-term help TOPIC` covers `keys` (the notation and every key name),
  `wait`, `run`, `messages` and `exit-status`. `waldo-wrap --help` and each
  subcommand's `--help` end with two or three examples.
- Every error's `text` says what to do next, as the `unknown_key` message
  does.
- Responses carry a `hint` when the host can see a likely mistake. The text
  format prints it as a last line starting with `hint:`. The hints:
  - A settle that returns while the shell has a foreground job that this
    request's input started (the terminal's foreground process group is
    not the child's own, and not what it was before the input was sent):
    `hint: 'make' is still running; settle only means the screen went quiet. Use --until match, or waldo-term run.`
    Later requests to the same program carry no hint: by then it is the
    thing being driven.
  - The mirror case: the program the input was sent to has left the
    foreground by the time the screen settles, and the session's own
    program has it again:
    `hint: 'vim' is no longer in the foreground; input now goes to 'bash'.`
  - A match wait with input that times out while the pattern matches text
    that was already there: the hint says that only a new match counts for
    a request that sends input, and to use `wait --until match` instead.
  - A wait for `exit` that times out: the hint names the foreground program
    and says that `exit` means the session's own program.
  - A `run` that times out after typing into something that is not a shell.
    That run is also given up, so the next `run` is not refused:
    `hint: run typed the command while 'vim' was in the foreground, and it still is; that does not look like a shell, and run needs a shell prompt. This run has been given up. ...`
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

Fires exactly once, after the pty is drained. See section 4.5.

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

## 4. Message schema

Asynchronous messages go to the destination as one JSON object each. Every
message has this envelope:

| Field | Meaning |
|---|---|
| `v` | Schema version, 1 |
| `session` | Session id |
| `seq` | Starts at 0, increases by 1 per message, assigned when queued |
| `trigger` | `hello`, `settle`, `match`, `stream`, `heartbeat` or `exit` |
| `time` | Wall clock, RFC 3339 UTC |
| `t_ms` | Milliseconds since the session started, monotonic |
| `dropped` | Present when messages were dropped, or could not be delivered, since the previous one that arrived; the count |

A gap in `seq` means messages were lost, and the next message that arrives
says how many in `dropped`. A `heartbeat` message is the envelope alone.

### 4.1 hello

```json
{"v":1,"session":"k3f9q2","seq":0,"trigger":"hello","time":"2026-11-17T23:41:07.120Z","t_ms":0,
 "name":null,"argv":["vim","main.c"],"cwd":"/home/u/proj","socket":"/run/user/1000/waldo/k3f9q2/sock",
 "cols":120,"rows":30,"host_pid":48200}
```

Sent before the child is spawned; it is the destination probe.

### 4.2 settle

```json
{"v":1,"session":"k3f9q2","seq":7,"trigger":"settle","time":"2026-11-17T23:41:09.482Z","t_ms":2362,
 "screen":{"gen":41,"cols":120,"rows":30,"cursor":{"row":1,"col":19,"visible":true},"alt_screen":true,
           "title":"main.c + - VIM","text":"#include <stdio.h>\n~\n~"}}
```

### 4.3 match

```json
{"v":1,"session":"k3f9q2","seq":12,"trigger":"match","time":"2026-11-17T23:41:15.003Z","t_ms":7883,
 "match":{"id":"pw","pattern":"[Pp]assword.*: $","text":"Password: ","groups":[],
          "count":1,"line":57,"on_screen":true,"row":9,"col":1},
 "screen":{"gen":63,"cols":120,"rows":30,"cursor":{"row":9,"col":11,"visible":true},"alt_screen":false,
           "title":"","text":"$ sudo make install\nPassword:"}}
```

`count` is how many times this trigger has fired. `line` is the absolute row
number. `row` and `col` are present only when `on_screen` is true.

### 4.4 stream

```json
{"v":1,"session":"k3f9q2","seq":20,"trigger":"stream","time":"2026-11-17T23:41:20.250Z","t_ms":13130,
 "from_line":118,"lines":["cc -c foo.c","cc -c bar.c","foo.c:12: warning: unused variable 'x'"]}
```

`from_line` is the number of the first line in `lines`, counting the
session's output lines from 1. `skipped_lines`, when present, is how many
lines just before `from_line` were not sent (section 3.4).

### 4.5 exit (the will)

```json
{"v":1,"session":"k3f9q2","seq":31,"trigger":"exit","time":"2026-11-17T23:42:01.900Z","t_ms":54780,
 "exit_code":2,"signal":null,"core_dumped":false,
 "killed_by_waldo":false,"kill_reason":null,"spawn_error":null,"duration_ms":54612,
 "screen":{"gen":140,"cols":120,"rows":30,"cursor":{"row":24,"col":1,"visible":true},"alt_screen":false,
           "title":"","text":"..."},
 "output":{"text":"$ make\ncc -c foo.c\n[... 2210 lines omitted ...]\nmake: *** [Makefile:8: all] Error 2",
           "lines_total":2260,"truncated":true,"omitted_lines":2210}}
```

- Exactly one of `exit_code` and `signal` is non-null, unless `spawn_error` is
  set. `signal` is a name such as `"SIGSEGV"`.
- `kill_reason` is `request`, `host_signal` or `null`.
- `output.text` is the rendered output: lines that scrolled off plus the final
  primary screen, soft-wrapped lines joined. Carriage-return progress bars
  collapse to their final state because the text is rendered, not raw. It is
  capped (`--output-cap`, default 64 KiB), keeping the first quarter and the
  last three quarters with a marker line between, so a will shows both the
  command and its last errors and is never megabytes on one line. There is
  no other copy: what the cap cuts is gone.
- Output drawn on the alternate screen is not part of `output`; the final
  `screen` shows what was last visible.

The will is the last message the session sends to its destination.

## 5. Destinations

Every session has a destination. If `--dest URI` is not given, the host uses
`file:<session dir>/events.ndjson` and reports it at startup (section 1.1). An
agent with only a Bash tool can ignore it until the end; the will is its last
line. By default only `hello` and `exit` are sent, so the file stays small.

By default the host probes the destination before spawning the child and
refuses to start if `hello` can't be delivered. `--no-probe` skips that: the
host starts regardless and delivers `hello` when it can. Use it when the
receiver comes up after the session does.

| Scheme | Delivery | Probe |
|---|---|---|
| `file:PATH` | Append one line per message, one `write` each, `O_APPEND` | Open for append, write `hello` |
| `unix:PATH` | Connect once, stream NDJSON | Connect, write `hello` |
| `http://HOST[:PORT]/PATH` | POST each message, `Content-Type: application/json` | POST `hello`, require 2xx |

`https:` is out of scope; put a local proxy in front if needed.

- `unix:` keeps the connection the probe opened for the whole session. If a
  write fails, the message is lost and the next one connects again. Writes
  time out after 5 s.
- `http:` opens a connection for each message and reads only the status
  line. That is slower than keeping one open, and it cannot go stale. An
  IPv6 address is written in brackets. Connecting, writing and reading each
  time out after 5 s.

**Queue.** The delivery thread reads from a bounded queue (256 messages or
8 MiB). When it is full:

- a queued `settle` is replaced by a newer `settle`, which takes its place
  and its `seq`; a `stream` message that follows another is merged into it;
- otherwise the oldest `match` or `stream` message is dropped, then the
  oldest `settle`, and counted in the next message's `dropped`;
- `hello` and `exit` are never dropped.

**Delivery guarantees.** At most once for `file:` and `unix:`. At least once
for `http:`, which tries a failed message twice more, after 250 ms and after
1 s, so receivers should de-duplicate on `(session, seq)`. A message that
still fails is counted in `dropped`. The log says when delivery starts
failing and when it works again, not once per message.

**The will.** On delivery failure the host retries for up to 5 s. If it still
can't deliver, it writes the will to `will.json` in the session directory and
leaves the directory in place.

**Detecting a dead host.** The host cannot announce its own SIGKILL or OOM
kill. Receivers can tell by:

- `unix:`: end of stream with no `exit` message.
- `file:`, `http:`, or anyone else: the session directory still exists, its
  `lock` is free, and it holds no `will.json`. A clean exit removes the
  directory. `waldo-term list` reports this state as `dead`.
- `http:` receivers with no filesystem access can ask for `--heartbeat SECONDS`
  and treat silence as death. A `heartbeat` message is sent whenever that
  long has passed without another message. It works with any destination.

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

The attach stream is an HTTP upgrade on the control socket (`GET /v1/attach`
with `Upgrade: waldo-attach`) carrying one JSON object per line in both
directions, so it can be read with a few lines of any language. A frame from
the host carries the session and view sizes, the cursor, the title, how many
requests are waiting, three input modes, and `lines`: pairs of row number and
styled text, only for rows that changed. The viewer sends `viewport`,
`resize`, `keys_hex` and `detach`.

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

- Attach is view-only by default. `attach --drive`, or Ctrl-\ inside attach,
  lets the human type; Ctrl-\ again stops. Ctrl-] detaches, and so does `q`
  while not typing.
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

## 8. Failure modes

| Situation | Behaviour |
|---|---|
| Program not found or exec fails | `waldo-wrap` exits non-zero with the errno text. No socket path is printed. If exec fails after the probe, the destination receives an `exit` with `spawn_error` |
| Destination unreachable at start | Exit non-zero before spawning the child. With `--no-probe`, start anyway and queue |
| Destination fails later | Queue fills, messages coalesce or drop with `dropped` counts; the pty is unaffected |
| Destination unreachable at exit | Retry 5 s, then write `will.json` in the session directory and keep the directory |
| Agent calls `waldo-term` after the session ended | Default destination: the will is read from it, status 3. Explicit `--dest`: status 4, and the will is at that destination |
| `run` used when the foreground program is not a shell | The command line is typed into whatever is there. The sentinel never arrives and the request times out, with a hint that names the program |
| A request arrives during `--linger` | Status, screen, output and triggers are answered with reason `exit`; anything else gets 409 `exited` |
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
detect completion; `waldo-term run` (section 2.5) does the same with a sentinel
that draws nothing.
