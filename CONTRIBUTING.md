# Contributing to waldo

Bug reports, fixes and improvements are welcome. This page says how to
build and test, and what a change needs before it can be merged.

For a security problem, do not open a public issue. Use "Report a
vulnerability" on the repository's Security tab.

## Building and testing

`README.md` has the commands, with Nix and without. The short form:

```sh
nix flake check                 # build, clippy with warnings as errors, all tests
```

or, without Nix:

```sh
scripts/build-vt.sh             # once
cargo test --release --locked -- --test-threads=1
cargo clippy --all-targets -- -D warnings
```

The tests start real programs (`bash`, `vim`, `ps`) in ptys and are
sensitive to timing, so they run one at a time. If a test fails only now
and then, that is worth reporting: twice so far it has been a real problem in
waldo, not in the test.

## What a change needs

- **A test.** Behaviour is tested by driving real programs through the two
  binaries, in `tests/integration.rs`. A bug fix comes with a test that
  failed before it.
- **`PROTOCOL.md` kept true.** It is the reference for anyone writing
  another client, and the tests check the code against it: every endpoint,
  parameter and error code. A new error code needs a row in its table and
  a case in `every_documented_error_can_be_provoked`.
- **Help kept true.** Every line in the help text that starts with `  $ `
  is run by the tests, in order. The guide (`waldo-term help`) has a
  length limit, because an agent reads it at the start of every job.
- **The reason, in `DESIGN.md`**, when the change is a decision and not
  only a fix. `PROTOCOL.md` says what waldo does; `DESIGN.md` says why.
- **A line in `CHANGELOG.md`** under `[Unreleased]`, for anything a user
  would notice.
- **Clippy clean.** CI fails on any warning.

The code is not formatted with `rustfmt`; please do not run `cargo fmt`
over files you touch, and keep to the style around your change.

## What waldo is, and is not

Changes that fit are easier to merge, so it helps to know the stance.

- **The host is where the behaviour lives.** `waldo-term` is a thin client;
  anything it can do, another client can do over the socket.
- **The host reports facts and lets the caller decide.** It says which
  program has the foreground, whether the screen changed, why a wait
  ended. It does not guess what a program is or what the caller meant.
  Features that are clever on the caller's behalf have been removed more
  than once; `DESIGN.md` records them.
- **The caller controls timing** that the host cannot know, with waits,
  `--pause` and `--key-delay`.
- **Linux only**, and one process per session with no daemon.

`TODO.md` lists the open work, with what is known about each item. It is
the best place to find something to do.

## Soak tests

`soak/README.md` describes the slower tests: a scripted hammer, and runs
in which LLM agents drive real programs and report how waldo behaved.
Those found most of the usability problems fixed so far. They are run by
hand and are not required for a change.

## Pull requests

- One change per pull request. They are squashed into one commit on
  `main`, so the title and description become the commit message: say
  what changed and why.
- CI must pass. Changes that touch only documentation do not start it.

## Licence

Waldo is licensed under MIT or Apache 2.0, at the user's option. Unless
you say otherwise, anything you contribute is under the same terms, with
no additional conditions.
