; The per-user Windows installer of the device client (release/README.md). Inno Setup 6 (preinstalled on GitHub's
; Windows runners), compiled by .github/workflows/release.yml:
;   ISCC.exe /DAppVersion=0.1.0 /DSetupName=agentdevice-setup-dev /DPayload=<folder> /DOutputDir=<folder> agentdevice.iss
; <Payload> holds agentdevice.exe (the release build, no endpoints built in) and environment.json (one environment's
; file from release/environments/). No administrator rights: everything goes into the person's own profile.
#ifndef AppVersion
  #error AppVersion is required (/DAppVersion=X.Y.Z)
#endif
#ifndef SetupName
  #error SetupName is required (/DSetupName=agentdevice-setup-dev)
#endif
#ifndef Payload
  #error Payload is required (/DPayload=<folder with agentdevice.exe and environment.json>)
#endif
#ifndef OutputDir
  #define OutputDir "."
#endif

[Setup]
; One application per computer account: a newer installer replaces the program in place and keeps the login.
AppId={{91B09E54-A95A-4F8A-8DCF-39A38D3649AE}
AppName=AgentRouter 设备
AppVersion={#AppVersion}
AppVerName=AgentRouter 设备 {#AppVersion}
AppPublisher=AgentRouter
AppPublisherURL=https://github.com/agentrouter-top/AgentDevice
AppSupportURL=https://github.com/agentrouter-top/AgentDevice/issues
VersionInfoVersion={#AppVersion}.0
VersionInfoProductName=AgentRouter 设备
VersionInfoDescription=AgentRouter 设备 安装程序
VersionInfoCompany=AgentRouter
; Per user, never elevated: %LOCALAPPDATA%\Programs\AgentDevice, the uninstall entry under HKCU.
PrivilegesRequired=lowest
DefaultDirName={localappdata}\Programs\AgentDevice
DisableDirPage=yes
DisableProgramGroupPage=yes
DisableWelcomePage=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#OutputDir}
OutputBaseFilename={#SetupName}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ShowLanguageDialog=no
; A running client holds the program file: `agentdevice stop` (PrepareToInstall) ends it first; Restart Manager is
; the fallback for a client too old to know `stop`.
CloseApplications=force
RestartApplications=no
UninstallDisplayName=AgentRouter 设备
UninstallDisplayIcon={app}\agentdevice.exe

[Languages]
Name: "zh"; MessagesFile: "ChineseSimplified.isl"

[CustomMessages]
zh.AutostartTask=登录电脑后自动启动
zh.AutostartStatus=正在打开自动启动…
zh.StartNow=现在启动并登录

[Tasks]
; Autostart only because the person leaves this box ticked (docs/development/AV-HYGIENE.md): the Task Scheduler logon
; task of `agentdevice autostart on`, no Run key.
Name: "autostart"; Description: "{cm:AutostartTask}"

[Files]
Source: "{#Payload}\agentdevice.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Payload}\environment.json"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\AgentRouter 设备"; Filename: "{app}\agentdevice.exe"; WorkingDir: "{app}"

[Run]
Filename: "{app}\agentdevice.exe"; Parameters: "autostart on --no-start"; WorkingDir: "{app}"; Tasks: autostart; Flags: waituntilterminated; StatusMsg: "{cm:AutostartStatus}"
; The resident window 「AgentRouter 设备」 opens and the browser asks to confirm the login (also after a silent install).
Filename: "{app}\agentdevice.exe"; WorkingDir: "{app}"; Description: "{cm:StartNow}"; Flags: postinstall nowait

[UninstallRun]
; Before the files go: autostart off, the running client ended, the computer removed from the account, its folder
; (%APPDATA%\AgentDevice: login, keys, logs) deleted. The result is in %TEMP%\agentdevice-uninstall.txt.
Filename: "{app}\agentdevice.exe"; Parameters: "uninstall"; WorkingDir: "{app}"; RunOnceId: "AgentDeviceUninstall"; Flags: waituntilterminated

[Code]
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Exe: String;
  Code: Integer;
begin
  Result := '';
  Exe := ExpandConstant('{app}\agentdevice.exe');
  if FileExists(Exe) then
    Exec(Exe, 'stop', ExpandConstant('{app}'), SW_SHOWNORMAL, ewWaitUntilTerminated, Code);
end;
