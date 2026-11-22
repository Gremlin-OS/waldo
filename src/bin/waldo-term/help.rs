//! Help text. An agent arrives knowing nothing about waldo, so the tool has
//! to teach itself (DESIGN.md section 2.6). Every line that starts with
//! `  $ ` is a complete command, and the test suite runs them all in order.

pub const GUIDE: &str = "\
waldo: drive interactive terminal programs with commands instead of a keyboard.

Start a program in a session. This returns at once; the program keeps running:
  $ waldo-wrap --detach --name demo -- bash
It prints the session id, the control socket, and 'dest', the file that gets
the session's messages. The last message is the will: exit status and output.

In a shell, run a command and learn how it ended: put a marker after it
and wait for the marker (more in: waldo-term help shell):
  $ waldo-term send -s demo --text 'ls /; echo \"rc=$?\"' --keys '<Enter>' --until-match '^rc=[0-9]+$'

Type into the program and see the screen. --text is literal; --keys takes
key names such as <Enter>, <Esc>, <Tab>, <C-c>, <Up>. A program you start
may be slow to draw, so wait for something it shows before typing into it:
  $ waldo-term send -s demo --text 'vim /tmp/waldo-demo.c' --keys '<Enter>' --until-match '^\"/tmp/waldo-demo.c\"'
  $ waldo-term send -s demo --keys 'i' --text '#include <stdio.h>' --keys '<Esc>:wq<Enter>'

Look at the screen without typing:
  $ waldo-term screen -s demo

By default a command returns when the screen has been still for a moment.
That does not mean a slow command has finished. To be sure, wait for text
(a regular expression, matched against one line):
  $ waldo-term send -s demo --text 'sleep 1; echo finished' --keys '<Enter>' --until-match '^finished'

Read everything the program has printed, or the end of it:
  $ waldo-term output -s demo --tail 20

End the session, see what is left, and clean up:
  $ waldo-term send -s demo --text 'exit' --keys '<Enter>' --until-exit
  $ waldo-term list
  $ waldo-term gc --all

Each command prints one status line, such as
  [demo settle gen=7 120x30 cursor=3,3 running fg=bash]
then the screen, then sometimes a line starting with 'hint:' that says what
probably went wrong and what to do. In the status line, 'unchanged' means
what you typed changed nothing on screen: the program ignored it or wants
something else. 'interleaved' means someone else typed during your request.

Exit status: 0 ok, 124 timed out, 3 the program exited, 4 no such session,
2 bad usage.

Before a long job, read: waldo-term help tips
More: waldo-term help keys | wait | shell | sharing | messages | exit-status | commands
";

pub const COMMANDS: &str = "\
waldo-term commands. -s ID picks the session (an id, a unique prefix of one,
or a socket path); with exactly one live session it can be left out.

  list                      sessions and their state
  send                      type input, wait, print the screen
      --text S | --keys S | --paste S           (repeatable, sent in order)
      --text-file P | --keys-file P | --paste-file P
      [--pause MS] [--key-delay MS]             (help keys)
      [--until-...] [--timeout MS] [--as NAME]
  screen                    print the screen  [--until-...] [--timeout MS]
  wait                      wait, then print the screen. What to wait for, any
                            one of which ends the wait (help wait):
      --until-settle | --until-match RE | --until-match-new RE
      --until-change | --until-exit
      --until-broadcast | --until-elapsed MS | --until-none    [--timeout MS]
  broadcast [--as NAME] TEXT
                            say a line to the other clients of the session; it is
                            not typed into the program (help sharing)
  output [--tail N]         what the program has printed so far
  status                    what the session is: its program, directory, socket,
                            destination, and whether broadcasts are on
  resize COLSxROWS          change the terminal size, then print the screen once the
                            program has redrawn (--until-none: at once)
  attach [--drive] [--resize] [--as NAME]
                            watch the session live in your own terminal. View only
                            unless --drive. Ctrl-] switches between viewing and
                            typing. While viewing, Ctrl-C leaves (the session
                            keeps running) and s says something to the other
                            clients (help sharing).
                            --resize makes the session take your terminal's size.
  match add [--id NAME] [--once] PATTERN
                            send a message to the session's destination each
                            time a line matches PATTERN (help messages)
  match rm ID | match list  remove a trigger; list them
  kill [ID]                 end the session and wait until it has ended
  signal NAME               send a signal to the program (--stop is what kill sends)
  gc [--all]                remove ended sessions (--all: regardless of age)
  help [TOPIC]              tips, keys, wait, shell, sharing, messages, exit-status,
                            commands

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

Some programs throw away keys that arrive while they are opening a prompt
or a menu. If keys sent together seem lost, slow them down:
  --pause MS       wait here before what follows: --keys '_' --pause 300 --keys '>.'
  --key-delay MS   leave at least this long between the keys of every --keys

