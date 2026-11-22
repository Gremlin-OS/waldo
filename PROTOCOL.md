# Waldo protocol

This is the reference for talking to a waldo session without `waldo-term`:
enough to write another client, another command-line front end, or a harness
integration. It describes what `waldo-wrap` does today. `DESIGN.md` has the
reasons; this file has the facts.

Every response and message carries `"v": 1`. Waldo has no tagged release
yet, so version 1 can still change, and this file changes with it. A client
should ignore fields it does not know.

A client deals with four things:

1. **Sessions on disk**: finding a session, telling whether it is alive, and
   reading how it ended (section 1).
2. **The control API**: HTTP over a unix socket, for input, waiting and
   reading the screen (sections 2 to 6).
3. **The attach stream**: a live view, for showing a session to a person
   (section 7).
4. **Messages**: what a session sends to its destination, ending with the
   will (section 8).

Section 9 lists every error. Section 10 says what `waldo-term` adds on top,
for anyone replacing it.

## 1. Sessions on disk

There is no daemon and no registry. A session is one `waldo-wrap` process
and one directory.

### 1.1 Starting a session

```
waldo-wrap [options] -- PROGRAM [ARGS...]
```

| Option | Meaning |
|---|---|
| `--detach` | Return once the session is ready; the host keeps running |
| `--name NAME` | Session id: 1 to 32 of `a-z 0-9 _ -`, starting with a letter or digit. Default: six random characters |
| `--dest URI` | Where messages go (section 8). Default: `file:<session dir>/events.ndjson` |
| `--no-probe` | Do not require the destination to accept `hello` at startup |
| `--no-broadcast` | Clients cannot broadcast to each other (section 4.10) |
| `--on LIST` | Also send `settle` and/or `stream` messages. Comma separated |
| `--heartbeat SECS` | Send a `heartbeat` after that long without another message |
| `--linger SECS` | Keep answering read-only requests that long after the program exits |
| `--record FILE` | Write the program's raw output to an asciicast v2 file |
| `--size COLSxROWS` | Terminal size, 2x1 to 500x200. Default 120x30 |
| `--output-cap BYTES` | How much rendered output to keep. Default 65536 |
| `--quiet-ms MS` | How long the screen must be still to count as settled. Default 200 |
| `--json` | Print the coordinates as one JSON object |

On success it prints exactly this on stdout, and nothing else:

```
session=k3f9q2
socket=/run/user/1000/waldo/k3f9q2/sock
dest=file:/run/user/1000/waldo/k3f9q2/events.ndjson
```

With `--json`: `{"session": "...", "socket": "...", "dest": "..."}`.

On failure it prints one line on stderr and exits non-zero, having printed
no coordinates. Without `--detach` it stays in the foreground and exits with
the program's exit status, or 128 plus the signal number.

The program's environment has `TERM=xterm-256color`, `COLORTERM=truecolor`,
`WALDO_SESSION` (the id) and `WALDO_SOCKET` (the socket path).

### 1.2 The session directory

The base directory is `$WALDO_DIR`, else `$XDG_RUNTIME_DIR/waldo`, else
`/tmp/waldo-<uid>`. It must be a real directory (not a symlink), owned by
the caller, with mode 0700. Each session is a directory under it, named by
the session id:

| File | Content |
|---|---|
| `sock` | The control socket, mode 0600 |
| `lock` | Locked exclusively (`flock`) by the host while the session is alive |
| `meta.json` | See below |
| `events.ndjson` | The messages, when the destination is the default |
| `will.json` | The exit message, only if it could not be delivered |
| `log` | The host's log. Never contains typed input |

`meta.json`:

```json
{
  "id": "k3f9q2",
  "name": null,
  "argv": ["vim", "main.c"],
  "cwd": "/home/u/proj",
  "child_pid": 48211,
  "host_pid": 48200,
  "started": "2026-11-17T23:41:07.120Z",
  "cols": 120,
  "rows": 30,
  "socket": "/run/user/1000/waldo/k3f9q2/sock",
  "dest": "file:/run/user/1000/waldo/k3f9q2/events.ndjson"
}
```

`name` is the `--name` value or null. `cols` and `rows` are the size at
start; the live size is in every response.

### 1.3 Is it alive?

A session is alive exactly while its host holds the lock. To test: open
`lock` and try `flock(fd, LOCK_SH | LOCK_NB)`. If that fails, the session is
alive. If it succeeds, unlock; the session has ended. The kernel releases
the lock however the host dies.

A directory without a `lock` file is not a session.

The host releases the lock itself after the will is delivered and before it
answers any request waiting for `exit`. So a client that receives reason
`exit` can remove the session, or start one with the same name, at once.
With `--linger` the lock is held until the lingering ends.

The state of a session directory:

| State | Test |
|---|---|
| `live` | The lock is held |
| `undelivered` | Not held, and `will.json` exists |
| `exited` | Not held, and the will can be read from a `file:` destination (section 1.4) |
| `dead` | Not held, and no will: the host was killed |

With an explicit `--dest`, the host removes the whole directory when it
exits after delivering the will, so there is nothing to find afterwards.

### 1.4 Reading how it ended

The will is the session's last message (section 8.6). After the session has
ended, look for it in this order:

1. `will.json` in the session directory.
2. If `dest` in `meta.json` starts with `file:`, the last non-empty line of
   that file, if its `trigger` is `exit`.

Otherwise the will went to the destination and is not on disk.

