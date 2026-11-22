# Soak tests

Tests that are too slow, too costly or too open-ended for `nix flake check`.
`TODO.md` has what they found and left open.

There are two kinds.

## 1. The scripted hammer

Four clients on one session, 50 sends each. Every line must be typed whole.

```sh
nix develop -c cargo test --test integration -- --test-threads=1 --ignored soak
```

It takes about half a minute. A smaller cut of it runs in the normal suite.

## 2. Agent runs

Subagents drive real programs through waldo and report how waldo behaved.
They find what scripted tests do not: confusing hints, missing hints, and
behaviour nobody thought to write a test for. They cost tokens and they are
not repeatable line for line, so they are run by hand.

### How to run one

1. Build waldo and fetch the programs the scenario needs:

   ```sh
   nix build
   nix build nixpkgs#nethack nixpkgs#htop nixpkgs#vim nixpkgs#tmux nixpkgs#sl --no-link --print-out-paths
   ```

2. Make an arena. It prints one wrapper script per agent:

   ```sh
   soak/arena.sh --path /nix/store/...-nethack/bin --budget 120 s1 s2 h1 h2
   ```

   `--shared` gives all agents one session directory (for the shared-session
   scenario); `--shared-home` gives them one working directory (for tmux).

3. Start one subagent per wrapper, in the background. Its brief is
   `soak/briefs/_common.md` followed by the scenario's own file. Replace
   `{W}` with the agent's wrapper path and fill in the other `{...}`
   fields. The runs so far used
   Sonnet and Haiku, half each: Haiku makes more mistakes, which is useful.

4. When they are done, check their reports against the logs:

   ```sh
   soak/summary.sh /tmp/waldo-soak.XXXXXX
   ```

   Treat a report as a lead, not a fact. Several reported problems were the
   program's own behaviour, or the agent cutting its own output short.
   Reproduce a suspected bug by hand before fixing it, and give each fix a
   regression test in `tests/integration.rs`.

### Rules that the briefs rely on

- The agent uses only its wrapper, `W`. Every call is logged, and calls that
  type (`send`) are counted and refused past the budget. The budget
  is in calls, and the wrapper enforces it; a budget stated only in words
  gets overrun.
- The wrapper prints the screen exactly as waldo gives it.
- Nothing is deleted. Each run has a fresh arena.
- **htop shows the real processes of whoever runs the test.** The htop
  briefs forbid its kill and renice keys. Do not remove that rule.
- A scenario with a shared resource needs an explicit end condition that
  does not depend on one agent's pace. In the first tmux run the host shut
  the server down while two agents were still working.

### Scenarios

| Brief | Agents | Arena options | What it exercises |
|---|---|---|---|
| `nethack.md` | 2 to 4 | `--budget 120` | Long key-by-key driving, prompts that eat keys, waits, triggers |
| `htop.md` | 2 | `--budget 80` | Menus, colour-only selection, a screen that repaints, resize |
| `vim.md` | 2 | `--budget 80` | Exact file contents: text and keys in one call, pastes, modes |
| `tmux-shared.md` | 4 | `--shared-home --budget 60` | Screens that change for reasons other than your input |
| `shared-session.md` | 2 | `--shared --budget 60` | Two clients typing into one waldo session |
| `nested.md` | 1 or 2 | `--budget 60` | waldo sessions started from, and attached to, inside waldo sessions |

Programs tried by hand and not given a brief:

- `sl`: an animation that ignores Ctrl-C. `send` returns when the train
  has gone.
- `less`: paging, search and quitting work. A shell command line typed into
  it by mistake started a shell and tried to write a file. Look at `fg=`
  before typing a command.
- `fzf`: a query, a move and Enter in one call pick the right item; the
  styled screen shows the selected row.
- `python3`: use `--paste` for a block. The REPL indents for you, so a
  block sent as `--text` with its own indentation is indented twice.
- A password prompt (`read -s`, Python's `getpass`): nothing is echoed, so
  the Enter after the text waits its 300 ms limit; the password arrives
  whole.
- `cmatrix`: never stops drawing. `send` times out with the "never went
  quiet" hint, `resize` answers after its 1 s cap, `wait --until-change`
  and quitting work, and the host uses about 1% of a CPU.
