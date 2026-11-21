vim. Every task has an exact expected file. Always start vim as `vim
--clean` (plain `vim -u NONE` mishandles pastes). Check a file with `cat -A
FILE; wc -l FILE` (tabs show as ^I, line ends as $). A paste that ends in a
newline leaves an empty last line in vim: that is vim, so do not end a paste
with one unless you want it.

## Variant A: vim started from a shell (the longer one)

One session: `W wrap --detach --name sh -- bash --norc --noprofile`. Start
vim with `W send -s sh --text 'vim --clean FILE' --keys '<Enter>'`; check
results with `W run -s sh 'cat -A FILE'` after each `:wq`.

1. a.txt, one call: `--keys 'i' --text $'alpha\nbeta gamma\ndelta' --keys
   '<Esc>:wq<Enter>'`. Does `<Esc>` followed at once by `:wq<Enter>` work?
2. b.txt: `--paste` a block with leading spaces, a tab, a line of shell
   special characters (`$HOME "quotes" 'single' \backslash`), an empty
   line, and non-ASCII text (`naïve café — 日本語 ✓`). Did indentation
   "staircase"? Was anything altered?
3. c.txt: 40 lines by `--paste-file`. Then `20G`, `5dd`, undo, redo,
   `:%s/foo/bar/g`, yank a line and put it at the end.
4. Visual block: `<C-v>jjjI# <Esc>` on a 6-line file; exactly 4 lines get
   the prefix.
5. A macro: `qa` ... `q` appending ` ;` and moving down; `5@a`.
6. `:split`, `:vsplit`, `<C-w>` moves, `:only`, `:help`, `:q`.
7. `/` search, `n`, and a `send --until match` on a message the search
   makes appear.
8. `W resize -s sh 80x20` and back; quote the status lines; is vim's bottom
   line on the last row in each response?
9. On purpose: (a) `W run -s sh --timeout 3000 'echo hello'` with vim in
   normal mode; copy the response; repair with `<Esc>:q!<Enter>`; then `W
   run -s sh 'echo back'` must work. (b) `--text '<Esc>'` in insert mode;
   copy the hint. (c) Start `W run -s sh --timeout 2000 'sleep 30'`, press
   `<C-c>`, then `W run -s sh 'echo after'` must work.
10. A single line of 300 characters; `wc -c` must agree.
11. `W send -s sh --text 'exit' --keys '<Enter>' --until exit`, `W list`,
    `W gc --all`.

## Variant B: vim as the session's own program (the shorter one)

A shell session `sh` for checking, and one vim session per file.

1. `W wrap --detach --name v1 -- vim --clean one.txt`, then IMMEDIATELY `W
   send -s v1 --keys 'i' --text 'first line' --keys '<Esc>:wq<Enter>'
   --until exit`. Expected: one line.
2. two.txt: insert `one`, `two`, `three`; `ggdd`; `Gp`; `:wq`. Expected:
   two, three, one.
3. three.txt: `i`, then `--paste $'def f():\n    if x:\n        return
   1\n\treturn 2'`, then `<Esc>:wq<Enter>`. Expected: those 4 lines exactly.
4. four.txt: three lines of `aaa foo bbb foo`; `:%s/foo/bar/g`.
5. five.txt: insert `hello`, then `--text '<Esc>:wq'` on purpose; copy the
   hint; repair; save.
6. six.txt: `:set number`, three lines, `W resize -s v6 60x15` and back;
   quote both status lines.
7. After v6 has ended: `W screen -s v6` and `W send -s v6 --keys 'i'`; copy
   what each prints and its exit status.
8. `W kill sh`, `W list`, `W gc --all`.
