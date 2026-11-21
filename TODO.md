# Waldo: open work

Everything known to be unfinished as of 2026-11-20, in one place. `DESIGN.md`
has the reasoning; section numbers below refer to it. Within each group the
order is my suggested priority.

## Found by using it

These came from driving real programs (top, htop, vim, Claude Code, NetHack),
not from the design.

1. **A model cannot easily tell which square is which.** Both subagents that
   played NetHack misjudged columns. Options: a screen format with row and
   column rulers, or coordinates relative to the cursor. Part of the trouble
   was the test's own helper script, which trimmed the screen; check how
   much remains with the untrimmed text format before building anything.
   The 2026-11-21 stress test adds a likely cause: NetHack 5 draws frames
   with box characters, which are three bytes each, so an agent that counts
   bytes (or cuts the screen with `sed` or `awk`) is off by two per frame
   edge. One player saw exactly that offset against `cursor=`. The guide
   does not say that columns count characters from 1.
2. **`signal --stop` returns before the program is gone**, so a `gc` right
   after it finds the session still running. `gc` now says so, and `kill`
   waits. Whether `signal --stop` should also wait was not decided.
3. **The key-after-text explanation is only partly checked.** Claude Code's
   input box was read (DESIGN.md section 13), but not the place where a
   short text and an Enter in one burst become a line break.

## Found by the stress tests of 2026-11-21

Sixteen subagents (eight Sonnet, eight Haiku) drove NetHack 5.0.0, htop
3.5.3, vim 9.2, one shared tmux 3.8 session, one shared waldo session, and
waldo sessions started from inside waldo sessions: 836 logged calls in
all. A scripted hammer (four clients, one session) found the worst bug:
concurrent `run`s were answered with each other's output. Signals and
resizes from another client could land in the middle of a request's
input; they now go through the input queue (2.1). Fixed the same day:
that,
`resize` answering before the redraw, a `run` typed into a program
blocking later runs, a `run` stopped with Ctrl-C blocking later runs, two
timeout hints, and a hint when the program being typed into leaves the
foreground. Still open:

1. **Keys sent in one call can be lost when an earlier key opens a
   prompt.** `--keys '_>.'` (travel, pick the stairs, confirm) left the
   travel prompt open for two players, once each; one of them had it work
   on a later level. Sending the keys one call at a time worked. Probably
   NetHack discards typed-ahead input when it opens a prompt, which is
   the same family as the key-after-text problem, but that was not
   checked. A wait for the program's reaction between keys would cost
   40 ms or more per key.
2. **A screen that did not change after input looks like lost keys.** With
   a `--More--` pending, further keys were swallowed and the response was
   identical to the one before. Three players asked for a hint. A general
   one is possible without knowing the program: "the screen did not change
   after this input".
3. **`run` prints the command's output, not the screen**, so after a `run`
   timeout the agent needs a second call to see what happened.
4. **`run` types before it knows whether a shell is there.** Into htop,
   `echo hello` opened a process's environment view; into vim it edited
   the buffer. The hint only comes at the timeout. `run` could refuse at
   once when the foreground program is not the session's own and not a
   known shell, at the price of refusing an unlisted shell started from
   another one.
5. **The `run` wrapper is on screen.** Without the shell's own marks, the
   typed line `{ cmd` / `}; printf '\033]7770;waldo;...'` stays visible.
   Three players called it noise. Shell integration (OSC 133) avoids it.
6. **A paste that ends in a newline leaves an empty last line in vim.**
   That is vim's doing, and correct, but both vim players tripped on it;
   `help` could say that a trailing newline in a paste is an Enter.
7. **A shared session's `run` output can hold the other client's typing.**
   While one agent's `run` was going, the other typed a command; the
   terminal's echo of it landed in the first one's output. The status
   line now says `interleaved`, but the output is not cleaned.
   Nested waldos (three deep by `run`, and two chained attaches with vim
   at the bottom) showed no fault. Left as they are: each layer reports
   its own exit status (a failure below shows as 5 above), `hint:` lines
   printed by an inner waldo-term appear on the outer screen and read
   like the outer one's, and `waldo-term` inside a session does not
   default to that session.
8. **Text left on the shell's input line breaks `run`.** A stray `4` typed
   into bash turned the wrapper into `4{ ...`, the end mark never ran,
   and the `run` sat until its 120 s timeout. `run` could clear the line
   first.
9. **`output` is empty while a full-screen program is up.** It leaves out
   the primary screen while the alternate screen is showing, so lines
   that have not scrolled off cannot be read until the program exits.

## Designed, not built

1. **PNG capture** (6.3). `GET /v1/screen?format=png`, `waldo-term
   screencap`, `format=cells`, and a vendored font. The font is not chosen.
2. **Images** (6.4). Kitty graphics, composited into captures and listed in
   text screens. Sixel by translation is an untested idea; spike it first.
3. **`--token-file`** (2.7): only if bridged use becomes common. Not planned.

## Follow-ups from the 2026-11-20 work

1. **Report the emulator bug upstream.** The no-SIMD static library exports
   stub `calloc` and `free` that override a shared glibc's (9.2).
2. **A `run` that relies on the shell's marks has no way back.** Once the
   shell has reported a command's end with OSC 133, `run` stops typing its
   sentinel. If the marks then stop (the prompt hook is unset, or a subshell
   without it is started), that `run` waits until its deadline. Options:
   fall back to the sentinel after such a timeout, or a request field that
   forces it. More generally, a `run` whose end never comes blocks every
   later `run` with `run_in_progress`. Two cases are handled: typing into
   a program that is not a shell is given up at its timeout, and a run
   that was interrupted ends when the shell is back. For any other case
   there is no command to drop a `run` by hand.
3. **Triggers can only be added once the session is running**, so output
   before the first `match add` is missed unless it is still on screen. A
   `waldo-wrap --match ID=PATTERN` option would close that.
4. **`settle` and `stream` are chosen at startup only** (`--on`). Turning
   them on and off through the API was not asked for; note it if it is.

## Attach, beyond what exists

1. Mirror the kitty keyboard protocol and modifyOtherKeys to the viewer's
   terminal, so programs that rely on them get the right keys from a person.
2. Scrolling back through history while attached.
3. Mouse input from the viewer.
4. Images: at least the outlined box of section 6.4.
5. A border or other cue when the viewer's window is larger than the session.

## Tested less than it should be

- `run --shell fish`: implemented, never run.
- aarch64: declared in the flake, never built.
- Attach: holding frames during a synchronized update, and marking an
  agent's request `interleaved` when a person types during it. Both are in
  the code with no test.
- Attach detach keys were exercised through a waldo session and once by a
  person typing; colour and redraw quality in a real terminal were never
  reported back.
- The core-dump flag: the test only checks that it is present.
- Whether the default size of 120x30 is right: judgement, no token counts.
- The message queue when full: unit tested only. No test makes a real
  receiver slow enough to fill it.
- `http:` retries and `unix:` reconnecting after a failed write: no test.
- A recording was checked as JSON lines, never played with `asciinema`.

## Driving waldo with subagents

Not waldo's code, but learned the same day.

- A step-by-step task (a game, a long menu flow) goes to a Sonnet subagent
  with a short brief, a helper script and a budget. It was far faster than
  doing each step in the main session.
- State budgets in the unit meant. "About 80 key-sends" was read as 80
  calls and became 110 keys. Better: have the helper script count and
  refuse past the limit.
- The helper script should print the screen as waldo gives it. Trimming it
  made columns harder to judge.
