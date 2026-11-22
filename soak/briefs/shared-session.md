Two agents typing into ONE waldo session at the same time. Arena:
`--shared`. Start the session yourself before the agents, so neither has to
wait for the other:

    W wrap --detach --name pool -- bash --norc --noprofile

`{ME}` and `{OTHER}` are the two names (ann, bob). One agent is the closer:
it ends the session at the end; the other must never kill it, type `exit`
into it, or run `gc`.

Tell both: `--json` prints the raw response, which has `"interleaved"`. In
the text form the status line carries the word `interleaved`. It means
someone else typed into the session during your request. Both of you type
into one shell, so use markers with your own name in them (`{ME}rc=`), or
you will match each other's.

1. `W screen -s pool`. Type `echo {ME} here`; `W wait -s pool --until-match
   --until-match '^{OTHER} here' --timeout 120000`.
2. Twelve sends, one call each, as fast as you can: `W send --json -s pool
   --text 'echo {ME}-N-end' --keys '<Enter>'`. Count `interleaved` true and
   false.
3. `W output -s pool`: every line with `{ME}-` or `{OTHER}-` must be exactly
   `$ echo NAME-N-end` or `NAME-N-end`. Quote any that is mixed, cut or
   doubled.
4. Eight commands with a marker, one call each: `W send -s pool --text
   'echo {ME}-cmd-N; echo "{ME}rcN=$?"' --keys '<Enter>' --until-match
   --until-match '^{ME}rcN=0$'`. For each: did the wait return on your own
   marker, and was your line `{ME}-cmd-N` just above it?
5. Five separate sends appending `{ME} 1` to `{ME} 5` to {ME}.txt, then run
   `cat {ME}.txt; wc -l {ME}.txt` in the shell.
6. A slow command while the other keeps typing: `sleep 4; echo {ME} slept`,
   with a marker and `--timeout 15000`. Did the other agent's typing show up
   on your screen before your marker? Did the status line say
   `interleaved`? Was their typing held until your command finished, or
   mixed into it?
7. While the other agent is typing a long request, send `W signal -s pool
   SIGINT` once and `W resize -s pool 110x28` once. Note how long each took
   to answer, and check afterwards that no command line of either agent was
   cut in the middle.
8. `W match add -s pool --id {ME}-t '^{OTHER} says (\w+)'`; `W match list`;
   type `echo {ME} says hello`; wait for `^{OTHER} says`; `W match list`
   again.
9. Type `echo {ME} finished`.
   - The closer: wait for `^{OTHER} finished` (3 minute timeout), repeat
     the check of step 3 over the whole output, then `W kill pool`, `W
     list`, `W gc --all`.
   - The other: `W wait -s pool --until-exit --timeout 300000`; copy what
     it prints and its exit status; then `W screen -s pool` and `W send -s
     pool --text 'echo too late' --keys '<Enter>'`, and copy those too.

Agents rarely overlap by chance. The scripted hammer (see the README) is
the real test of concurrent typing; this one is for what two agents make of
sharing.
