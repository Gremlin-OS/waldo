waldo inside waldo. Two variants.

Tell the agent: INSIDE waldo sessions the plain commands `waldo-wrap` and
`waldo-term` are on the PATH and are what you type there. For anything that
types (send, run) you may address ONLY the top session with W; everything
lower must be reached by typing commands into the top one. Read-only W calls
on lower sessions (`W screen -s l2`, `W list`) are allowed, to check what
really happened. Shell quoting gets deep: say where it cost you.

## Variant A: a tower

1. `W wrap --detach --name l1 --size 130x36 -- bash --norc --noprofile`.
2. From inside l1 start l2 (110x30); from inside l1 tell l2 to start l3
   (90x24). Run `echo deep; echo $WALDO_SESSION` at the bottom through both
   layers. Quote the nested status lines.
3. Run a failing command at the bottom through both layers. What exit
   status does each layer report, and what does W exit with?
4. `sleep 5` at the bottom with `--timeout 1000` on the innermost run. What
   does each layer report? Then make the bottom usable again.
5. Attach chain: l2 to l3, then l1 to l2, both with `--drive`. Describe the
   l1 screen: how many status bars, what sizes. Type into l1; verify with
   `W screen -s l3` that it arrived at the bottom.
6. A full-screen program through two attach layers: `vim --clean deep.txt`,
   two lines, `<Esc>:wq<Enter>`; check the file by typing `cat -A deep.txt`
   through the layers.
7. `W resize -s l1 100x26` and back. What do the inner views show?
8. Detach from the top down: `<C-]>` into l1, then a `waldo-term send -s l2
   --keys '<C-]>'` typed into l1.
9. Pull the floor out: re-attach both, then type `exit` through the layers
   so l3's shell exits. What does each layer show afterwards?
10. From inside l1, `waldo-term kill l2`; then `W kill l1`, `W list`, `W gc
    --all`.

## Variant B: a fan

1. `W wrap --detach --name boss -- bash --norc --noprofile`.
2. Start workers w1, w2, w3 from inside boss, one call each; `waldo-term
   list` from inside boss.
3. One job per worker, through boss: one that writes and reads a file, one
   with several lines of output, one that fails. Copy the whole response
   each time; for the failure note the exit status at each level.
4. A slow job with `--timeout 1000` through boss, then `waldo-term wait -s
   w1 --until done` through boss.
5. All three at once: `for w in w1 w2 w3; do waldo-term run -s $w 'echo
   hello from '$w & done; wait`. Is each answer whole?
6. `waldo-term send` to a worker through boss.
7. On purpose: a session that does not exist, and an unknown key name.
8. Kill one worker from inside boss, then run a command on it.
9. Clean up from inside boss, then `W kill boss`, `W list`, `W gc --all`.
