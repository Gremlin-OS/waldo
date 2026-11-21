htop. Two variants.

SAFETY RULE, absolute, in both: htop shows the real processes of the person
whose machine this is. Never send a signal, kill or renice anything from
htop: do not press F9, k, F7, F8, `[` or `]`. If a kill or signal menu ever
appears, press <Esc> at once.

## Variant A: htop as the session's own program

1. `W wrap --detach --name mark -- sleep 31337` (a process to look for) and
   `W wrap --detach --name top -- htop`.
2. Do each of these and check on screen that it took effect. Use `W screen
   -s top --style ansi` where plain text cannot tell you: htop marks
   selections with colour only.
   a. Sort menu (F6): choose another column, confirm, check the order and
      which header is highlighted.
   b. Filter (F4) for `31337`; check the `sleep 31337` line; clear it.
   c. Search (F3) for `sleep`; step to the next match.
   d. Tree view (F5) on and off.
   e. Setup (F2): move around, change nothing, leave with <Esc>.
   f. Help (F1), and leave.
   g. <PageDown> a few times, <End>, <Home>.
3. `W resize -s top 80x24`, then 160x45, then 120x30. For each, quote the
   status line and say whether the screen in the response was already
   redrawn at the new size.
4. htop repaints every second or two. Note how waits behave: does `send`
   return `settle` or `timeout`, how long do calls take; try `wait --until
   change`, and `wait --until match` with the filter on.
5. Batches that cross a menu boundary in ONE call, such as `--keys
   '<F6><Down><Down><Enter>'`: did every key take effect?
6. `W send -s top --keys 'q' --until exit`; then `W kill mark`, `W list`,
   `W gc --all`.

## Variant B: htop started from a shell

1. `W wrap --detach --name sh -- bash --norc --noprofile`, `W run -s sh
   'echo ready'`, `W run -s sh 'sleep 31337 & echo started'`.
2. `W send -s sh --text 'htop' --keys '<Enter>'`. Copy the status line and
   any hint. From then on note every hint with its call number.
3. One call each: filter (`--keys '<F4>' --text '31337'`, then `<Enter>`,
   then clear with `<F4><Esc>`), sort menu, tree view, help, scrolling.
4. On purpose: `W run -s sh --timeout 3000 'echo hello'` while htop is up.
   Copy the whole response; `W screen -s sh`; press <Esc> if a menu is open
   (remember the safety rule).
5. `W resize -s sh 90x25`, then 120x30: quote status lines.
6. `W send -s sh --keys 'q'`; back at the prompt, `W run -s sh 'echo back;
   kill %1'` must work.
7. `W send -s sh --text 'exit' --keys '<Enter>' --until exit`, `W list`,
   `W gc --all`.
