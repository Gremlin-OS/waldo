Every brief starts with this text, then the scenario's own. Replace `{W}`
with the agent's wrapper path.

---

You are stress-testing "waldo", a tool that lets an agent drive interactive
terminal programs. The program you drive is only the workload. What I want
back is a report on how waldo behaved.

Setup. Your wrapper script is:
{W}
Call it W. `W ARGS...` runs `waldo-term ARGS...`; `W wrap ARGS...` runs
`waldo-wrap ARGS...`. From your own shell use only W: never call waldo-term,
waldo-wrap or the program under test directly, and do not read or edit files
under the arena directory by other means. Read `W help`, `W help keys`,
`W help wait` and `W help shell` first, quickly. Options go before positional
arguments (`W resize -s NAME --until-none 100x30`).

Where a brief says to run a command "in the shell", put a marker after it
and wait for the marker, as `W help shell` explains:
`W send -s NAME --text 'CMD; echo "rc=$?"' --keys '<Enter>' --until-match
--until-match '^rc=[0-9]+$'`. The marker line carries the exit status.

Budget: W counts every `send` CALL, and refuses past
the limit it prints. One call may carry several keys. screen, wait, resize,
match, output, help and list are not counted, but do not spam them.

Report back, in plain text, under these headings:
- Results: what happened at each step, with status lines and `hint:` lines
  quoted exactly where they matter.
- Waldo problems: anything waldo did that was wrong, surprising, slow or
  unhelpful. For each: the exact W command, what came back, what you
  expected, and whether it repeated when you tried again.
- Friction: correct behaviour that cost extra calls or confused you.
- Things that worked notably well.
Separate the program's own behaviour, and your own typos, from waldo's. Only
report what you actually saw. Do not pipe W's output through head or tail
when the status line or a hint matters: you will lose them.

Do the work yourself: do not start other agents or hand any part of it on.
