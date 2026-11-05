#!/usr/bin/env bash
# Fails the build when the source drifts from docs/development/AV-HYGIENE.md. Run from the repository root.
set -u
fail=0
src=(client .github)
# client/testdata and go.sum: vendored contract vectors and module hashes (base64 can spell anything).
exclude=(--exclude-dir=testdata --exclude=go.sum --exclude=av-hygiene.sh)

check() { # <description> <extended regex> [extra grep args]
  local what=$1 pattern=$2; shift 2
  local hits
  hits=$(grep -rnIE "${exclude[@]}" "$@" -e "$pattern" "${src[@]}" 2>/dev/null)
  if [ -n "$hits" ]; then
    echo "AV hygiene: $what"; echo "$hits"; echo; fail=1
  fi
}

check "no encoded PowerShell" '-[Ee]ncoded[Cc]ommand|-[Ee][Nn][Cc] |FromBase64String'
check "no execution-policy changes" 'ExecutionPolicy|Bypass'
check "no hidden shells" 'CREATE_NO_WINDOW|WindowStyle +Hidden|SW_HIDE'
check "no suspend/resume or injection primitives" 'CREATE_SUSPENDED|ResumeThread|SuspendThread|WriteProcessMemory|VirtualAllocEx|CreateRemoteThread'
check "no autostart (Run key, startup folder, autostart plugins)" 'CurrentVersion.{1,2}Run|Start Menu.{1,4}Programs.{1,4}Startup|plugin-autostart|auto-launch|RegSetValueEx'
check "no UAC elevation" 'requireAdministrator|highestAvailable'
check "no packers" '\bupx\b|UPX'
# One visible console window that the client's commands attach to (AV-HYGIENE.md): no creation flag that hides,
# detaches or adds a console.
check "the client starts children without console flags (one visible window)" 'CREATE_NEW_CONSOLE|DETACHED_PROCESS|CREATE_NO_WINDOW'
grep -q 'Resident("AgentRouter 设备")' client/cmd/agentdevice/main.go || { echo "AV hygiene: agentdevice run must keep its visible console window"; fail=1; }
# Release builds strip symbols normally (-s -w) and are not packed.
grep -q -- '-trimpath -ldflags "-s -w' .github/workflows/ci.yml || { echo "AV hygiene: client builds must use go build -trimpath -ldflags \"-s -w …\""; fail=1; }

[ $fail -eq 0 ] && echo "AV hygiene: ok"
exit $fail
