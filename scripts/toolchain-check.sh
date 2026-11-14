#!/usr/bin/env zsh
# Warn when the local stable toolchain is behind the current stable that CI installs (`dtolnay/rust-toolchain@stable`):
# a lint or a formatting rule that is new in the current stable passes the local gate and fails in CI (the wave 19
# hotfix: local 1.99.1, CI 1.100). Run it in the factory pre-flight (`just toolchain-check`).
# Usage: scripts/toolchain-check.sh
#
# Compares `rustc --version` with the stable line of `rustup check` (network; it installs nothing). A local version
# older than the current stable prints a WARN with the update command and the exit status stays 0: the lead decides
# when to update, and no script runs `rustup update`. Exit 0 with a note when rustup is missing, the network is down
# or the output is not understood (the check is advisory); 2 on a usage error.
set -uo pipefail
(( $# == 0 )) || { print -u2 -- "usage: scripts/toolchain-check.sh"; exit 2; }

command -v rustc > /dev/null 2>&1 || { print -r -- "toolchain-check: rustc not found; check skipped"; exit 0; }
local_version="$(rustc --version | sed -nE 's/^rustc ([0-9]+\.[0-9]+\.[0-9]+).*/\1/p')"
[[ -n "$local_version" ]] || { print -r -- "toolchain-check: cannot read the version from '$(rustc --version)'; check skipped"; exit 0; }
command -v rustup > /dev/null 2>&1 || { print -r -- "toolchain-check: rustup not found; check skipped (local rustc $local_version)"; exit 0; }

# `rustup check` exits 100 when an update is available. Its stable line reads
#   stable-<triple> - update available: 1.99.1 (<hash> <date>) -> 1.100.0 (<hash> <date>)
#   stable-<triple> - Up to date : 1.100.0 (<hash> <date>)
check="$(rustup check 2>&1)" || true
line="$(print -r -- "$check" | grep -E '^stable-' | head -1)"
latest=""
if [[ "$line" == *"->"* ]]; then
    latest="$(print -r -- "$line" | sed -nE 's/.*-> ([0-9]+\.[0-9]+\.[0-9]+).*/\1/p')"
elif [[ "$line" == *[Uu]p\ to\ date* ]]; then
    latest="$(print -r -- "$line" | sed -nE 's/.*[Uu]p to date *: *([0-9]+\.[0-9]+\.[0-9]+).*/\1/p')"
fi
if [[ -z "$latest" ]]; then
    print -r -- "toolchain-check: no stable version in 'rustup check' (offline?); check skipped (local rustc $local_version)"
    exit 0
fi
# Version order, not string order: 1.100.0 is newer than 1.99.1.
newest="$(printf '%s\n%s\n' "$local_version" "$latest" | sort -t. -k1,1n -k2,2n -k3,3n | tail -1)"
if [[ "$local_version" != "$latest" && "$newest" == "$latest" ]]; then
    print -P "%F{yellow}WARN%f local rustc $local_version is behind the current stable $latest that CI uses: run \`rustup update stable\` before the gate"
else
    print -P "%F{green}PASS%f local rustc $local_version is the current stable ($latest)"
fi
exit 0
