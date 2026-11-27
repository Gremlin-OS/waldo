# Waldo: dead ends

Things that were tried and dropped, and what the trial showed, so that
nobody tries them again without a new reason. `TODO.md` has what is still
open.

## Help that steers agents away from `--until-match`

**The worry.** Agents end most sends with `--until-match`, and now and then
give a pattern that their own input satisfies: the echo of the command, or
text they pasted into an editor. The wait is then answered before the
program has acted on the rest of the input. One agent read a vim screen
that still said `-- INSERT --` and reported that its Escape had been lost.
It had not.

**What was tried.** A rewrite of help, on the theory that its examples
taught the habit:

- the marker after a shell command (`; echo "rc=$?"`, then
  `--until-match '^rc=[0-9]+$'`) left only in the `shell` section, and
  taken out of the guide, `wait` and `sharing`;
- the first example of the guide made a plain `send` with the default wait;
- tip 2 reworded to say what a pattern wait is for (output you are waiting
  on: a line in a log, a prompt, a marker), and that the pattern must not
  match what you type.

**What it showed.** Four agents on Haiku with each help, 14 tasks each (vim
edits, a Python block, slow shell commands, less, a password prompt, a
script printing a log), scored from the files and the wrapper logs:

| | help as it was | rewrite |
|---|---|---|
| sends at a shell prompt that waited for a pattern | 84 of 105 | 86 of 102 |
| sends typed into vim that waited for a pattern | 0 of 52 | 1 of 59 |
| agents whose pattern matched their own echo | 2 of 4 | 2 of 4 |
| files correct, of 7 | 7, 7, 7, 7 | 7, 7, 7, 7 |
| sends used | 45, 53, 42, 44 | 41, 49, 50, 49 |

Nothing moved. The rewrite was not merged.

**Why, as far as we can tell.** Agents do not learn the marker from waldo.
They arrive with it: a shell prompt returns nothing, so they add an `echo`
to have a sign that the command ended. Help that stops showing the habit
does not remove it. The misuse in vim that started this was one agent in
four in one run, and did not recur with either help.

**If it comes up again.** Wording is not the lever. See the next entry for
what the host can say about it.

## A hint when a pattern matched the request's own command

**What was tried.** The host added a hint to a pattern wait that ended on
the line the cursor was on when the request began to type, when that line
held the typed text: "the pattern matched the line this request typed on:
'$ vim --clean a.txt'. The wait ended on your own input, before the
program answered it."

A first version asked only that the matched line hold the typed text. It
misreported a program that prints its input back: a password prompt
followed by `got=hunter2`. Requiring the line typed on fixed that, and
left out text pasted into an editor, for which no rule was found that
never misreports.

**What it showed.** Four agents on Haiku, six tasks. The hint fired twice,
both times rightly, on `--until-match 'a\.txt'` when starting vim, and
both agents then looked before typing. Without the hint, three of four
agents who made the same mistake noticed it from the screen, and the
fourth typed on and got away with it. Results, sends and tokens were the
same with and without. It was not merged.

**If it comes up again.** It was never wrong in the trial, only unproven:
the tasks let an agent recover from typing too early. A trial that could
show a difference needs a task where that breaks something, such as a
slow program that is typed into as soon as it is started.

## Naming `waldo-term wait` in the error for an empty `send`

**What was tried.** `waldo-term send` with a wait and nothing to type is
refused. The error was given a last sentence: "To wait without typing,
use: waldo-term wait".

**What it showed.** Agents who hit the error used `wait` on their next
call with the sentence and without it. The call that is lost is the one
that got the error. It was not merged.
