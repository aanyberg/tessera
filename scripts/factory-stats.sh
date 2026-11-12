#!/usr/bin/env zsh
# Per-agent and per-role cost table for a wave, from Claude Code transcripts. Feeds the flow-analyst retro.
# Usage: scripts/factory-stats.sh [--since <iso-time>] [--until <iso-time>] [--teams-dir <dir>]
#                                 <session-dir-or-transcript>...
#
# A session dir is ~/.claude/projects/<project>/<session-id>; its subagents/*.jsonl are read with their
# .meta.json (agent type, description). A bare .jsonl is reported as "lead". --since/--until keep agents whose
# first message falls in the window (UTC ISO 8601, for example 2026-10-27T21:00:00Z).
#
# Teammates (agent teams): when a session dir (or the lead's bare .jsonl) is the lead session of a team in <teams-dir>/<team>/config.json
# (`leadSessionId`; default <teams-dir> ~/.claude/teams), each member except the lead (`name`, `agentType`, `model`,
# `agentId`) is listed under "Teammates", and its transcript, a sibling `<session id>.jsonl` of the session dir whose
# lines carry `"teamName":"<team>"` and `"agentName":"<name>"`, is counted like a subagent (role = agentType,
# description "teammate <name>"). Only those fields are read from the config: never its prompts (S8).
#
# Output tokens are summed over unique message ids (the largest value seen per id), so they are comparable
# between runs but not billing-grade. "active" is the span with gaps over 3 minutes removed.
set -euo pipefail
since="" until_="" teams_dir="$HOME/.claude/teams"
while (( $# )); do
    case "$1" in
        --since) since="$2"; shift 2 ;;
        --until) until_="$2"; shift 2 ;;
        --teams-dir) (( $# >= 2 )) || { print -u2 -- "--teams-dir needs a directory"; exit 2; }; teams_dir="$2"; shift 2 ;;
        -*) print -u2 "unknown option $1"; exit 2 ;;
        *) break ;;
    esac
done
(( $# )) || { print -u2 "usage: scripts/factory-stats.sh [--since T] [--until T] [--teams-dir D] <session-dir-or-transcript>..."; exit 2; }

files=()
typeset -A file_type file_desc file_model
members=()
# Teammates of the agent team the session <id> leads: their transcripts are siblings of its transcript in <project>.
add_team() {
    local id="$1" project="$2" cfg team name atype model aid f found
    local -a candidates
    for cfg in "$teams_dir"/*/config.json(N); do
        [[ "$(jq -r '.leadSessionId // ""' "$cfg" 2> /dev/null)" == "$id" ]] || continue
        team="${${cfg:h}:t}"
        candidates=( ${(f)"$(grep -l -F "\"teamName\":\"$team\"" "$project"/*.jsonl(N) 2> /dev/null)"} )
        candidates=(${candidates:#})
        while IFS='|' read -r name atype model aid; do
            found=""
            for f in $candidates; do
                [[ "$(grep -m1 -o '"agentName":"[^"]*"' "$f")" == "\"agentName\":\"$name\"" ]] || continue
                found="$f"; break
            done
            if [[ -n "$found" ]]; then
                files+=("$found"); file_type[$found]="$atype"; file_desc[$found]="teammate $name"; file_model[$found]="$model"
            fi
            members+=("| $name | $atype | ${model:--} | $aid | ${${found:t:r}:-not found} |")
        done < <(jq -r '.members[]? | select(.agentType != "team-lead") | [.name, .agentType, (.model // ""), .agentId] | join("|")' "$cfg")
    done
}
for arg in "$@"; do
    if [[ -d "$arg" ]]; then
        files+=( "$arg"/subagents/*.jsonl(N) )
        add_team "${${arg%/}:t}" "${${arg%/}:h}"
    else
        files+=("$arg")
        add_team "${${arg:t}%.jsonl}" "${arg:h}"
    fi
done

rows="$(mktemp)"
trap 'rm -f "$rows"' EXIT
for f in $files; do
    meta="${f%.jsonl}.meta.json"
    if [[ -n "${file_type[$f]:-}" ]]; then
        type="${file_type[$f]}" desc="${file_desc[$f]}"
    elif [[ -f "$meta" ]]; then
        type="$(jq -r '.agentType // "unknown"' "$meta")"
        desc="$(jq -r '.description // ""' "$meta")"
    else
        type=lead desc="${f:t:r}"
    fi
    # A live session can end in a partly written line: parse line by line and skip what does not parse.
    jq -R 'fromjson? // empty' "$f" | jq -s -c --arg type "$type" --arg desc "$desc" --arg fallback "${file_model[$f]:-}" '
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
            model: (([ $m[].model | select(. != "" and . != "<synthetic>") ] | first // "") | if . == "" then $fallback else . end),
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
if (( ${#members} )); then
    print "\n## Teammates (team config)\n"
    print "| name | role | model | agent id | transcript |"
    print "|---|---|---|---|---|"
    print -rl -- $members
fi
