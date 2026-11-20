//! Help text. An agent arrives knowing nothing about waldo, so the tool has
//! to teach itself (DESIGN.md section 2.6). Every line that starts with
//! `  $ ` is a complete command, and the test suite runs them all in order.

pub const GUIDE: &str = "\
waldo: drive interactive terminal programs with commands instead of a keyboard.

Start a program in a session. This returns at once; the program keeps running:
  $ waldo-wrap --detach --name demo -- bash
It prints the session id, the control socket, and 'dest', the file that gets
the session's messages. The last message is the will: exit status and output.

Run a shell command and get its output and exit status:
  $ waldo-term run -s demo 'echo hello; ls /'

Type into the program and see the screen. --text is literal; --keys takes
key names such as <Enter>, <Esc>, <Tab>, <C-c>, <Up>:
  $ waldo-term send -s demo --text 'vim /tmp/waldo-demo.c' --keys '<Enter>'
  $ waldo-term send -s demo --keys 'i' --text '#include <stdio.h>' --keys '<Esc>:wq<Enter>'

Look at the screen without typing:
  $ waldo-term screen -s demo

By default a command returns when the screen has been still for a moment.
That does not mean a slow command has finished. To be sure, wait for text
(a regular expression, matched against one line):
  $ waldo-term send -s demo --text 'sleep 1; echo finished' --keys '<Enter>' --until match --pattern '^finished'

Read everything the program has printed, or the end of it:
  $ waldo-term output -s demo --tail 20

End the session, see what is left, and clean up:
  $ waldo-term send -s demo --text 'exit' --keys '<Enter>' --until exit
  $ waldo-term list
  $ waldo-term gc --all

Each command prints one status line, such as
  [demo settle gen=7 120x30 cursor=3,3 running fg=bash]
then the screen (or the output, for run), then sometimes a line starting
with 'hint:' that says what probably went wrong and what to do.

Exit status: 0 ok, 124 timed out, 3 the program exited, 4 no such session,
5 a run command failed, 2 bad usage.

More: waldo-term help keys | wait | run | messages | exit-status | commands
";

pub const COMMANDS: &str = "\
waldo-term commands. -s ID picks the session (an id, a unique prefix of one,
or a socket path); with exactly one live session it can be left out.

  list                      sessions and their state
  run COMMAND               run a shell command; print its output and exit status
      [--timeout MS] [--shell sh|fish]
  send                      type input, wait, print the screen
      --text S | --keys S | --paste S           (repeatable, sent in order)
      --text-file P | --keys-file P | --paste-file P
      [--until U] [--pattern RE] [--timeout MS]
  screen                    print the screen  [--until U] [--pattern RE] [--timeout MS]
  wait                      wait, then print the screen
      --until settle|match|change|exit|done [--pattern RE] [--timeout MS]
  output [--tail N]         what the program has printed so far
  resize COLSxROWS          change the terminal size
  attach [--drive] [--resize]
                            watch the session live in your own terminal. View only
                            unless --drive; Ctrl-\\ toggles typing, Ctrl-] detaches.
                            --resize makes the session take your terminal's size.
  match add [--id NAME] [--once] PATTERN
                            send a message to the session's destination each
                            time a line matches PATTERN (help messages)
  match rm ID | match list  remove a trigger; list them
  kill [ID]                 end the session and wait until it has ended
  signal NAME               send a signal to the program (--stop is what kill sends)
  gc [--all]                remove ended sessions (--all: regardless of age)
  help [TOPIC]              keys, wait, run, messages, exit-status, commands

Any command that prints a screen takes --json for the full response, and
--style ansi to keep colour codes in the screen text. Programs often mark the
selected menu entry or button with colour alone; plain text cannot show that.
'-' as the value of --text, --keys or --paste reads it from stdin.

examples:
  $ waldo-term list
  $ waldo-term help keys
";

pub const KEYS: &str = "\
Input is a list of items, sent in order:

  --text S    literal characters. Nothing in S is special. A newline is Enter.
  --keys S    key names in <...>, with literal characters between them.
  --paste S   a paste: sent in one piece, bracketed if the program supports it.

Key names (case does not matter):
  <Esc> <Enter> <Tab> <BS> <Del> <Space> <Insert>
  <Up> <Down> <Left> <Right> <Home> <End> <PageUp> <PageDown>
  <F1> to <F12>
  <lt>                a literal '<'
Modifiers: <C-x> Ctrl, <M-x> Alt, <S-Tab> Shift; they combine: <C-S-Left>.

A key that follows text waits until the program has shown the text, as a
person would, so --text PROMPT --keys '<Enter>' submits what it typed.

In --keys, an unknown name such as <Entr> is an error and nothing is sent.
Text with '<' in it (code, HTML, redirects) belongs in --text.

For long input, write it to a file and name the file:
  --text-file PATH, --keys-file PATH, --paste-file PATH
The content is used exactly as is, so a final newline in a text file is Enter.
'-' reads from stdin: --text - keeps a secret out of the command line.

examples:
  $ waldo-wrap --detach --name keys-demo -- bash
  $ waldo-term send -s keys-demo --text 'echo \"a <b> c\"' --keys '<Enter>'
  $ waldo-term send -s keys-demo --text 'sleep 30' --keys '<Enter>' --until none
  $ waldo-term send -s keys-demo --keys '<C-c>'
  $ waldo-term kill keys-demo
";

