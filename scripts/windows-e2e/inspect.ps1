# What the installer left in the account's own profile (windows-e2e.yml, install from a release): run AS the test
# account (its HKCU, its Task Scheduler view, its folders), writes one JSON object to $Out. Read-only.
param([Parameter(Mandatory)][string]$Out)
$app = Join-Path $env:LOCALAPPDATA 'Programs\AgentDevice'
$state = Join-Path $env:APPDATA 'AgentDevice'
$key = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\{91B09E54-A95A-4F8A-8DCF-39A38D3649AE}_is1'
$lnk = Join-Path ([Environment]::GetFolderPath('Programs')) 'AgentRouter 设备.lnk'
$entry = Get-ItemProperty -Path $key -ErrorAction SilentlyContinue
$env_ = $null
if (Test-Path "$app\environment.json") { $env_ = (Get-Content "$app\environment.json" -Raw | ConvertFrom-Json).environment }
$task = & schtasks.exe /Query /TN AgentDevice /FO CSV /NH 2>$null
$taskOk = ($LASTEXITCODE -eq 0) -and [bool]$task
$procs = @(Get-CimInstance Win32_Process -Filter "Name='agentdevice.exe'" | Where-Object { $_.ExecutablePath -like "$app\*" } |
  Where-Object { (Invoke-CimMethod -InputObject $_ -MethodName GetOwner).User -eq $env:USERNAME } | ForEach-Object { $_.ProcessId })
$result = [ordered]@{
  user               = "$env:USERDOMAIN\$env:USERNAME"
  appFolder          = (Test-Path $app)
  program            = (Test-Path "$app\agentdevice.exe")
  environment        = $env_
  uninstaller        = (Test-Path "$app\unins000.exe")
  startMenuShortcut  = (Test-Path -LiteralPath $lnk)
  uninstallEntry     = [bool]$entry
  displayName        = $entry.DisplayName
  displayVersion     = $entry.DisplayVersion
  logonTask          = $taskOk
  stateFolder        = (Test-Path $state)
  stateFiles         = @(if (Test-Path $state) { Get-ChildItem $state -Name })
  clientProcesses    = $procs
  uninstallResult    = $(if (Test-Path "$env:TEMP\agentdevice-uninstall.txt") { Get-Content "$env:TEMP\agentdevice-uninstall.txt" -Raw } else { $null })
}
$result | ConvertTo-Json -Depth 3 | Set-Content -Path $Out -Encoding utf8
