#!/usr/bin/env zsh
# Exercise scripts/factory-stats.sh on a synthetic project directory: a lead session with one subagent, and an agent
# team (<teams dir>/<team>/config.json) whose teammates have sibling transcripts. Nothing real is read or copied: a
# team config and a transcript hold other projects' prompts and paths, so every fixture is written here.
#
#   scripts/test-factory-stats.sh
set -euo pipefail

script="$(cd "$(dirname "$0")" && pwd)/factory-stats.sh"
work="$(mktemp -d "${TMPDIR:-/tmp}/factory-stats-test.XXXXXX")"
trap 'rm -rf "$work"' EXIT
failures=0
ok() { print -r -- "ok   $1"; }
not_ok() { print -r -- "FAIL $1"; failures=$((failures + 1)); }

project="$work/project" lead="lead-session-id"
mkdir -p "$project/$lead/subagents" "$work/teams/session-fix" "$work/no-teams"

# <out tokens> <message id> <timestamp> [team agent]: one assistant line with a tool call.
line() {
    local extra=""
    [[ -n "${4:-}" ]] && extra=",\"teamName\":\"$4\",\"agentName\":\"$5\""
    print -r -- "{\"type\":\"assistant\",\"timestamp\":\"$3\",\"message\":{\"id\":\"$2\",\"model\":\"claude-sonnet-5-5\",\"usage\":{\"output_tokens\":$1,\"cache_read_input_tokens\":2000},\"content\":[{\"type\":\"tool_use\",\"id\":\"tu-$2\"}]}$extra}"
}
line 300 sub1 2026-11-12T10:00:00.000Z > "$project/$lead/subagents/agent-1.jsonl"
line 100 sub2 2026-11-12T10:02:00.000Z >> "$project/$lead/subagents/agent-1.jsonl"
print -r -- '{"agentType":"rust-critic","description":"critic for task 7"}' > "$project/$lead/subagents/agent-1.meta.json"

# alpha has a transcript, beta does not; a transcript of another team's alpha must not be counted.
line 700 a1 2026-11-12T10:00:00.000Z session-fix t-alpha > "$project/teammate-alpha.jsonl"
line 800 a2 2026-11-12T10:01:00.000Z session-fix t-alpha >> "$project/teammate-alpha.jsonl"
line 9000 o1 2026-11-12T10:00:00.000Z session-other t-alpha > "$project/other-team.jsonl"
cat > "$work/teams/session-fix/config.json" <<JSON
{"name":"session-fix","leadSessionId":"$lead","members":[
 {"agentId":"team-lead@session-fix","name":"team-lead","agentType":"team-lead"},
 {"agentId":"t-alpha@session-fix","name":"t-alpha","agentType":"release-engineer","model":"sonnet","prompt":"SECRET-PROMPT-CANARY /Users/someone/else"},
 {"agentId":"t-beta@session-fix","name":"t-beta","agentType":"rust-implementer","model":"opus","prompt":"SECRET-PROMPT-CANARY"}]}
JSON

plain="$("$script" --teams-dir "$work/no-teams" "$project/$lead")"
with_team="$("$script" --teams-dir "$work/teams" "$project/$lead")"

# A session dir with no team is reported as before: the subagent only, no Teammates section.
if [[ "$plain" == *"| rust-critic |"* && "$plain" == *"| critic for task 7 |"* && "$plain" != *Teammates* && "$plain" != *release-engineer* ]]; then
    ok "a session without a team reports its subagents only"
else not_ok "plain session: $plain"; fi
# Its per-role and per-agent tables are the first lines of the team run too (the team adds rows, never changes them).
if [[ "$(print -r -- "$plain" | grep -c 'rust-critic')" == 2 && "$(print -r -- "$with_team" | grep -c 'rust-critic')" == 2 ]]; then
    ok "the subagent rows are the same with a team"
else not_ok "subagent rows differ"; fi
# A teammate: its transcript is counted (700 + 800 output tokens, 2 tool calls) under its agent type, with its name.
if [[ "$with_team" == *"| release-engineer | claude-sonnet-5-5 | 1 | 1.5k |"* && "$with_team" == *"| release-engineer | teammate t-alpha | 1.5k | 2 |"* ]]; then
    ok "a teammate transcript is counted as its role"
else not_ok "teammate row: $with_team"; fi
# The members come from the config: both listed, the lead is not, a missing transcript is said so.
if [[ "$with_team" == *"## Teammates"* && "$with_team" == *"| t-alpha | release-engineer | sonnet | t-alpha@session-fix | teammate-alpha |"* \
    && "$with_team" == *"| t-beta | rust-implementer | opus | t-beta@session-fix | not found |"* && "$with_team" != *"| team-lead |"* ]]; then
    ok "teammates are listed from the team config"
else not_ok "teammates section: $with_team"; fi
# Another team's transcript for the same agent name is not counted.
if [[ "$with_team" == *"| **total** | | 2 | 1.9k |"* ]]; then ok "another team's transcript is ignored"; else not_ok "foreign transcript counted: $with_team"; fi
# S8: no prompt or path from the config reaches the output.
if [[ "$with_team" != *SECRET-PROMPT-CANARY* && "$with_team" != */Users/someone* ]]; then ok "the output carries no prompt"; else not_ok "prompt leaked"; fi
# The window applies to teammates too.
windowed="$("$script" --teams-dir "$work/teams" --since 2026-11-12T11:00:00Z "$project/$lead")"
if [[ "$windowed" != *"teammate t-alpha"* && "$windowed" == *"| t-alpha |"* ]]; then ok "--since drops a teammate outside the window but still lists it"; else not_ok "window: $windowed"; fi
# A session dir and its bare transcript both passed (and the dir passed twice): each teammate once, in both tables.
line 150 l1 2026-11-12T09:59:00.000Z > "$project/$lead.jsonl"
both="$("$script" --teams-dir "$work/teams" "$project/$lead" "$project/$lead.jsonl" "$project/$lead/")"
if [[ "$(print -r -- "$both" | grep -c 'teammate t-alpha')" == 1 && "$(print -r -- "$both" | grep -c '^| t-alpha |')" == 1 \
    && "$(print -r -- "$both" | grep -c 'rust-critic')" == 2 && "$both" == *"| **total** | | 3 | 2k |"* ]]; then
    ok "a session dir and its bare transcript list each teammate once"
else not_ok "duplicate arguments: $both"; fi
rc=0; "$script" --teams-dir > /dev/null 2>&1 || rc=$?
if [[ "$rc" == 2 ]]; then ok "--teams-dir without a value rejected"; else not_ok "--teams-dir without a value: exit $rc"; fi

(( failures == 0 )) || { print -u2 "$failures failure(s)"; exit 1; }
print "all factory-stats cases passed"
