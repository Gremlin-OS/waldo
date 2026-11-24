# Waldo: open work

Everything known to be unfinished, in one place. `DESIGN.md` has the
reasoning; section numbers refer to it. Within each group the order is my
suggested priority. What was fixed is in the git history, not here.

## Behaviour

Found by driving real programs: by hand, and by the soak runs of
`soak/README.md`.

1. **Keys typed past a prompt, or past the keys that end a program, are
   lost**, and the host can only say so afterwards (DESIGN.md 2.1, "Where
   typed keys go missing"). Two ideas, neither tried:
   - The pty's packet mode reports when a program flushes the terminal's
     input, as vim does on exit. That would turn "may have been lost" into
     "was discarded" for part of the vim case.
   - The NetHack soak runs should be read again for what the lost keys
     followed. The tracing used NetHack 5.0.0; which version and interface
     the soak runs used was not recorded.
2. **`unchanged` is unproven.** It appeared on about 1 send in 50 in
   NetHack and no agent repeated its input after it, but that is too few
   cases to say it helps.
3. **Settle can fire before a program that was just started has drawn.**
   On a slow machine `vim FILE` and Enter settled on the echoed command
   line; the next send was typed at the shell's terminal, vim read it as
   typed-ahead, and what followed was lost. This did not reproduce on a
   fast machine with `sleep 1; vim FILE`: vim got every key, and they were
   only echoed onto the screen first. What was lost on the slow one is
   not known. Found by the first CI run on
   Ubuntu, in the guide's own example, which now waits for text. The host
   knows a fact it does not use here: the foreground program changed
   during the request and has printed nothing yet.
4. **A model cannot easily tell which column is which.** NetHack players
   misjudged columns. One likely cause: frames drawn with box characters
   are three bytes each, so counting bytes, or cutting the screen with
   `sed` or `awk`, is off by two per frame edge. The guide does not say
   that columns count cells from 1. A screen format with rulers is the
   other idea; measure before building it.
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
    checked for triggers at most every 25 ms, so a `send --until-match`
    can return before a trigger on the same line has fired. It fires at
    the next look or at settle.
11. **Help does not say that a trailing newline in a paste is an Enter**
    (it leaves an empty last line in vim).
12. **The key-after-text explanation is only partly checked** (2.1).
13. **Report the emulator bug upstream**: the no-SIMD static library exports
    stub `calloc` and `free` that override a shared glibc's (9.2).

## From the comparison with tmux (2026-11-21)

Six rounds, the first three against plain tmux, Sonnet and Haiku, on
scripted tasks, htop, NetHack and five small programs. Every checkable
answer was right with both tools. Waldo did better where pasting or
timing mattered and at first cost 10 to 17% more tokens, mostly in waits
that ran to their timeout. `help tips` and an `--until-exit` that means
the program being typed into took timeouts from 16 to 1 over four agents
and brought the cost about level with tmux. NetHack depth swung from 1 to
6 with the same tool and shows nothing either way.

- The comparison is not in the repository: the tmux wrapper and the
  three task lists exist only in scratch files. `soak/` should get a
  `versus-tmux` brief if it is to be repeated.
- Nothing measured what tmux does not have: the API for a harness, the
  exit message, triggers, and input kept whole between clients.

## Designed, not built

0. **Saying who arrives and leaves.** Nothing tells the clients of a
   session that a person has attached or left, so an agent that asks for
   help cannot tell whether anyone is there to hear. The design: the host
   adds its own entry to the broadcast stream, "chris attached" and
   "chris left", marked so that a client cannot make one. That gives a
   record at the destination and tells named agents on their next request,
   with no new endpoint or wait. Attach would send its name when it
   connects. Not built: nobody has needed it yet. A lease that lets one
   client hold the keyboard across several requests was considered and
   set aside for the same reason; broadcasts were enough for two agents to
   take turns.

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
