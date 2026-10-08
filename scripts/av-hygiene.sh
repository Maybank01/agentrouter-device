#!/usr/bin/env bash
# Fails the build when the source drifts from docs/AV-HYGIENE.md. Run from the repository root.
set -u
fail=0
src=(crates app/src-tauri .github Cargo.toml)
exclude=(--exclude-dir=target --exclude-dir=gen --exclude=av-hygiene.sh)

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
check "nothing runs from the temp folder" 'std::env::temp_dir|%TEMP%' --include=*.rs --exclude-dir=tests

# The release profile strips symbols normally and is not obfuscated or packed.
grep -q '^strip = true' Cargo.toml || { echo "AV hygiene: release profile must keep strip = true"; fail=1; }
# The command-line manifest runs as the invoking user.
grep -q 'level="asInvoker"' crates/connector-cli/res/app.manifest || { echo "AV hygiene: manifest must be asInvoker"; fail=1; }

[ $fail -eq 0 ] && echo "AV hygiene: ok"
exit $fail
