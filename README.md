# Waldo

Waldo lets an LLM agent, or a person, operate interactive terminal programs
through messages instead of a keyboard. It is a terminal, like xterm or tmux,
with a JSON endpoint where the keyboard would be.

It is two Linux programs:

- `waldo-wrap` hosts one program in a pty, tracks its screen with a terminal
  emulator, and serves a control API on a unix socket. One process per
  wrapped program; there is no daemon.
- `waldo-term` is the client: it sends keys, prints screens, waits for
  conditions, and attaches a person to a session.

## A first session

```sh
waldo-wrap --detach --name demo -- bash        # start; returns at once
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
- **Shell commands** need no special support: put a marker after the
  command and wait for it (`waldo-term help shell`). A quiet screen alone
  cannot tell you that a command has finished, or how.
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

With Nix:

```sh
nix build            # both binaries, in ./result/bin
nix flake check      # build, clippy, and all tests
nix develop          # a shell with cargo and the test programs
```

Inside `nix develop`, `cargo build` and `cargo test -- --test-threads=1`
work as usual. The tests start real programs in ptys and are sensitive to
timing, so they run one at a time.

Without Nix you need Rust (cargo), Zig 0.17, a C library to link against,
and for the tests `bash`, `vim` and `ps`:

```sh
scripts/build-vt.sh                       # the emulator library, about five minutes, once
cargo build --release --locked            # both binaries, in target/release
cargo test --release --locked -- --test-threads=1
scripts/dist.sh                           # optional: a tarball in target/dist
```

`scripts/build-vt.sh` builds the emulator with Zig and puts it where the
cargo build looks for it. To use a library built some other way, set
`LIBGHOSTTY_VT_LIB_DIR` to the directory that holds `libghostty-vt.a`.

The terminal emulator is libghostty-vt 1.0.1, built from the tarballs in
`vendor/libghostty-vt` and linked statically. The build needs no network
beyond fetching the Rust crates.

## Releases

Pushing a tag `vX.Y.Z` that matches the version in `Cargo.toml` builds
tarballs for x86_64 and aarch64 Linux on GitHub, runs the tests on each,
and makes a draft release with the tarballs and a `SHA256SUMS` file. A
person publishes the draft. Every push to `main` also keeps an x86_64
tarball as a workflow artifact.

The binaries are built on Ubuntu 22.04 and need glibc 2.35 or newer. They
need nothing else at run time. Each tarball carries the licences of the
emulator and of the Rust crates linked in.

It runs on Linux only.

## More

- `PROTOCOL.md`: the reference for writing another client or a harness
  integration: the session directory, the control API, the attach stream
  and the messages.
- `DESIGN.md`: how it works and why.
- `TODO.md`: the open work.
- `soak/README.md`: the slow tests, run by hand: a scripted hammer, and
  subagents driving real programs.

## Licence

Waldo is under either the MIT licence (`LICENSE-MIT`) or the Apache
Licence 2.0 (`LICENSE-APACHE`), as you prefer. libghostty-vt, in
`vendor/libghostty-vt`, is MIT licensed by the Ghostty project.