pub const WAIT: &str = "\
Commands that print the screen wait first. --until says for what:

  settle   (default for send and wait) the program has drawn something, the
           screen has then been still for 200 ms,
           and all input has been written. It means nothing is being drawn. It
           does not mean a command finished: a compiler that is silent for a
           second looks settled.
  match    --pattern RE matches a line of the screen. RE is a regular
           expression against one line; lines that wrapped are joined. With
           send, text that was already on screen before the input does not
           count. The cursor's line is included, so prompts such as
           'Password: ' can be matched: --pattern 'assword: $'
  change   the screen differs from when the command started
  exit     the program has exited
  done     the command started by 'run' has finished
  none     (default for screen) do not wait

--timeout MS sets the deadline: default 10000, at most 600000. On the
deadline the screen is still printed, the reason is 'timeout', and the exit
status is 124. If the program exits first, the reason is 'exit' and the
status is 3.

To learn when a shell command has finished, use 'waldo-term run', or
--until match on its expected output or on the prompt.

examples:
  $ waldo-wrap --detach --name wait-demo -- bash
  $ waldo-term send -s wait-demo --text 'sleep 1; echo ready' --keys '<Enter>' --until match --pattern '^ready$'
  $ waldo-term wait -s wait-demo --until settle
  $ waldo-term send -s wait-demo --text 'exit 0' --keys '<Enter>' --until exit
";

pub const RUN: &str = "\
waldo-term run COMMAND types COMMAND into a shell session, waits until it has
finished, and prints what it printed and its exit status:

  [demo done exit=2 4.1s gen=212 120x30 cursor=24,3 running fg=bash]
  cc -c foo.c
  foo.c:12: error: expected ';' before 'return'

The exit status of waldo-term is 0 if COMMAND exited 0, and 5 if it did not.

- The session's program must be a shell (sh, bash or zsh; --shell fish for
  fish) sitting at its prompt.
- --timeout MS: default 120000. On the deadline the command is still running;
  pick it up later with: waldo-term wait --until done
- One run at a time per session. To stop a running command: --keys '<C-c>'.
- For a program that asks questions, use send and --until match instead.

examples:
  $ waldo-wrap --detach --name run-demo -- bash
  $ waldo-term run -s run-demo 'cd /tmp && pwd'
  $ waldo-term run -s run-demo --timeout 5000 'sleep 1; echo slept'
  $ waldo-term kill run-demo
";

pub const MESSAGES: &str = "\
Every session has a destination, printed as 'dest' when it starts. Messages
go to it, one JSON object each. By default it is events.ndjson in the
session directory. waldo-wrap --dest URI picks another:

  file:/PATH             append one line per message
  unix:/PATH             connect to a socket and write one line per message
  http://HOST:PORT/PATH  POST each message; the receiver must answer 2xx

Always sent:
  hello   first message: the command line, the socket, the size
  exit    last message, the will. Fields:
            exit_code        the program's exit status, or null
            signal           the signal that killed it, such as \"SIGKILL\", or null
            core_dumped, killed_by_waldo, kill_reason, spawn_error, duration_ms
            screen           the last screen
            output.text      everything the program printed, as it was rendered
            output.truncated true if the middle was cut; omitted_lines says how much

Sent when asked for:
  match   a line matched a trigger added with 'waldo-term match add PATTERN'.
          Fields: match.id, match.text, match.groups, match.count, and the
          screen. It is sent once per appearance: text that stays on screen,
          or is redrawn before the screen goes still, does not repeat.
          --once removes the trigger after its first match.
  settle  waldo-wrap --on settle: the screen, each time it changes and then
          goes still
  stream  waldo-wrap --on stream: output lines as they scroll off the
          screen, in 'lines', at most four messages a second
  heartbeat  waldo-wrap --heartbeat SECS: sent after SECS seconds with no
          other message, so a receiver can tell a dead host from a quiet one

Each message has 'seq', counting from 0. If the receiver is too slow,
messages are dropped, never hello or exit; the next message that arrives
carries 'dropped' with the count.

After a session has ended, waldo-term commands for it print the will and
exit with status 3. 'waldo-term gc' removes ended sessions.

examples:
  $ waldo-wrap --name msg-demo -- echo hello
  $ waldo-term output -s msg-demo
  $ waldo-wrap --detach --name match-demo --dest file:/tmp/waldo-match-demo.ndjson -- bash
  $ waldo-term match add -s match-demo --id done '^finished'
  $ waldo-term run -s match-demo 'echo finished'
  $ waldo-term match list -s match-demo
  $ waldo-term match rm -s match-demo done
  $ waldo-term kill match-demo
  $ grep -c '\"trigger\":\"match\"' /tmp/waldo-match-demo.ndjson
";

pub const EXIT_STATUS: &str = "\
waldo-term exit status:

  0    the wait condition was met (or the command needed no wait)
  124  the deadline passed first; the screen is still printed
  3    the program exited before the condition was met, or had already
       exited; its exit details are printed ('output' still exits 0)
  4    no such session, or the session died without leaving a will
  5    'run': the command finished with a non-zero exit status
  2    bad usage, or a request the session refused (such as an unknown key)
  1    anything else

waldo-wrap without --detach exits with the program's own exit status, or 128
plus the signal number.
";

pub fn topic(name: &str) -> Option<&'static str> {
    Some(match name {
        "keys" | "input" => KEYS,
        "wait" => WAIT,
        "run" => RUN,
        "messages" | "will" => MESSAGES,
        "exit-status" | "exit" | "status" => EXIT_STATUS,
        "commands" => COMMANDS,
        _ => return None,
    })
}
