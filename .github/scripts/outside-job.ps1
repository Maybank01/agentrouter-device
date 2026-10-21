# Runs a command outside the GitHub Actions step's job object (device #66) and returns its output and exit code.
#
# Root cause of #66: the hosted runner puts every step in a Windows job object whose limits do not allow breakaway
# (JOB_OBJECT_LIMIT_BREAKAWAY_OK unset, limit flags 0x0). srt-win's broker starts the sandbox account's runner with
# CreateProcessWithLogonW + CREATE_BREAKAWAY_FROM_JOB, which then fails with ERROR_ACCESS_DENIED (5) — not a
# Secondary Logon problem. Measured on windows-latest 2026-10-21: inside the step, breakaway → error 5, every other
# flag combination works; from a process created by WMI (Win32_Process.Create, not in that job) all of them work.
# A person's PC starts `agentrouter` from an ordinary shell, which is not in such a job: this is the faithful test.
# -Command: native commands chained with && (its last exit code is the result; no `exit` inside).
param([Parameter(Mandatory)] [string] $Command)
$ErrorActionPreference = 'Stop'
$dir = Join-Path $env:RUNNER_TEMP ("outside-job-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $dir | Out-Null
$log = Join-Path $dir 'output.txt'; $code = Join-Path $dir 'exit.txt'; $script = Join-Path $dir 'run.ps1'
# The WMI-created process starts with the machine's environment: the step's (PATH with cargo, test switches) is
# carried over, quoted for PowerShell.
$lines = Get-ChildItem env: | ForEach-Object { "`${env:$($_.Name)} = '$($_.Value.Replace("'", "''"))'" }
$lines += "Set-Location '$($PWD.Path.Replace("'", "''"))'"
$lines += "& { $Command } *> '$log'"
$lines += "Set-Content -Path '$code' -Value `$LASTEXITCODE"
Set-Content -Path $script -Value $lines
$r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = "pwsh -NoProfile -NonInteractive -File `"$script`""; CurrentDirectory = $PWD.Path }
if ($r.ReturnValue -ne 0) { throw "Win32_Process.Create failed: $($r.ReturnValue)" }
while (Get-Process -Id $r.ProcessId -ErrorAction SilentlyContinue) { Start-Sleep -Seconds 2 }
if (Test-Path $log) { Get-Content $log }
$exit = if (Test-Path $code) { [int](Get-Content $code -Raw).Trim() } else { 1 }
Remove-Item -Recurse -Force $dir
exit $exit
