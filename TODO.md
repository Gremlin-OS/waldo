# Waldo: open work

Everything known to be unfinished as of 2026-11-20, in one place. `DESIGN.md`
has the reasoning; section numbers below refer to it. Within each group the
order is my suggested priority.

## Found by using it

These came from driving real programs (top, htop, vim, Claude Code, NetHack),
not from the design.

1. **The settle hint is noise for interactive programs.** Every screen from a
   program started inside a shell carries "'nethack' is still running; settle
   only means the screen went quiet." It is true and useless when that
   program is the thing being driven. Likely fix: show it only when the
   foreground program has changed since the request's input was sent, or
   only once per foreground program.
2. **A model cannot easily tell which square is which.** Both subagents that
   played NetHack misjudged columns. Options: a screen format with row and
   column rulers, or coordinates relative to the cursor. Part of the trouble
   was the test's own helper script, which trimmed the screen; check how
   much remains with the untrimmed text format before building anything.
3. **`gc` straight after `signal --stop` removed nothing.** Seen once: the
   session was reported ended, then `gc --all` printed "removed 0". Probably
   the host still held its lock for an instant. `kill` waits for the lock;
   `wait --until exit` does not. Reproduce, then decide whether `gc` should
   wait briefly or `wait --until exit` should.
4. **Input sent before a shell draws its first prompt is echoed twice**, once
   by the terminal and once by the shell. Harmless, and `run` strips both
   from its output, but it is confusing on screen. `run` could wait for the
   first settle before typing.
5. **The key-after-text test is weak.** It could pass by luck without the
   fix. The real evidence was four manual runs against Claude Code. Find a
   program from nixpkgs that detects pastes by timing, or write a small one.
6. **`run` on something that is not a shell** types the command line into it
   and times out. A hint on that timeout would help ("the foreground program
   is 'vim', not a shell").

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
   forces it.
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

## Housekeeping

- CI (`.github/workflows/ci.yml` runs `nix flake check`, which includes
  clippy with warnings as errors) has never run: it was written before
  being pushed. Check the first run.

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
