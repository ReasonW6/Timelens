#define AppVersion "0.1.0"
#ifndef ProductName
  #define ProductName "Timelens"
#endif
#ifndef AppIdValue
  #define AppIdValue "{{4DC7714B-2E2A-4E4E-A86F-BF9DB11A6653}"
#endif
#ifndef TaskFolder
  #define TaskFolder "Timelens"
#endif
#ifndef DataDirectory
  #define DataDirectory ""
#endif

[Setup]
AppId={#AppIdValue}
AppName={#ProductName}
AppVersion={#AppVersion}
AppPublisher=ReasonW6
DefaultDirName={autopf}\{#ProductName}
DefaultGroupName={#ProductName}
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
Source: "..\target\release\timelens-ai-worker.exe"; DestDir: "{app}"; DestName: "Timelens.AI.exe"; Flags: ignoreversion
Source: "maintenance.ps1"; DestDir: "{app}\internal"; Flags: ignoreversion
Source: "path-safety.ps1"; DestDir: "{app}\internal"; Flags: ignoreversion
Source: "register-tasks.ps1"; DestDir: "{app}\internal"; Flags: ignoreversion
Source: "unregister-tasks.ps1"; DestDir: "{app}\internal"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#ProductName}"; Filename: "{app}\Timelens.exe"

[Code]
const
  DriveFixed = 3;

var
  TaskRegistrationExitCode: Integer;
  DeleteDataOnUninstall: Boolean;

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

function MaintenanceArguments: string;
begin
  Result := ' -TaskPath \{#TaskFolder}\';
  #if DataDirectory != ""
    Result := Result + ' -DataDirectory ' + AddQuotes('{#DataDirectory}');
  #endif
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Parameters: string;
  ResultCode: Integer;
begin
  Result := '';
  ExtractTemporaryFile('maintenance.ps1');
  ExtractTemporaryFile('path-safety.ps1');
  Parameters := '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File ' +
    AddQuotes(ExpandConstant('{tmp}\maintenance.ps1')) + ' -Mode PrepareUpgrade -InstallDir ' +
    AddQuotes(ExpandConstant('{app}')) + MaintenanceArguments;
  if not Exec(PowerShellPath, Parameters, '', SW_HIDE, ewWaitUntilTerminated, ResultCode) or (ResultCode <> 0) then
    Result := '无法安全停止已有 Timelens。安装文件尚未替换，请关闭应用后重试。';
end;

procedure RegisterTimelensTasks;
var
  Parameters: string;
  ResultCode: Integer;
begin
  Parameters := '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File ' +
    AddQuotes(ExpandConstant('{app}\internal\register-tasks.ps1')) +
    ' -InstallDir ' + AddQuotes(ExpandConstant('{app}')) + ' -StartNow' + MaintenanceArguments;
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
      AddQuotes(ExpandConstant('{app}\internal\unregister-tasks.ps1')) + MaintenanceArguments;
    if DeleteDataOnUninstall then Parameters := Parameters + ' -DataMode Delete'
    else Parameters := Parameters + ' -DataMode Keep';
    if not Exec(PowerShellPath, Parameters, '', SW_HIDE, ewWaitUntilTerminated, ResultCode) or (ResultCode <> 0) then
      RaiseException('数据或任务清理未完成，卸载已停止。请检查数据位置后重试，应用文件保留用于恢复。');
  end;
end;

function InitializeUninstall: Boolean;
var
  Choice: Integer;
begin
  DeleteDataOnUninstall := ExpandConstant('{param:DATA|delete}') <> 'keep';
  Result := True;
  if not UninstallSilent then
  begin
    Choice := TaskDialogMsgBox('卸载后如何处理本地数据？',
      '外部备份与导出文件不受影响。删除无法撤销，不创建隐藏副本。', mbConfirmation,
      MB_YESNOCANCEL, ['删除全部本地数据及凭据', '保留加密数据'], 0);
    Result := (Choice = IDYES) or (Choice = IDNO);
    DeleteDataOnUninstall := Choice = IDYES;
  end;
end;