In --keys, an unknown name such as <Entr> is an error and nothing is sent.
Text with '<' in it (code, HTML, redirects) belongs in --text.

For long input, write it to a file and name the file:
  --text-file PATH, --keys-file PATH, --paste-file PATH
The content is used exactly as is, so a final newline in a text file is Enter.
'-' reads from stdin: --text - keeps a secret out of the command line.

examples:
  $ waldo-wrap --detach --name keys-demo -- bash
  $ waldo-term send -s keys-demo --text 'echo \"a <b> c\"' --keys '<Enter>'
  $ waldo-term send -s keys-demo --text 'sleep 30' --keys '<Enter>' --until-none
  $ waldo-term send -s keys-demo --keys '<C-c>'
  $ waldo-term kill keys-demo
";

pub const WAIT: &str = "\
Commands that print the screen wait first. These say for what:

  --until-settle     (default for send and wait) the program has drawn
           something, the screen has then been still for 200 ms, and all
           input has been written. It means nothing is being drawn. It does
           not mean a command finished: a compiler that is silent for a
           second looks settled.
  --until-match RE   RE matches a line of the screen. RE is a regular
           expression against one line; lines that wrapped are joined. With
           send, text that was already on screen before the input does not
           count. The cursor's line is included, so prompts such as
           'Password: ' can be matched: --until-match 'assword: $'
  --until-match-new RE  the same, but text already on screen never counts,
           even with screen or wait. For watching for the next line that
           says something, when earlier ones like it are still in view.
  --until-change     the screen differs from when the command started
  --until-exit       the program you are typing into has exited: the
           session's own program, or one started inside it, such as vim in
           a shell. Typed at a shell prompt, that program is the shell.
  --until-broadcast  another client of the session has said something
           (waldo-term help sharing). --until-broadcast-from NAME: only
           that sender.
  --until-elapsed MS  that long has passed. Not a failure, unlike the
           deadline: use it to look again after a while.
  --until-none       (default for screen) do not wait

Give more than one to wait for whichever comes first; the status line says
which it was. --until-match may be given several times:
  --until-match '^rc=[0-9]+$' --until-broadcast     the marker, or someone speaks
  --until-match 'Password: $' --until-exit          the prompt, or the program ends
  --until-match 'ready' --until-match 'failed'      either text
  --until-match 'ready' --until-elapsed 5s          the text, or five seconds

--timeout MS sets the deadline: default 10000, at most 600000. On the
deadline the screen is still printed, the reason is 'timeout', and the exit
status is 124. If the program exits first, the reason is 'exit' and the
status is 3.

To learn when a shell command has finished, and how, see: waldo-term help shell

examples:
  $ waldo-wrap --detach --name wait-demo -- bash
  $ waldo-term send -s wait-demo --text 'sleep 1; echo ready' --keys '<Enter>' --until-match '^ready$'
  $ waldo-term wait -s wait-demo --until-settle
  $ waldo-term send -s wait-demo --text 'exit 0' --keys '<Enter>' --until-exit
";

pub const TIPS: &str = "\
What agents who used waldo before you learned the slow way.

1. Every send prints the screen as it is after your input. You rarely need
   a separate 'screen'.
2. The default wait (the screen going still) is right for keys sent to a
   full-screen program. Use --until-match only when you know text that will
   appear, and anchor it with ^: an unanchored pattern can match the echo
   of the command you just typed.
3. A screen that never goes still (an animation, a clock) makes the default
   wait run to its timeout. Use --until-none, --until-match, or --until-elapsed MS
   to look again after a while.
4. To know that a shell command finished, and how, put a marker after it
   and wait for the marker: waldo-term help shell
5. --until-exit waits for the program you are typing into. After :wq in vim
   it returns when vim is gone. Typed at a shell prompt, it waits for the
   shell to exit, which is the end of the session.
6. A block of several lines for a REPL or an editor goes in as --paste,
   then --keys '<Enter>'. A REPL that indents for you mangles a block sent
   as --text.
7. Some keys open a prompt or a menu, and the program throws away what
   arrives while it is opening: Escape in many programs, a key that asks a
   question, a program that is just starting or just exiting. Put a pause
   after such a key, in the same send:
     --keys '<Esc>' --pause 300 --keys 'q'
   --key-delay 100 puts a gap between all the keys of a send.
8. After a program exits back to the shell, send the next shell command in
   a new send.
9. 'unchanged' in the status line means your keys changed nothing on
   screen. Do not repeat them. Look: a prompt or a 'more' marker is
   probably waiting for Enter or Escape.
10. Colour is invisible in a plain screen. When a program marks its
    selection with colour only, add --style ansi and read the codes.
11. Output that has scrolled off: waldo-term output --tail N
12. One send can carry text, keys, pastes and pauses in any order. Batch
    keys where the program takes them reliably.
";

