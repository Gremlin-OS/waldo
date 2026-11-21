NetHack. Two variants; give half the agents each. `{NAME}` is the agent's
player name.

## Variant A: NetHack as the session's own program

1. `W wrap --detach --name game -- nethack -u {NAME}`
2. Play. Goal: get as deep in the dungeon as you can. Let the game pick your
   character. Batch several moves into one `send --keys` call.
3. Along the way use each of these a few times and check the result makes
   sense: several keys in one call, `send --text`, `wait --until match
   --pattern ...`, `screen`, `screen --style ansi`, a `match add` trigger
   for text you expect to appear (then `match list`), and `resize` once to
   100x30 and back.
4. When the budget is nearly used, or you die: `W kill game` if it is still
   going, then `W list` and `W gc --all`. Note what they print.

Also report: how far you got and how it ended; any batch of keys where a
later key seemed lost (for example after a key that opens a prompt); any
time the screen came back unchanged after input.

## Variant B: NetHack started from a shell

1. `W wrap --detach --name sh -- bash --norc --noprofile`, and IMMEDIATELY,
   with no wait in between, `W run -s sh 'echo first; uname -s'`. Note what
   it printed and whether any line on the screen is there twice.
2. Two or three more runs: one that fails (`ls /nonexistent`), one with many
   lines (`seq 1 40`). Check output and exit status.
3. `W send -s sh --text 'nethack -u {NAME}' --keys '<Enter>'`. Copy any
   `hint:` line. From then on, note the call number and text of every hint.
4. Play, as in variant A.
5. Once, mid-game, on purpose: `W run -s sh --timeout 4000 'echo hello'`.
   This types a command line into NetHack and should time out. Copy the
   whole response. Look at the screen, clean up (Escape usually helps),
   carry on.
6. With about 10 calls left: quit the game back to the shell prompt
   (`#quit`, answer its questions). Then `W run -s sh 'echo back'` must
   work. Then `W send -s sh --text 'exit' --keys '<Enter>' --until exit`,
   `W list`, `W gc --all`.
