#!/usr/bin/env bash
# Runs a command and reports the most disk in use while it ran, so that build machines can be sized from
# measurements: peak-disk.sh <path on the disk> <command> [args...]. Exits with the command's status. On
# GitHub Actions the result also goes to the job summary.
set -u
path="$1"
shift

used_kb() { df -Pk "$path" | awk 'NR==2 {print $3}'; }
gib() { awk -v kb="$1" 'BEGIN {printf "%.1f", kb / 1048576}'; }

record="$(mktemp)"
used_kb >"$record"
(
  while sleep 5; do
    now="$(used_kb)"
    [ "$now" -gt "$(cat "$record")" ] && echo "$now" >"$record"
  done
) &
sampler=$!
trap 'kill "$sampler" 2>/dev/null; rm -f "$record"' EXIT

start="$(cat "$record")"
"$@"
status=$?

kill "$sampler" 2>/dev/null
wait "$sampler" 2>/dev/null
now="$(used_kb)"
peak="$(cat "$record")"
[ "$now" -gt "$peak" ] && peak="$now"
size="$(df -Pk "$path" | awk 'NR==2 {print $2}')"

line="peak disk used: $(gib "$peak") GiB of $(gib "$size") GiB ($(gib "$start") GiB at start): $1"
echo "$line"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  echo "- $line" >>"$GITHUB_STEP_SUMMARY"
fi
exit "$status"