### 1.5 Removing an ended session

Remove `sock`, `meta.json`, `will.json`, `events.ndjson`, `log` and `lock`
from the directory, then the directory. Only do this when the lock is free.

### 1.6 Ending a live session

`POST /v1/signal` with `{"stop": true}`, then wait until the lock is free.
The host sends the program SIGHUP, then SIGTERM after 2 s, then SIGKILL
after 2 more.

## 2. Transport

The control API is HTTP/1.1 on the unix stream socket `sock`.

- The `Host` header is ignored; send any value.
- The peer must have the host's uid (checked with `SO_PEERCRED`). A
  connection from another user is closed without a reply.
- Request bodies need a `Content-Length`. A chunked body gets 411. Bodies
  are at most 1 MiB, and request headers at most 64 KiB.
- `Expect: 100-continue` is answered with `100 Continue`.
- Keep-alive works: HTTP/1.1 connections stay open unless the request says
  `Connection: close`. Requests on one connection are handled one at a time,
  in order; the next is read when the previous has been answered.
- A connection that goes away cancels its pending request. Input the
  request had already queued is still typed.
- Several connections may be open at once, from one client or many.

Responses are JSON (`Content-Type: application/json`, one object followed by
a newline) unless the request asks for text (section 3.5). Errors are always
JSON (section 9).

Rows and columns in the API count from 1. Columns count terminal cells, so a
wide character takes two.

Example with curl:

```
curl --unix-socket "$SOCK" http://waldo/v1/screen
curl --unix-socket "$SOCK" -H 'Content-Type: application/json' \
     -d '{"input":[{"text":"ls"},{"keys":"<Enter>"}]}' http://waldo/v1/input
```

## 3. Common parts

### 3.1 Wait parameters

Every request that returns a screen can wait first. The parameters are the
same everywhere:

| Name | Meaning |
|---|---|
| `until` | `settle`, `match`, `change`, `exit`, `broadcast` or `none` (section 5) |
| `pattern` | For `match`: a regular expression against one line |
| `since_gen` | For `change`: return when `gen` differs from this. Default: `gen` when the request arrived |
| `style` | `plain` (default) or `ansi` (section 3.3) |
| `timeout_ms` | The deadline. Default 10000; values above 600000 are lowered to it. `timeout` is accepted as another name |
| `as` | The client's name, for broadcasts (section 4.10): 1 to 32 characters from letters, digits, `_` and `-`. Optional |

Each is looked for in three places, in this order: the query string; the
`wait` object in a JSON body; the top level of a JSON body. So these mean
the same:

```
POST /v1/wait?until=match&pattern=ready
POST /v1/wait   {"wait": {"until": "match", "pattern": "ready"}}
POST /v1/wait   {"until": "match", "pattern": "ready"}
```

A number may be sent as a JSON number or as a string of digits.

### 3.2 The response

Requests that return a screen all answer 200 with this object, including
when the deadline passed:

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
    "text": "#include <stdio.h>\n~\n~"
  }
}
```

| Field | Meaning |
|---|---|
| `reason` | Why the request returned: `settle`, `match`, `change`, `exit`, `broadcast`, `none` or `timeout` |
| `waited_ms` | How long the request waited |
| `interleaved` | True if someone else's input, signal or resize took effect during this request |
| `changed` | Only on a request that typed something: false if `gen` is the same as when its first input was written, so nothing on screen changed, not even the cursor |
| `child.state` | `running` or `exited` |
| `child.pid` | The program's process id |
| `child.foreground` | While running: the name of the process that leads the terminal's foreground process group, or null if it cannot be read |
| `screen` | Section 3.3 |
| `match` | Only when `reason` is `match` (below) |
| `exit` | When `reason` is `exit` and it was the session's own program that exited (below) |
| `broadcasts` | Only when there are some to give: what other clients said (section 4.10) |
| `hint` | Sometimes: a sentence about a likely mistake and what to do |

`match`, when a wait matched:

```json
{"id": null, "pattern": "^ready$", "text": "ready", "groups": [],
 "count": 1, "line": 57, "on_screen": true, "row": 9, "col": 1}
