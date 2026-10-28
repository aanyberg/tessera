#!/usr/bin/env zsh
# Per-agent and per-role cost table for a wave, from Claude Code transcripts. Feeds the flow-analyst retro.
# Usage: scripts/factory-stats.sh [--since <iso-time>] [--until <iso-time>] <session-dir-or-transcript>...
#
# A session dir is ~/.claude/projects/<project>/<session-id>; its subagents/*.jsonl are read with their
# .meta.json (agent type, description). A bare .jsonl is reported as "lead". --since/--until keep agents whose
# first message falls in the window (UTC ISO 8601, for example 2026-10-27T21:00:00Z).
#
# Output tokens are summed over unique message ids (the largest value seen per id), so they are comparable
# between runs but not billing-grade. "active" is the span with gaps over 3 minutes removed.
set -euo pipefail
since="" until_=""
while (( $# )); do
    case "$1" in
        --since) since="$2"; shift 2 ;;
        --until) until_="$2"; shift 2 ;;
        -*) print -u2 "unknown option $1"; exit 2 ;;
        *) break ;;
    esac
done
(( $# )) || { print -u2 "usage: scripts/factory-stats.sh [--since T] [--until T] <session-dir-or-transcript>..."; exit 2; }

files=()
for arg in "$@"; do
    if [[ -d "$arg" ]]; then files+=( "$arg"/subagents/*.jsonl(N) ); else files+=("$arg"); fi
done

rows="$(mktemp)"
trap 'rm -f "$rows"' EXIT
for f in $files; do
    meta="${f%.jsonl}.meta.json"
    if [[ -f "$meta" ]]; then
        type="$(jq -r '.agentType // "unknown"' "$meta")"
        desc="$(jq -r '.description // ""' "$meta")"
    else
        type=lead desc="${f:t:r}"
    fi
    # A live session can end in a partly written line: parse line by line and skip what does not parse.
    jq -R 'fromjson? // empty' "$f" | jq -s -c --arg type "$type" --arg desc "$desc" '
        def epoch: sub("\\.[0-9]+Z$"; "Z") | fromdateiso8601;
        [ .[] | select(.type == "assistant" and .message.id != null) ] as $a
        | ($a | group_by(.message.id) | map({
            out: (map(.message.usage.output_tokens // 0) | max),
            cache: (map(.message.usage.cache_read_input_tokens // 0) | max),
            tools: ([ .[].message.content[]? | select(.type == "tool_use") | .id ] | unique | length),
            model: (.[0].message.model // "")
          })) as $m
        | ([ .[] | .timestamp // empty ] | sort) as $ts
        | ($ts | map(epoch)) as $e
        | select(($ts | length) > 0)
        | {
            type: $type, desc: $desc, start: $ts[0],
            model: ([ $m[].model | select(. != "" and . != "<synthetic>") ] | first // ""),
            out: ($m | map(.out) | add // 0),
            cache: ($m | map(.cache) | add // 0),
            tools: ($m | map(.tools) | add // 0),
            span: (($e[-1] - $e[0]) / 60),
            active: (([ range(1; $e | length) as $i | ($e[$i] - $e[$i - 1]) | select(. <= 180) ] | add // 0) / 60)
          }' >> "$rows"
done

jq -s -r --arg since "$since" --arg until "$until_" '
    def k: if . >= 1000000 then "\(. / 100000 | floor / 10)M" elif . >= 1000 then "\(. / 100 | floor / 10)k" else "\(.)" end;
    def m: (. * 10 | round / 10 | tostring);
    [ .[] | select(($since == "" or .start >= $since) and ($until == "" or .start <= $until)) ] as $r
    | ($r | map(.out) | add // 0) as $total
    | "## Per role\n",
      "| role | model | n | out tokens | share | cache read | tool calls | avg min (span) | avg min (active) |",
      "|---|---|---|---|---|---|---|---|---|",
      ( $r | group_by(.type) | sort_by(-(map(.out) | add)) | .[]
        | "| \(.[0].type) | \([ .[].model ] | unique | join(", ")) | \(length) | \(map(.out) | add | k) | \(if $total > 0 then (map(.out) | add) * 100 / $total | round else 0 end)% | \(map(.cache) | add | k) | \(map(.tools) | add) | \(map(.span) | add / length | m) | \(map(.active) | add / length | m) |" ),
      "| **total** | | \($r | length) | \($total | k) | 100% | \($r | map(.cache) | add // 0 | k) | \($r | map(.tools) | add // 0) | | |",
      "\n## Per agent\n",
      "| start (UTC) | role | description | out tokens | tool calls | span min | active min |",
      "|---|---|---|---|---|---|---|",
      ( $r | sort_by(.start) | .[]
        | "| \(.start[0:16]) | \(.type) | \(.desc | gsub("\\|"; "/")) | \(.out | k) | \(.tools) | \(.span | m) | \(.active | m) |" )
' "$rows"
