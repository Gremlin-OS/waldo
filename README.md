# Waldo

Waldo lets an LLM agent, or a person, operate interactive terminal programs
through messages instead of a keyboard. It is a terminal, like xterm or tmux,
with a JSON endpoint where the keyboard would be.

It is two Linux programs:

- `waldo-wrap` hosts one program in a pty, tracks its screen with a terminal
  emulator, and serves a control API on a unix socket. One process per
  wrapped program; there is no daemon.
- `waldo-term` is the client: it sends keys, prints screens, waits for
  conditions, runs shell commands, and attaches a person to a session.

## A first session

```sh
waldo-wrap --detach --name demo -- bash        # start; returns at once
waldo-term run -s demo 'echo hello; ls /'      # a shell command: output and exit status
waldo-term send -s demo --text 'vim notes.txt' --keys '<Enter>'
waldo-term send -s demo --keys 'i' --text 'some text' --keys '<Esc>:wq<Enter>'
waldo-term screen -s demo                      # look without typing
waldo-term attach -s demo                      # watch it live; Ctrl-] detaches
waldo-term kill demo                           # end it
```

Each command prints one status line, then the screen:

```
[demo settle gen=7 120x30 cursor=3,3 running fg=bash]
$ echo hello
hello
$
```

`waldo-term help` is the guide, written for an agent that has never seen
waldo. Every example in it is run by the test suite. `waldo-term help
commands` lists every option, and `waldo-wrap --help` covers the host.

## What it does

- **Input** is text, named keys (`<Enter>`, `<C-c>`, `<Up>`), or a paste,
  encoded the way the program's current terminal modes call for.
- **Waiting** is part of every request: until the screen goes still, until a
  line matches a pattern, until the screen changes, until the program exits.
- **`run`** types a shell command and returns its output and exit status,
  which a quiet screen alone cannot tell you.
- **Messages** go to a destination: a file, a unix socket, or an HTTP
  endpoint. The last one is the will, with how the program ended and
  everything it printed. On request there are also messages for pattern
  matches, for each time the screen settles, and for output lines as they
  scroll off.
- **Attach** shows a session live in your own terminal and lets you type
  into it.

The control API is HTTP over the unix socket, so `curl --unix-socket` works
without `waldo-term`.

## Building

The build uses Nix:

```sh
nix build            # both binaries, in ./result/bin
nix flake check      # build, clippy, and all tests
nix develop          # a shell with cargo and the test programs
```

Inside `nix develop`, `cargo build` and `cargo test -- --test-threads=1`
work as usual. The tests start real programs in ptys and are sensitive to
timing, so they run one at a time.

The terminal emulator is libghostty-vt 1.0.1, built from the tarballs in
`vendor/libghostty-vt` and linked statically. The build needs no network
beyond what Nix fetches for the Rust crates.

It runs on Linux only.

## More

- `DESIGN.md`: how it works and why, and what is not built yet.
- `TODO.md`: the open work.
- `soak/README.md`: the slow tests, run by hand: a scripted hammer, and
  subagents driving real programs.