```

`text` is what the pattern matched and `groups` its capture groups (null for
a group that did not take part). `line` is the absolute number of the output
line. `row` and `col` are there only when `on_screen` is true; a line can
match as it scrolls off.

`exit`, when the session's own program has exited: the fields of the will
(section 8.6) without the envelope and without `screen`, and with `output`
cut to its last 40 lines. When `until: exit` was answered because a program
inside the session exited, the reason is `exit`, there is no `exit` object,
and `child.state` is still `running`.

### 3.3 The screen

| Field | Meaning |
|---|---|
| `gen` | A counter that goes up whenever the visible screen, cursor, title or size changes |
| `cols`, `rows` | The terminal size now |
| `cursor` | `row` and `col` from 1, and `visible` |
| `alt_screen` | True while a full-screen program has the alternate screen |
| `title` | The window title the program set |
| `text` | The rows, joined with newlines. Trailing spaces are trimmed from each row and trailing blank rows are dropped |

With `style=ansi`, `text` keeps colour and attributes as SGR escape
sequences and has no other escapes. Each run of same-styled cells starts
with a complete sequence that begins with a reset, such as `ESC[0;1;31m`,
and a row that set any style ends with `ESC[0m`. Colours are written as the
program chose them: `30`-`37`, `40`-`47`, `90`-`97`, `100`-`107`, `38;5;N`,
`38;2;R;G;B`, and the same for backgrounds with 48.

### 3.4 Output

Separately from the screen, the host keeps the session's output: every line
that has scrolled off the primary screen, plus what is on it now. Lines the
terminal wrapped are joined. It is what a pager would show, as text.

```json
{"text": "...", "lines_total": 2260, "truncated": true, "omitted_lines": 2210}
```

It is capped at `--output-cap` bytes. When it is over, the first quarter and
the last three quarters are kept, with one line `[... N lines omitted ...]`
between. What a full-screen program draws on the alternate screen is not
output.

### 3.5 Text responses

With `Accept: text/plain`, or `?format=text`, a 200 response is text
(`?format=json` forces JSON):

- A screen response is one status line, then `screen.text`, then
  `hint: ...` if there is a hint:

  ```
  [k3f9q2 settle gen=41 120x30 cursor=1,19 alt running fg=vim]
  #include <stdio.h>
  ~
  ```

  The status line is, in order: the session; the reason; `gen=N`;
  `COLSxROWS`; `cursor=ROW,COL`; `alt` if on the alternate screen; `running`
  and `fg=NAME`, or `exited`; `unchanged` if `changed` is false;
  `interleaved` if true; and for an exit, how it
  ended: `code=N`, or `signal=NAME` and perhaps `core_dumped`, then
  `killed_by_waldo` if so.
- `GET /v1/output` is the output text and a newline.
- `GET /v1/status` is the JSON, indented.
- The trigger endpoints always answer in JSON.

## 4. Endpoints

| Method and path | Purpose |
|---|---|
| `GET /v1/status` | What the session is |
| `GET /v1/screen` | The screen, after an optional wait |
| `POST /v1/wait` | Wait, then the screen |
| `POST /v1/input` | Type, wait, then the screen |
| `GET /v1/output` | The output so far |
| `POST /v1/resize` | Change the terminal size |
| `POST /v1/signal` | Signal the program, or end the session |
| `GET /v1/triggers` | List match triggers |
| `POST /v1/triggers` | Add one |
| `DELETE /v1/triggers/ID` | Remove one |
| `GET /v1/broadcasts` | What clients have said to each other |
| `POST /v1/broadcasts` | Say something to the other clients |
| `GET /v1/attach` | Upgrade to the attach stream (section 7) |

A wrong method gets 405; an unknown path gets 404 with the list of
endpoints in its text.

### 4.1 GET /v1/status

```json
{
  "v": 1,
  "session": "k3f9q2",
  "name": null,
  "argv": ["vim", "main.c"],
  "cwd": "/home/u/proj",
  "socket": "/run/user/1000/waldo/k3f9q2/sock",
  "dest": "file:/run/user/1000/waldo/k3f9q2/events.ndjson",
  "host_pid": 48200,
  "started": "2026-11-17T23:41:07.120Z",
  "uptime_ms": 5310,
  "cols": 120,
  "rows": 30,
  "gen": 41,
  "child": {"state": "running", "pid": 48211, "foreground": "vim"},
  "broadcasts": true,
  "messages": {"settle": false, "stream": false, "triggers": 1}
}
```

`messages` says which optional messages are on and how many triggers exist.
`broadcasts` is false for a session started with `--no-broadcast`.

### 4.2 GET /v1/screen

Takes the wait parameters in the query string. The default `until` is
`none`, so it returns at once. Answers with the response of section 3.2.

### 4.3 POST /v1/wait

Takes the wait parameters in the query string or a JSON body; the body may
be empty. The default `until` is `settle`. Answers with the response of
section 3.2.

### 4.4 POST /v1/input

```json
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

`input` is a list of items, typed in order. Each item is an object with
exactly one of these keys, whose value is a string (a number for `pause`):

| Key | Meaning |
|---|---|
| `text` | Literal characters. Nothing is special. A newline is sent as Enter (carriage return) |
| `keys` | Key notation (section 4.5) |
| `paste` | A paste: bracketed if the program has turned bracketed paste on, as plain text otherwise. The content is sanitised so it cannot end the bracket early |
| `text_file`, `keys_file`, `paste_file` | The same, with the content read from the named file |
| `pause` | Nothing is typed. The value is a number of milliseconds, at most 10000: what follows is typed no sooner than that long after what came before |

`key_delay_ms`, at the top level of the body or in the query string, is a
minimum gap between the request's items, at most 10000. With it, each key
name and each literal character of a `keys` item is typed separately, with
that gap between them. `text` and `paste` items are still typed in one
piece. Use it, or a `pause`, for a program that discards keys which arrive
while it is opening a prompt or a menu.

For the `_file` kinds the host reads the file, so the path must be absolute
and readable by the host. It must be a regular UTF-8 file of at most 1 MiB,
and is used exactly as it is: a final newline in a `text_file` is an Enter.

All items are checked, and all files read, before anything is typed. If any
item is bad the request is refused and nothing is sent.

The wait parameters apply after the input; the default `until` is `settle`.
The answer is the response of section 3.2.

A body that is not JSON (its `Content-Type` is not `application/json` and it
does not start with `{`) is taken as one `keys` item, with the wait
parameters in the query string:

```
curl --unix-socket "$SOCK" -d ':q!<Enter>' 'http://waldo/v1/input?until=settle'
```

### 4.5 Key notation

A `keys` string is literal text with key names in angle brackets.

- `<` starts a key name if a `>` follows within 40 characters with no
  space and no other `<` between. Otherwise it is a literal `<`.
- Names, in any case: `Esc` (`Escape`), `Enter` (`CR`, `Return`), `Tab`,
  `BS` (`Backspace`), `Del` (`Delete`), `Space`, `Up`, `Down`, `Left`,
  `Right`, `Home`, `End`, `PageUp` (`PgUp`), `PageDown` (`PgDn`), `Insert`
  (`Ins`), `F1` to `F12`.
- `<lt>` is a literal `<`.
- Modifiers are prefixes and combine: `C-` Ctrl, `M-` or `A-` Alt, `S-`
  Shift. `<C-c>`, `<M-x>`, `<S-Tab>`, `<C-S-Left>`.
- A single character is a key only with a modifier: `<C-c>` is a key, `<c>`
  is an error.
- Anything else in brackets is the error `unknown_key`, and nothing is
  sent. Text that contains `<` (code, HTML, redirects) belongs in a `text`
  item.

Keys are encoded when they are typed, for the modes the program has set at
that moment: application cursor keys, keypad mode, modifyOtherKeys and the
kitty keyboard protocol.

### 4.6 GET /v1/output

`?tail=N` limits it to the last N lines. Answers:

```json
{"v": 1, "session": "k3f9q2", "output": {"text": "...", "lines_total": 12, "truncated": false, "omitted_lines": 0}}
```

### 4.7 POST /v1/resize

```json
{"cols": 100, "rows": 30}
```

Sizes are 2x1 to 500x200. The program gets SIGWINCH. The answer is the
response of section 3.2. By default it waits for the program to redraw: it
returns `settle` when the screen has gone still, or `none` with the screen
as it is after 1 s. A `wait` object in the body replaces that default;
`{"wait": {"until": "none"}}` answers as soon as the size has changed.

### 4.8 POST /v1/signal

```json
{"signal": "SIGINT"}
{"stop": true}
```

`signal` is a name with or without `SIG`, in any case: HUP, INT, QUIT, ILL,
ABRT, FPE, KILL, SEGV, PIPE, ALRM, TERM, USR1, USR2, CONT, STOP, TSTP, BUS,
TRAP or WINCH. It goes to the program's process group and to the terminal's
foreground process group, which is what Ctrl-C at a keyboard reaches. The
answer is the response of section 3.2 with reason `none`, sent once the
signal has been delivered.

`stop` ends the session: SIGHUP, SIGTERM after 2 s, SIGKILL after 2 more.
It is answered at once; the session ends a little later.

A Ctrl-C typed with `{"keys": "<C-c>"}` is different: it is a byte written
to the terminal, and the program's terminal mode decides whether it becomes
a signal or arrives as a key.

### 4.9 Triggers

A trigger is a standing pattern. Each time a line matches, the session
sends a `match` message to its destination (section 8.3). At most 32 per
session.

`POST /v1/triggers`:

```json
{"pattern": "[Pp]assword: $", "id": "pw", "mode": "edge"}
```

Only `pattern` is required. `id` is 1 to 32 characters from letters, digits,
`_` and `-`; without one the host makes up `t1`, `t2` and so on. `mode` is
`edge` (the default: fire once per appearance) or `once` (fire once, then
remove itself).

All three trigger endpoints answer with the list, plus what was added or
removed:

```json
{
  "v": 1,
  "session": "k3f9q2",
  "dest": "file:/run/user/1000/waldo/k3f9q2/events.ndjson",
  "triggers": [{"id": "pw", "pattern": "[Pp]assword: $", "mode": "edge", "count": 0}],
  "added": {"id": "pw", "pattern": "[Pp]assword: $", "mode": "edge", "count": 0}
}
```

`DELETE /v1/triggers/pw` answers the same with `removed` in place of
`added`. `count` is how many times the trigger has fired.

### 4.10 Broadcasts

A broadcast is one line of text that a client says to the other clients
of the session. It is never typed into the program and never appears on
the screen. The host keeps the last 32.

`POST /v1/broadcasts`:

```json
{"text": "vim is closed; the shell is yours", "as": "ann"}
```

`text` is required: one line of at most 1000 bytes with no control
characters; space at its ends is dropped. `as` is the sender's name and is
optional; it may also be given in the query string. The answer:

```json
{"v": 1, "session": "k3f9q2",
 "broadcast": {"n": 3, "from": "ann", "text": "vim is closed; the shell is yours", "t_ms": 61250}}
```

`n` counts the session's broadcasts from 1. `from` is null for a sender
without a name. `t_ms` is milliseconds since the session started.

A client receives broadcasts in four ways. An attached viewer gets them on
its stream (section 7.2). Any other client:

- **On its own responses.** A request with the wait parameter `as` gets a
  `broadcasts` list in its response (section 3.2): every broadcast kept
  that this name has not been given before, oldest first, leaving out the
  ones it sent under that name. Each is given to a name once. A name the
  host has not seen before is given everything kept. A request without
  `as` gets none. The host remembers 64 names.
- **By waiting.** `until: broadcast` returns, with reason `broadcast`, as
  soon as there is one to give. With `as`, that is one this name has not
  been given, so it returns at once if some are waiting. Without `as`, it
  is one made after the request began, from anyone.
- **By asking.** `GET /v1/broadcasts` answers with all that are kept, or
  with `?since=N` those numbered above N, and `last`, the number of the
  newest. It changes nothing about what any name has been given.

```json
{"v": 1, "session": "k3f9q2", "last": 3,
 "broadcasts": [{"n": 3, "from": "ann", "text": "vim is closed; the shell is yours", "t_ms": 61250}]}
```

A session started with `--no-broadcast` has none of this. Both endpoints
and `until: broadcast` answer 409 `broadcasts_off`, a broadcast sent over
the attach stream is dropped, and `as` is accepted and does nothing, so a
client that always names itself still works. `GET /v1/status` says which
kind of session it is.

Each broadcast also goes to the destination (section 8.9). The name is
what the sender says it is: every client of a session is the same user,
and the host does not check it.

## 5. Waiting

### 5.1 The conditions

| `until` | Returns when |
|---|---|
| `settle` | The screen is settled (5.2) |
| `match` | `pattern` matches a line (5.3) |
| `change` | `gen` differs from `since_gen` |
| `exit` | The program that had the terminal's foreground when the request began has exited. That is the session's own program, or a program started inside it, such as an editor run from a shell |
| `broadcast` | Another client has said something this one has not been given (section 4.10) |
| `none` | At once |

Whatever the condition:

- A request that carries input is not answered until all of that input has
  been typed, except with `none`. So a pattern that matches the echo of the
  request's own text does not return it with its Enter still to come.
- At the deadline the request returns with reason `timeout` and the screen
  as it is. That is a 200, not an error.
- If the session's own program exits first, the request returns with reason
  `exit` and the `exit` object, once the program's remaining output has been
  read.

### 5.2 Settled

The screen is settled when all of these hold:

1. `gen` has not changed for the quiet window: 200 ms, or `--quiet-ms`. The
   window is counted from the later of the last screen change and the last
   input written. It is 40 ms when the shell has announced a prompt (OSC 133
   prompt start) since the last input.
2. All queued input has been written.
3. The program is not holding a synchronized update open (mode 2026). A
   hold is honoured for at most 1 s.
4. The screen has shown some text, or input has been sent. A program that
   has drawn nothing is not settled; a wait on it runs to the deadline.

Settled means nothing is being drawn. It does not mean a command has
finished: a command that is silent for longer than the window looks
settled. To know that a shell command ended, see section 6.5.

### 5.3 Matching

- The pattern is a regular expression without backtracking (the syntax of
  Rust's `regex-lite`: no look-around, no back-references).
- It is tried against one line at a time: rows the terminal wrapped are
  joined into one line first. It never spans lines. A line is cut to 4096
  characters for matching.
- The line the cursor is on is included, padded with spaces up to the
  cursor, so a prompt that ends in a space with no newline can be matched:
  `assword: $`.
- Lines are looked at on the visible screen, and once more as each scrolls
  off the primary screen, so fast output that never rests on screen is
  still seen.
- With input (`POST /v1/input`), a match that was already on screen when
  the request arrived does not count: the wait is for something new. If
  the deadline passes while only such matches exist, the hint says so.
- Without input (`POST /v1/wait`, `GET /v1/screen`), text already on screen
  matches at once.

## 6. What happens to input

### 6.1 Order

The session has one input queue. Each request's items are typed in order
and without interruption: input from another request, or from a person
attached to the session, never falls inside one request.

Signals and resizes go through the same queue. One sent while another
request is being typed takes effect after it. There are two exceptions:

- `{"stop": true}` never waits.
- If the queue is stuck because the program is not reading its input and
  the terminal's buffer is full, signals and resizes are carried out at
  once.

A request is not all-or-nothing. If the program exits partway through, the
rest of the request is dropped. A request that reaches its deadline is not
cancelled: what it still has queued is typed afterwards.

### 6.2 Timing

The host does not type everything at once. A client does not have to do
anything about these, but they explain the delays:

- **The first input of a session** waits until the screen has settled for
  the first time, or until 2 s after the session started.
- **A key that follows text** in the same request waits until the program
  has reacted to the text: the screen has changed and then been still for
  30 ms. The wait is at least 40 ms and at most 300 ms.
- **A lone Escape** is followed by a 30 ms pause, unless the program uses
  the kitty keyboard protocol, so it is not read as the start of an Alt
  chord.
- **A signal or resize** that follows input waits for the program to react
  to that input in the same way, at most 300 ms.
- **Delays the request asked for**: a `pause` item, and `key_delay_ms`
  between items (section 4.4). These add to the waits above.

### 6.3 Interleaving

When another client's input, signal or resize takes effect while a request
is waiting, that request's response has `"interleaved": true`. It does not
mean anything was mixed up. It means the screen may show more than the
request caused.

### 6.4 Hints

A response may carry `hint`: a sentence for the agent. The host adds one
when it can see a likely mistake:

- A settle returned while a command that this request started still has the
  foreground.
- The program the input was typed into has left the foreground.
- Text that looks like key notation was sent as `text`.
- A timeout: the screen never went quiet; the program has not drawn
  anything; the pattern did not appear, or only matched what was already
  there; the session has not ended.

Hints are advice, and their wording may change. Do not parse them.

### 6.5 Shell commands

There is no request for "run this command and tell me how it ended". Type
the command with a marker after it and wait for the marker:

```json
{"input": [{"text": "make test; echo \"rc=$?\""}, {"keys": "<Enter>"}],
 "wait": {"until": "match", "pattern": "^rc=[0-9]+$"},
 "timeout_ms": 120000}
```

The screen in the response shows the output and then `rc=N`. If the output
is longer than the screen, read it with `GET /v1/output`. Choose a marker
the output will not contain.

## 7. The attach stream

Attach is for showing a session to a person and letting them type. The host
sends the screen as it changes; it never passes the program's own bytes
through.

### 7.1 Opening it

```
GET /v1/attach?cols=100&rows=29 HTTP/1.1
Host: waldo
Upgrade: waldo-attach
Connection: Upgrade
```

`cols` and `rows` are the size of the viewer's drawing area, 1 to 1000;
without them the session's size is used. The host answers

```
HTTP/1.1 101 Switching Protocols
Upgrade: waldo-attach
Connection: Upgrade
```

and from then on each direction carries one JSON object per line. Without
the `Upgrade` header the answer is 400 `upgrade_required`.

### 7.2 Frames from the host

```json
{
  "session": "k3f9q2",
  "cols": 120,
  "rows": 30,
  "view": {"cols": 100, "rows": 29, "x": 0, "y": 1},
  "cursor": {"row": 3, "col": 2, "visible": true},
  "title": "",
  "alt_screen": false,
  "busy": 0,
  "modes": {"app_cursor": false, "app_keypad": false, "bracketed_paste": true},
  "lines": [[0, "$ ls"], [1, "a.c  b.c"]]
}
```

| Field | Meaning |
|---|---|
| `cols`, `rows` | The session's size |
| `view` | The part of the screen this viewer is shown: its size, and the offset of its top left corner in the session's screen. The view is the viewer's size or the session's, whichever is smaller. When the session is larger, the view follows the cursor |
| `cursor` | The cursor's place in the view |
| `busy` | How many API requests are waiting. Non-zero means an agent is at work |
| `broadcasting` | False for a session started with `--no-broadcast`. A frame without the field is from a host that allows them |
| `modes` | Terminal modes the viewer's terminal should copy, so that the person's keys are encoded as the program expects |
| `lines` | Pairs of row and text, only for rows that changed since the last frame. The first frame has every row |
| `broadcasts` | Only when a client has said something since this viewer's last frame: the broadcasts (section 4.10), oldest first, the viewer's own among them. A frame is sent for them even if nothing on screen changed |

**Rows and columns in frames count from 0**, unlike the rest of the API.
Row text is styled in the way of `style=ansi` (section 3.3), already cut to
the view's width.

A frame is sent when something changed, at most every 16 ms, and not while
the program holds a synchronized update open. A viewer that cannot keep up
has frames skipped, not queued.

When the session ends, the host sends `{"ended": true}` and closes.

### 7.3 Messages from the viewer

Each is one JSON object on a line. Unknown keys are ignored, and one object
may carry several of these:

| Message | Meaning |
|---|---|
| `{"viewport": {"cols": 80, "rows": 23}}` | The viewer's drawing area changed. The next frame has every row |
| `{"keys_hex": "6c730d"}` | Bytes to type, as hex: what the viewer's terminal produced for the person's keys. They join the input queue as one request |
| `{"resize": {"cols": 80, "rows": 23}}` | Make the session this size. The first size is restored when the last viewer that resized has gone |
| `{"broadcast": {"text": "typed it", "as": "chris"}}` | Say something to the other clients, as `POST /v1/broadcasts` does (section 4.10). `as` is optional. One that breaks that section's rules is dropped without an answer, so a viewer should check first |
| `{"detach": true}` | Close the stream. The session carries on |

A viewer that only watches sends nothing.

## 8. Messages

A session sends messages to its destination: one JSON object each.

### 8.1 The envelope

Every message starts with:

| Field | Meaning |
|---|---|
| `v` | 1 |
| `session` | The session id |
| `seq` | Counts from 0, one per message |
| `trigger` | `hello`, `settle`, `match`, `stream`, `heartbeat`, `broadcast` or `exit` |
| `time` | Wall clock, RFC 3339 UTC with milliseconds |
| `t_ms` | Milliseconds since the session started |
| `dropped` | Only when messages were lost since the last one that arrived: how many |

`hello` and `exit` are always sent. The others are sent when asked for.

### 8.2 hello

The first message, sent before the program starts.

```json
{"v":1,"session":"k3f9q2","seq":0,"trigger":"hello","time":"2026-11-17T23:41:07.120Z","t_ms":0,
 "name":null,"argv":["vim","main.c"],"cwd":"/home/u/proj","socket":"/run/user/1000/waldo/k3f9q2/sock",
 "cols":120,"rows":30,"host_pid":48200}
```

### 8.3 match

Sent when a trigger fires (section 4.9).

```json
{"v":1,"session":"k3f9q2","seq":12,"trigger":"match","time":"2026-11-17T23:41:15.003Z","t_ms":7883,
 "match":{"id":"pw","pattern":"[Pp]assword.*: $","text":"Password: ","groups":[],
          "count":1,"line":57,"on_screen":true,"row":9,"col":1},
 "screen":{"gen":63,"cols":120,"rows":30,"cursor":{"row":9,"col":11,"visible":true},"alt_screen":false,
           "title":"","text":"$ sudo make install\nPassword:"}}
```

The `match` object is that of section 3.2, with the trigger's `id` and its
`count` so far. The `screen` is always plain.

A trigger fires when a match appears that it has not already reported. A
match is the line's number together with the matched text. It is forgotten
when the screen settles without it, so text that stays, or is repainted
before the screen goes still, does not fire again. Text that is on screen
when a trigger is added fires. Matches present right after a resize are
recorded without firing, because a resize renumbers lines.

### 8.4 settle

With `--on settle`: sent each time the screen settles after having changed.

```json
{"v":1,"session":"k3f9q2","seq":7,"trigger":"settle","time":"2026-11-17T23:41:09.482Z","t_ms":2362,
 "screen":{"gen":41,"cols":120,"rows":30,"cursor":{"row":1,"col":19,"visible":true},"alt_screen":true,
           "title":"main.c + - VIM","text":"#include <stdio.h>\n~\n~"}}
```

### 8.5 stream and heartbeat

With `--on stream`: output lines as they scroll off the primary screen, at
most one message every 250 ms. When the program exits, the lines still on
screen follow, so the stream as a whole is the output.

```json
{"v":1,"session":"k3f9q2","seq":20,"trigger":"stream","time":"2026-11-17T23:41:20.250Z","t_ms":13130,
 "from_line":118,"lines":["cc -c foo.c","cc -c bar.c"]}
```

`from_line` is the number of the first line, counting the session's output
lines from 1. `skipped_lines`, when present, is how many lines just before
`from_line` were not sent, because they arrived faster than they could be.

With `--heartbeat SECS`: a message that is the envelope alone, sent when
that long has passed without any other message.

### 8.6 exit: the will

The last message.

```json
{"v":1,"session":"k3f9q2","seq":31,"trigger":"exit","time":"2026-11-17T23:42:01.900Z","t_ms":54780,
 "exit_code":2,"signal":null,"core_dumped":false,
 "killed_by_waldo":false,"kill_reason":null,"spawn_error":null,"duration_ms":54612,
 "screen":{"gen":140,"cols":120,"rows":30,"cursor":{"row":24,"col":1,"visible":true},"alt_screen":false,
           "title":"","text":"..."},
 "output":{"text":"$ make\ncc -c foo.c\n[... 2210 lines omitted ...]\nmake: *** [Makefile:8: all] Error 2",
           "lines_total":2260,"truncated":true,"omitted_lines":2210}}
```

| Field | Meaning |
|---|---|
| `exit_code` | The program's exit status, or null if a signal ended it |
| `signal` | The signal's name, such as `"SIGSEGV"`, or null |
| `core_dumped` | Whether it dumped core |
| `killed_by_waldo` | True if the host ended it |
| `kill_reason` | `request` (a stop request), `host_signal` (the host was signalled), or null |
| `spawn_error` | Why the program could not be started, or null |
| `duration_ms` | How long the program ran |
| `screen` | The last screen, plain |
| `output` | Everything it printed (section 3.4) |

### 8.7 Destinations

| URI | How messages are delivered |
|---|---|
| `file:/PATH` | Each message is appended as one line, in one write |
| `unix:/PATH` | The host connects to the socket and writes one line per message on that connection |
| `http://HOST[:PORT]/PATH` | Each message is a `POST` with the JSON as body |

Paths must be absolute. An IPv6 host is written in brackets. `https:` is
not supported.

- `unix:` keeps one connection for the session. If a write fails the
  message is lost and the next one connects again.
- `http:` opens a connection for each message and sends `Host`,
  `User-Agent: waldo`, `Content-Type: application/json`, `Content-Length`
  and `Connection: close`. Any 2xx status means delivered. A failed message
  is tried twice more, after 250 ms and after 1 s, so a receiver can see one
  twice; `session` and `seq` identify it.
- Connecting, writing and reading time out after 5 s.

Unless `--no-probe` is given, the host delivers `hello` before starting the
program and refuses to start if that fails.

Messages wait in a queue of 256 messages or 8 MiB. When it is full, a newer
`settle` replaces a waiting one, a `stream` message merges into one before
it, and otherwise the oldest `match` or `stream`, then `settle`, is dropped.
`hello` and `exit` are never dropped. The next message that arrives carries
the number lost in `dropped`, and a gap in `seq` shows the same.

If the will cannot be delivered within 5 s, it is written to `will.json` in
the session directory, and the directory is kept.

A host that is killed outright sends no will. A `unix:` receiver sees the
connection close without an `exit` message. Others can use `--heartbeat`,
or look at the session directory (section 1.3).

### 8.8 After the program exits

1. The host reads the program's remaining output.
2. It fires triggers one last time and sends the end of the stream.
3. It delivers the will.
4. It answers every waiting request with reason `exit`.
5. With `--linger`, it keeps the socket open for that long. `GET` requests
   for status, screen, output and triggers, and `POST /v1/wait`, are
   answered, with reason `exit` where they carry one. `POST /v1/signal`
   ends the lingering. Anything else gets 409 `exited`.
6. It removes the socket and exits. After that there is nothing to connect
   to, and the will is the only record (section 1.4).

### 8.9 broadcast

Sent for every broadcast (section 4.10), whatever `--on` says:

```json
{"v":1,"session":"k3f9q2","seq":21,"trigger":"broadcast","time":"2026-11-17T23:42:08.370Z","t_ms":61250,
 "broadcast":{"n":3,"from":"ann","text":"vim is closed; the shell is yours","t_ms":61250}}
```

## 9. Errors

An error is a status of 400 or more with this body, always JSON:

```json
{"error": "unknown_key", "text": "input item 0: unknown key name <Entr> at offset 4; use <lt> for a literal <, ..."}
```

`error` is a stable code. `text` is a sentence for a person or an agent
that says what to do next; its wording may change.

| Status | Code | When |
|---|---|---|
| 400 | `bad_request` | Not a valid HTTP request, or a bad `Content-Length`. The connection is closed |
| 411 | `length_required` | A chunked request body. The connection is closed |
| 413 | `body_too_large` | A body over 1 MiB. The connection is closed |
| 431 | `headers_too_large` | Headers over 64 KiB. The connection is closed |
| 404 | `not_found` | No such endpoint |
| 405 | `method_not_allowed` | The endpoint exists but not with this method |
| 400 | `bad_json` | The body is not JSON, or not an object |
| 400 | `missing_input` | `/v1/input` without an `input` list |
| 400 | `bad_item` | An input item is not an object with exactly one known kind and a string value, or a `pause` that is not a number of milliseconds up to 10000 |
| 400 | `bad_delay` | `key_delay_ms` is not a whole number of milliseconds up to 10000 |
| 400 | `unknown_key` | A name in key notation that is not a key |
| 400 | `file_unreadable` | A `_file` item's path is not absolute, not a regular file, over 1 MiB, not UTF-8, or cannot be read |
| 400 | `unknown_until` | `until` is not one of the six conditions |
| 400 | `unknown_style` | `style` is not `plain` or `ansi` |
| 400 | `bad_timeout` | `timeout_ms` is not a whole number |
| 400 | `missing_pattern` | `until=match`, or a new trigger, without a pattern |
| 400 | `bad_pattern` | The pattern is not a valid regular expression |
| 400 | `bad_tail` | `tail` is not a number |
| 400 | `bad_size` | `cols` or `rows` missing or out of range |
| 400 | `bad_signal` | `/v1/signal` with neither `signal` nor `stop` |
| 400 | `unknown_signal` | Not a signal name |
| 400 | `unknown_mode` | A trigger `mode` that is not `edge` or `once` |
| 400 | `bad_id` | A trigger id that is not 1 to 32 allowed characters |
| 409 | `trigger_exists` | A trigger with that id is already there |
| 409 | `too_many_triggers` | The session already has 32 |
| 404 | `no_such_trigger` | Removing a trigger that does not exist |
| 400 | `bad_name` | `as` is not 1 to 32 allowed characters |
| 400 | `missing_text` | A broadcast without `text`, or with only space in it |
| 409 | `broadcasts_off` | A broadcast, a request for them, or `until: broadcast`, on a session started with `--no-broadcast` |
| 400 | `bad_text` | A broadcast's `text` is over 1000 bytes or has a control character, such as a newline |
| 400 | `upgrade_required` | `/v1/attach` without `Upgrade: waldo-attach` |
| 409 | `exited` | The program has exited and the session is lingering; only requests that read or wait are answered |

An error in an input item names the item: `input item 2: ...`. When a
request is refused, nothing it carried has been typed.

## 10. What waldo-term adds

`waldo-term` is a thin client. Everything above is done by the host;
`waldo-term` adds the following, which a replacement would need to do
itself.

- **Choosing a session.** `-s` takes an id, a unique prefix of one, or a
  socket path (anything containing `/`). With no `-s` and exactly one live
  session, that one is used.
- **Ended sessions.** For a session that has ended, or ends before
  answering, it reads the will (section 1.4) and prints it. When a
  connection fails or closes without an answer, it waits up to 3 s for the
  lock to be released before deciding the session has ended.
- **File paths.** It makes relative `--text-file`, `--keys-file` and
  `--paste-file` paths absolute before sending, and reads `-` from stdin.
- **A name.** `--as NAME`, or else the environment variable `WALDO_AS`, is
  sent as `as` by `send`, `screen`, `wait` and `broadcast`.
- **Broadcasts in text output.** Each is printed as `broadcast from NAME:
  TEXT` (or `broadcast: TEXT` for a sender without a name) on its own line
  **above** the status line. Nothing the program prints can appear there,
  so a program cannot pass its own output off as a broadcast.
- **Text output.** It asks for JSON and prints the text format of section
  3.5 itself. For a will it prints `[ID exit code=N 54.6s exited]` (or
  `signal=NAME` and so on) and then the output.
- **Exit status.** 0 when the wait condition was met; 124 for reason
  `timeout`; 3 when the program exited and that was not what was waited for,
  or had already exited; 4 for no such session, or one that died without a
  will; 2 for bad usage and for 4xx responses other than 409; 1 for
  anything else.
- **`kill`.** A stop request, then a wait for the lock to be free, at most
  10 s, then one line saying how the program ended.
- **`list` and `gc`.** Sections 1.3 and 1.5.
- **`attach`.** It puts the terminal in raw mode on the alternate screen,
  draws frames in the area above a one-line status bar, copies the three
  modes of section 7.2 to the terminal, and sends the person's keys as
  `keys_hex`. It has two modes and keeps one key for itself, Ctrl-], which
  switches between them. While interacting, every other key goes to the
  program. While viewing, none does: Ctrl-C leaves, sending `detach`, and
  `s` opens a one-line editor in the status bar. In the editor Enter sends
  the line as a broadcast, Escape or Ctrl-C gives up, and Ctrl-U clears it;
  on a session with broadcasts off there is no `s`. What other clients
  broadcast is shown in the status bar for 12 s. Ctrl-] cannot be typed into
  the program from attach; `waldo-term send --keys '<C-]>'` does it. The
  broadcast carries `--as NAME`, or `WALDO_AS`, or else the login name.
