waldo inside waldo. Two variants.

Tell the agent: INSIDE waldo sessions the plain commands `waldo-wrap` and
`waldo-term` are on the PATH and are what you type there. You may type
(`W send`) ONLY into the top session; everything lower must be reached by
typing `waldo-term` commands into the top one. Read-only W calls on lower
sessions (`W screen -s l2`, `W list`) are allowed, to check what really
happened. Shell quoting gets deep: say where it cost you. A `waldo-term`
typed into a session prints the lower session's status line and screen onto
that session's screen, so you will see status lines nested.

## Variant A: a tower

1. `W wrap --detach --name l1 --size 130x36 -- bash --norc --noprofile`.
2. In l1's shell start l2 (`waldo-wrap --detach --name l2 --size 110x30 --
   bash --norc --noprofile`). Then, by typing a `waldo-term send -s l2 ...`
   into l1, have l2 start l3 (90x24). Then type `echo deep; echo
   $WALDO_SESSION` at the bottom through both layers, with a marker at each
   level you need one. Quote the nested status lines.
3. Run a failing command at the bottom through both layers, with a marker
   at the bottom. Can you read its exit status from the top?
4. `sleep 5` at the bottom with a marker and `--timeout 1000` on the
   innermost wait. What does each layer report? Then make sure the bottom
   is usable again.
5. Attach chain: l2 to l3, then l1 to l2, both with `--drive`. Describe the
   l1 screen: how many status bars, what sizes. Type into l1; verify with
   `W screen -s l3` that it arrived at the bottom.
6. A full-screen program through two attach layers: `vim --clean deep.txt`,
   two lines, `<Esc>:wq<Enter>`; check the file by typing `cat -A deep.txt`
   through the layers.
7. `W resize -s l1 100x26` and back. What do the inner views show?
8. Leave from the top down: `<C-]><C-c>` into l1 (Ctrl-] stops typing,
   Ctrl-C leaves the viewer), then a `waldo-term send -s l2 --keys
   '<C-]><C-c>'` typed into l1.
9. Pull the floor out: re-attach both, then type `exit` through the layers
   so l3's shell exits. What does each layer show afterwards?
10. In l1's shell, `waldo-term kill l2`; then `W kill l1`, `W list`, `W gc
    --all`.

## Variant B: a fan

1. `W wrap --detach --name boss -- bash --norc --noprofile`.
2. In boss's shell start workers w1, w2, w3, one call each; then
   `waldo-term list`.
3. One job per worker, by typing `waldo-term send -s wN --text '...; echo
   "rc=$?"' --keys '<Enter>' --until-match '^rc='` into boss: one
   that writes and reads a file, one with several lines of output, one that
   fails. Copy the whole response each time.
4. A slow job with `--timeout 1000` on the inner wait, then `waldo-term wait
   -s w1 --until-match '^rc='` typed into boss.
5. All three at once: `for w in w1 w2 w3; do waldo-term send -s $w --text
   "echo hello from $w" --keys '<Enter>' & done; wait`. Is each answer
   whole?
6. On purpose: a session that does not exist, and an unknown key name.
7. Kill one worker from boss's shell, then send to it.
8. Clean up from boss's shell, then `W kill boss`, `W list`, `W gc --all`.
