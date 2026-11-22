Four agents attached to ONE tmux session, each through its own waldo
session. Arena: `--shared-home`, and tmux on the PATH. Give the agents
different sizes (`{SIZE}`: for example 120x30, 100x28, 90x24, 120x30) and
names (`{NAME}`: red, blue, green, gold). One agent is the host.

Put a helper `tm` on the PATH that runs tmux on one shared socket with no
user configuration: `exec tmux -f /dev/null -S SOCKET "$@"`. The socket path
must be short (under about 100 characters).

Tell every agent: tmux clients attached to the same session share the
current window and the input focus. Whatever any of you types goes to the
same place, and when one of you switches window, everyone's view switches.
Your screen will change for reasons that are not your own input, and your
typing can collide with the others'. That is the test. Say clearly in your
report when something was tmux's shared focus and not waldo.

## Every agent

1. `W wrap --detach --name t --size {SIZE} -- bash --norc --noprofile`.
   The host then types `tm new -s arena`. The others wait until `W send -s t
   --text 'tm has-session -t arena && echo up' --keys '<Enter>'` shows a
   line `up` (retry every 5 s, at most 6 times), then type `tm attach -t
   arena`.
2. `W match add -s t --id fin '^ALL DONE'`.
3. Roll call: type `echo {NAME} here`, then `W wait -s t --until-match
   --until-match '^OTHER here' --timeout 90000` for each of the others. Quote
   any garbled line exactly.
4. Type `tm new-window -n {NAME}`. The status bar marks the current window
   with `*`. Append five lines to {NAME}.txt, ONE SEND CALL PER LINE (`echo
   "{NAME} 1" >> {NAME}.txt` ...). Before each, check the status bar in
   your latest screen; if you are not in your own window, switch back with
   `tm select-window -t {NAME}`. Count how often you were moved, and by
   whom.
5. Type `cat {NAME}.txt; wc -l *.txt` and quote it.
6. Type `echo {NAME} finished` in window 0.

## The end, which must not depend on one agent's pace

- The others: `W wait -s t --until-match '^ALL DONE' --timeout
  300000`, then `W match list -s t`, detach with `--keys '<C-b>d'`, run
  `echo detached` in the shell, exit the shell with `--until-exit`, `W list`, `W
  gc --all`. Then type nothing more.
- The host: do NOT end the session on a timer. Wait until all three
  `NAME finished` lines have appeared (`W wait --until-match` for each,
  with a 5 minute timeout), then type `echo ALL DONE`, wait 30 s, detach,
  run `tm kill-server; echo cleaned` in the shell, exit, `W list`, `W gc
  --all`. If a `finished` line never comes, say so and end anyway.

After the run, check the four files in the shared work directory yourself.