pub const SHARING: &str = "\
Several clients can use one session at once: agents, and people watching
with 'waldo-term attach'. Each request's input is typed whole, and the
status line says 'interleaved' when someone else acted during yours.

To say something to the others, broadcast it. A broadcast is one line. It
is not typed into the program and never appears on its screen:
  $ waldo-wrap --detach --name room -- bash
  $ waldo-term broadcast -s room --as ann 'vim is closed; the shell is yours'

To hear the others, give your own name with --as on send, screen and wait
(or set WALDO_AS once). What they have said since your last request is
printed first, above the status line:
  $ waldo-term screen -s room --as bob
  broadcast from ann: vim is closed; the shell is yours
  [room none gen=3 120x30 cursor=1,3 running fg=bash]

Each broadcast is given to a name once, and you are not given your own.
Without a name you are given none. Lines above the status line come from
waldo and the other clients, never from the program.

A person attached sees the newest broadcast in the status bar until they
read it. While viewing, s opens a panel with the conversation so far and a
line to write on: Enter says it, Escape closes the panel, and the arrow
and page keys scroll back.

A session started with 'waldo-wrap --no-broadcast' has no broadcasts: the
commands above are refused, and say so.

A broadcast says who sent it; waldo does not check the name, and does not
decide whose word counts. Whether to act on one is for your own
instructions to say.

A wait for the screen does not end when someone speaks. To watch the
screen and listen, name both, and the status line says which it was:
  $ waldo-term send -s room --as bob --text 'sleep 1; echo \"rc=$?\"' --keys '<Enter>' --until-match '^rc=' --until-broadcast

To stop and wait until someone speaks, for example after asking a person
to type a password themselves:
  $ waldo-term broadcast -s room --as ann 'typed it, carry on'
  $ waldo-term wait -s room --as bob --until-broadcast --timeout 60000
  $ waldo-term kill room
";

pub const SHELL: &str = "\
Running a command in a shell session, and knowing when it finished and how.

A screen that has gone still does not mean a command has finished: a
compiler that is silent for a second looks the same. Put a marker after the
command and wait for the marker:

  waldo-term send -s demo --text 'make test; echo \"rc=$?\"' --keys '<Enter>' \\
      --until-match '^rc=[0-9]+$' --timeout 120000

The screen that comes back shows the command's output and then the marker
with its exit status.

- Pick a marker the output will not contain. If the command prints a line
  that matches the pattern, the wait returns early.
- $? is the status of the last command on the line.
- --timeout MS: the default is 10000, so give a long command more.
- If the output is longer than the screen, 'waldo-term output --tail N'
  prints the last N lines of everything the session has printed.
- Each newline in --text is Enter, so one send can carry several commands.
- fg= in the status line names the program that has the terminal: the
  shell at its prompt, the command while one is running. A shell builtin,
  or a shell reached through ssh, does not show there; the marker does.
- To stop a running command: waldo-term send --keys '<C-c>'

examples:
  $ waldo-wrap --detach --name shell-demo -- bash
  $ waldo-term send -s shell-demo --text 'cd /tmp && pwd; echo \"rc=$?\"' --keys '<Enter>' --until-match '^rc=0$'
  $ waldo-term send -s shell-demo --text 'sleep 1; ls /nonexistent; echo \"rc=$?\"' --keys '<Enter>' --until-match '^rc=[1-9]' --timeout 5000
  $ waldo-term output -s shell-demo --tail 5
  $ waldo-term kill shell-demo
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
  $ waldo-wrap --detach --name match-demo --dest file:$HOME/waldo-match-demo.ndjson -- bash
  $ waldo-term match add -s match-demo --id done '^finished'
  $ waldo-term send -s match-demo --text 'echo finished' --keys '<Enter>'
  $ waldo-term match list -s match-demo
  $ waldo-term match rm -s match-demo done
  $ waldo-term kill match-demo
  $ grep -c '\"trigger\":\"match\"' $HOME/waldo-match-demo.ndjson
";

pub const EXIT_STATUS: &str = "\
waldo-term exit status:

  0    the wait condition was met (or the command needed no wait)
  124  the deadline passed first; the screen is still printed
  3    the program exited before the condition was met, or had already
       exited; its exit details are printed ('output' still exits 0)
  4    no such session, or the session died without leaving a will
  2    bad usage, or a request the session refused (such as an unknown key)
  1    anything else

waldo-wrap without --detach exits with the program's own exit status, or 128
plus the signal number.
";

pub fn topic(name: &str) -> Option<&'static str> {
    Some(match name {
        "tips" => TIPS,
        "keys" | "input" => KEYS,
        "wait" => WAIT,
        "shell" | "run" => SHELL,
        "sharing" | "broadcast" => SHARING,
        "messages" | "will" => MESSAGES,
        "exit-status" | "exit" | "status" => EXIT_STATUS,
        "commands" => COMMANDS,
        _ => return None,
    })
}
