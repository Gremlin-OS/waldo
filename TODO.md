# Waldo: open work

Everything known to be unfinished, in one place. `DESIGN.md` has the
reasoning; section numbers refer to it. Within each group the order is my
suggested priority. What was fixed is in the git history, not here.

## Behaviour

Found by driving real programs: by hand, and by the soak runs of
`soak/README.md`.

1. **Keys sent in one call can be lost when an earlier key opens a prompt.**
   In NetHack, `--keys '_>.'` left the travel prompt open for two players,
   once each. One call per key worked. Probably the program discards
   typed-ahead input when it opens a prompt, the same family as the
   key-after-text problem; not checked. Waiting for a reaction between keys
   would cost 40 ms or more per key.
2. **A screen that did not change after input looks like lost keys.** With
   NetHack's `--More--` pending, further keys were swallowed and the
   response was identical to the one before. A hint is possible without
   knowing the program: "the screen did not change after this input".
3. **A model cannot easily tell which column is which.** NetHack players
   misjudged columns. One likely cause: frames drawn with box characters
   are three bytes each, so counting bytes, or cutting the screen with
   `sed` or `awk`, is off by two per frame edge. The guide does not say
   that columns count cells from 1. A screen format with rulers is the
   other idea; measure before building it.
4. **The foreground name is missing when a pipeline's first program has
   exited.** With `seq 1 100 | less` the status line shows `running` and no
   `fg=`: waldo reads the name from the process group's leader, which is
   gone. It could take the name of another process in the group.
5. **`output` is empty while a full-screen program is up.** It leaves out
   the primary screen while the alternate screen is showing.
6. **`signal --stop` returns before the program is gone**, so a `gc` right
    after it finds the session still running, and says so. `kill` waits.
    Whether `signal --stop` should wait too was not decided.
7. **Triggers can only be added once the session is running**, so output
    before the first `match add` is missed unless it is still on screen. A
    `waldo-wrap --match ID=PATTERN` option would close that.
8. **`settle` and `stream` are chosen at startup only** (`--on`).
9. **Nested waldo**, left as it is: `hint:` lines printed by an inner
    `waldo-term` appear on the outer screen and read like the outer one's;
    `waldo-term` inside a session does not default to that session.
10. **A trigger can lag a wait for the same text.** The visible screen is
    checked for triggers at most every 25 ms, so a `send --until match`
    can return before a trigger on the same line has fired. It fires at
    the next look or at settle.
11. **Help could say more about pastes**: a trailing newline in a paste is
    an Enter (it leaves an empty last line in vim), and a block for a REPL
    that indents for you should be a `--paste`, not `--text`.
12. **The key-after-text explanation is only partly checked** (2.1).
13. **Report the emulator bug upstream**: the no-SIMD static library exports
    stub `calloc` and `free` that override a shared glibc's (9.2).

## Designed, not built

1. **PNG capture** (6.3). `GET /v1/screen?format=png`, `waldo-term
   screencap`, `format=cells`, and a vendored font. The font is not chosen.
2. **Images** (6.4). Kitty graphics, composited into captures and listed in
   text screens. Sixel by translation is an untested idea; spike it first.
3. **`--token-file`** (2.7): only if bridged use becomes common. Not planned.

## Attach, beyond what exists

1. Mirror the kitty keyboard protocol and modifyOtherKeys to the viewer's
   terminal, so programs that rely on them get the right keys from a person.
2. Scrolling back through history while attached.
3. Mouse input from the viewer.
4. Images: at least the outlined box of section 6.4.
5. A border or other cue when the viewer's window is larger than the session.

## Tested less than it should be

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
- Signals sent from htop's own kill menu: the soak runs forbid it, since
  htop shows real processes.
- The soak briefs as they now stand: they are tidied versions of the ones
  that were run, and were rewritten again when `run` was removed.
- A shell command typed into a program that is not a shell (2.5): the
  caller's lookout now. In htop it opened a process's environment view, in
  vim it edited the buffer, in `less` it started a shell.
