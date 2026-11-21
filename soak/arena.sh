#!/usr/bin/env bash
# Make a fresh arena for a soak run: one wrapper script per agent.
#
#   soak/arena.sh [options] AGENT...
#
#   --bin DIR        where waldo-wrap and waldo-term are (default: ./result/bin)
#   --path DIRS      extra directories for PATH, colon separated (the
#                    programs under test: nethack, htop, vim, tmux, ...)
#   --budget N       typing calls allowed per agent (default 80)
#   --shared         all agents use one session directory, so they can
#                    address the same sessions
#   --shared-home    all agents work in one directory
#   --dir DIR        where to make the arena (default: $TMPDIR or /tmp)
#
# It prints the arena directory and each agent's wrapper. The wrapper, called
# W in the briefs, runs waldo-term (or waldo-wrap, as `W wrap ...`), logs
# every call to AGENT/log with its time and exit status, and counts calls
# that type (send, run), refusing them past the budget.
#
# Nothing is ever deleted: every run gets a new directory.
set -eu

bin=$PWD/result/bin
extra=
budget=80
shared=no
shared_home=no
base=${TMPDIR:-/tmp}
while [ $# -gt 0 ]; do
  case $1 in
    --bin) bin=$2; shift 2 ;;
    --path) extra=$2; shift 2 ;;
    --budget) budget=$2; shift 2 ;;
    --shared) shared=yes; shift ;;
    --shared-home) shared_home=yes; shift ;;
    --dir) base=$2; shift 2 ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    -*) echo "arena.sh: unknown option $1" >&2; exit 2 ;;
    *) break ;;
  esac
done
[ $# -gt 0 ] || { echo "arena.sh: name at least one agent" >&2; exit 2; }
[ -x "$bin/waldo-term" ] || { echo "arena.sh: no waldo-term in $bin; run 'nix build' or pass --bin" >&2; exit 2; }
bin=$(cd "$bin" && pwd)

arena=$(mktemp -d "$base/waldo-soak.XXXXXX")
echo "arena=$arena"
for agent in "$@"; do
  dir=$arena/$agent
  if [ $shared = yes ]; then sessions=$arena/sessions; else sessions=$dir/sessions; fi
  if [ $shared_home = yes ]; then work=$arena/work; else work=$dir/work; fi
  mkdir -p "$dir" "$sessions" "$work"
  chmod 700 "$sessions"
  cat > "$dir/w" <<EOF
#!/usr/bin/env bash
# waldo-term (default) or waldo-wrap ("w wrap ...") for agent $agent.
D=$dir
export WALDO_DIR=$sessions HOME=$work PATH=$bin${extra:+:$extra}:\$PATH PS1='\$ '
unset XDG_RUNTIME_DIR PROMPT_COMMAND WALDO_SESSION WALDO_SOCKET TMUX TMUX_PANE
cd $work || exit 98
BUDGET=$budget
bin=waldo-term
if [ "\$1" = wrap ]; then bin=waldo-wrap; shift; fi
n=\$(cat \$D/count 2>/dev/null || echo 0)
typing=no
if [ \$bin = waldo-term ] && { [ "\$1" = send ] || [ "\$1" = run ]; }; then typing=yes; fi
if [ \$typing = yes ] && [ \$n -ge \$BUDGET ]; then
  echo "budget used up: \$BUDGET typing calls made. Stop and write your report." >&2
  exit 99
fi
t0=\$(date +%s%N)
out=\$(\$bin "\$@" 2>&1); code=\$?
ms=\$(( (\$(date +%s%N) - t0) / 1000000 ))
# A call refused as a usage error typed nothing and is not counted.
if [ \$typing = yes ] && [ \$code -ne 2 ]; then n=\$((n+1)); echo \$n > \$D/count; fi
{
  printf '=== %s #%s ms=%s exit=%s :: %s' "\$(date +%T.%N | cut -c1-12)" "\$n" "\$ms" "\$code" "\$bin"
  printf ' %q' "\$@"
  printf '\n%s\n' "\$out"
} >> \$D/log
printf '%s\n' "\$out"
[ \$typing = yes ] && echo "[typing calls used: \$n of \$BUDGET]"
exit \$code
EOF
  chmod +x "$dir/w"
  echo "$agent: $dir/w"
done
