#define AppVersion "0.1.0"

[Setup]
AppId={{4DC7714B-2E2A-4E4E-A86F-BF9DB11A6653}
AppName=Timelens
AppVersion={#AppVersion}
AppPublisher=ReasonW6
DefaultDirName={autopf}\Timelens
DefaultGroupName=Timelens
DisableProgramGroupPage=yes
OutputDir=..\dist
OutputBaseFilename=Timelens-{#AppVersion}-x64-setup
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
PrivilegesRequired=admin
SetupArchitecture=x64
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.22000
AllowUNCPath=no
UninstallDisplayIcon={app}\Timelens.exe
CloseApplications=yes
RestartApplications=no
SetupLogging=yes

[Files]
Source: "..\target\release\timelens.exe"; DestDir: "{app}"; DestName: "Timelens.exe"; Flags: ignoreversion
Source: "..\target\release\timelens-collector.exe"; DestDir: "{app}"; DestName: "Timelens.Collector.exe"; Flags: ignoreversion
Source: "register-tasks.ps1"; DestDir: "{app}\internal"; Flags: ignoreversion
Source: "unregister-tasks.ps1"; DestDir: "{app}\internal"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Timelens"; Filename: "{app}\Timelens.exe"

[Code]
const
  DriveFixed = 3;

var
  TaskRegistrationExitCode: Integer;

function GetDriveType(RootPathName: string): Cardinal;
  external 'GetDriveTypeW@kernel32.dll stdcall';

function IsFixedLocalPath(const Path: string): Boolean;
var
  Root: string;
begin
  Root := ExtractFileDrive(ExpandFileName(Path));
  Result := (Root <> '') and (Pos('\\', Root) <> 1) and
    (GetDriveType(AddBackslash(Root)) = DriveFixed);
end;

function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if (CurPageID = wpSelectDir) and not IsFixedLocalPath(WizardDirValue) then
  begin
    MsgBox(
      'Timelens 必须安装到本地固定磁盘。网络盘、可移动盘和 UNC 路径不受支持。',
      mbError,
      MB_OK
    );
    Result := False;
  end;
end;

function PowerShellPath: string;
begin
  Result := ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe');
end;

procedure RegisterTimelensTasks;
var
  Parameters: string;
  ResultCode: Integer;
begin
  Parameters := '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File ' +
    AddQuotes(ExpandConstant('{app}\internal\register-tasks.ps1')) +
    ' -InstallDir ' + AddQuotes(ExpandConstant('{app}')) + ' -StartNow';
  if not Exec(PowerShellPath, Parameters, '', SW_HIDE, ewWaitUntilTerminated, ResultCode) or
     (ResultCode <> 0) then
  begin
    TaskRegistrationExitCode := 9;
    RaiseException(Format('无法建立 Timelens 权限边界与登录任务（退出码 %d）。', [ResultCode]));
  end;
end;

function GetCustomSetupExitCode: Integer;
begin
  Result := TaskRegistrationExitCode;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    RegisterTimelensTasks;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Parameters: string;
  ResultCode: Integer;
begin
  if CurUninstallStep = usUninstall then
  begin
    Parameters := '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File ' +
      AddQuotes(ExpandConstant('{app}\internal\unregister-tasks.ps1'));
    if not Exec(PowerShellPath, Parameters, '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
      Log('Unable to start task cleanup helper.')
    else if ResultCode <> 0 then
      Log(Format('Task cleanup helper returned exit code %d.', [ResultCode]));
  end;
end;
