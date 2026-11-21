# Waldo: open work

Everything known to be unfinished, in one place. `DESIGN.md` has the
reasoning; section numbers refer to it. Within each group the order is my
suggested priority. What was fixed is in the git history, not here.

## Behaviour

Found by driving real programs: by hand, and by the soak runs of
`soak/README.md`.

1. **A `run` whose end never comes blocks every later `run`.** Two cases are
   handled: one typed into a program that is not a shell is given up at its
   timeout, and one that was interrupted ends when the shell is back. Not
   handled:
   - Text left on the shell's input line. A stray `4` turned the wrapper
     into `4{ ...`, the end mark never ran, and the `run` sat until its
     120 s timeout. `run` could clear the line first.
   - A shell whose OSC 133 marks stop after `run` has come to rely on them
     (the prompt hook is unset, or a subshell without it is started).
   - There is no command to drop a `run` by hand.
2. **`run` types before it knows whether a shell is there.** Into htop it
   opened a process's environment view; into vim it edited the buffer; into
   `less` it started a shell and tried to write a file. The hint only comes
   at the timeout. Refusing by the foreground program's name was built and
   taken out on 2026-11-21: it also refuses a shell reached through `ssh`,
   `sudo`, `docker exec` or tmux, where the local foreground program is not
   the shell, and it passes a shell that is busy. The status line already
   says what is in the foreground. If this is revisited, the shell's own
   prompt marks are real evidence of a prompt; a name is not.
3. **Keys sent in one call can be lost when an earlier key opens a prompt.**
   In NetHack, `--keys '_>.'` left the travel prompt open for two players,
   once each. One call per key worked. Probably the program discards
   typed-ahead input when it opens a prompt, the same family as the
   key-after-text problem; not checked. Waiting for a reaction between keys
   would cost 40 ms or more per key.
4. **A screen that did not change after input looks like lost keys.** With
   NetHack's `--More--` pending, further keys were swallowed and the
   response was identical to the one before. A hint is possible without
   knowing the program: "the screen did not change after this input".
5. **A model cannot easily tell which column is which.** NetHack players
   misjudged columns. One likely cause: frames drawn with box characters
   are three bytes each, so counting bytes, or cutting the screen with
   `sed` or `awk`, is off by two per frame edge. The guide does not say
   that columns count cells from 1. A screen format with rulers is the
   other idea; measure before building it.
6. **`run` prints the command's output, not the screen**, so after a `run`
   timeout the agent needs a second call to see what happened.
7. **A shared session's `run` output can hold the other client's typing.**
   The terminal's echo of it lands in the output. The status line says
   `interleaved`; the output is not cleaned.
8. **The foreground name is missing when a pipeline's first program has
   exited.** With `seq 1 100 | less` the status line shows `running` and no
   `fg=`: waldo reads the name from the process group's leader, which is
   gone. It could take the name of another process in the group.
9. **`output` is empty while a full-screen program is up.** It leaves out
   the primary screen while the alternate screen is showing.
10. **The `run` wrapper stays on screen**: `{ cmd` / `}; printf
    '\033]7770;waldo;...'`. Shell integration (OSC 133) avoids it.
11. **`signal --stop` returns before the program is gone**, so a `gc` right
    after it finds the session still running, and says so. `kill` waits.
    Whether `signal --stop` should wait too was not decided.
12. **Triggers can only be added once the session is running**, so output
    before the first `match add` is missed unless it is still on screen. A
    `waldo-wrap --match ID=PATTERN` option would close that.
13. **`settle` and `stream` are chosen at startup only** (`--on`).
14. **Nested waldo**, left as it is: each layer reports its own exit status
    (a failure below shows as 5 above); `hint:` lines printed by an inner
    `waldo-term` appear on the outer screen and read like the outer one's;
    `waldo-term` inside a session does not default to that session.
15. **Help could say more about pastes**: a trailing newline in a paste is
    an Enter (it leaves an empty last line in vim), and a block for a REPL
    that indents for you should be a `--paste`, not `--text`.
16. **The key-after-text explanation is only partly checked** (2.1).
17. **Report the emulator bug upstream**: the no-SIMD static library exports
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
- Signals sent from htop's own kill menu: the soak runs forbid it, since
  htop shows real processes.
- The soak briefs as they now stand: they are tidied versions of the ones
  that were run.
