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
SetupIconFile=..\crates\timelens-app\assets\brand\timelens.ico
; Everything that runs elevated (collector, uninstaller, maintenance scripts) lives
; under Common Files, whose ancestors only administrators can rename. The chosen
; installation directory then holds only ordinary-privilege programs and may be
; any folder on a fixed local disk.
UninstallFilesDir={commonpf64}\{#ProductName}
CloseApplications=yes
RestartApplications=no
SetupLogging=yes

[Files]
Source: "..\target\release\timelens.exe"; DestDir: "{app}"; DestName: "Timelens.exe"; Flags: ignoreversion
Source: "..\target\release\timelens-collector.exe"; DestDir: "{commonpf64}\{#ProductName}"; DestName: "Timelens.Collector.exe"; Flags: ignoreversion
Source: "..\target\release\timelens-ai-worker.exe"; DestDir: "{app}"; DestName: "Timelens.AI.exe"; Flags: ignoreversion
Source: "maintenance.ps1"; DestDir: "{commonpf64}\{#ProductName}\internal"; Flags: ignoreversion
Source: "path-safety.ps1"; DestDir: "{commonpf64}\{#ProductName}\internal"; Flags: ignoreversion
Source: "register-tasks.ps1"; DestDir: "{commonpf64}\{#ProductName}\internal"; Flags: ignoreversion
Source: "unregister-tasks.ps1"; DestDir: "{commonpf64}\{#ProductName}\internal"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#ProductName}"; Filename: "{app}\Timelens.exe"

#if DataDirectory == ""
; The core already runs in the background from its logon task; this only shows
; its window, so a first-time user sees the app instead of a silent tray icon.
[Run]
Filename: "{app}\Timelens.exe"; Description: "打开 Timelens"; Flags: postinstall nowait skipifsilent runasoriginaluser
#endif

[Code]
const
  DriveFixed = 3;

var
  TaskRegistrationExitCode: Integer;
  DeleteDataOnUninstall: Boolean;
#if DataDirectory == ""
  DataModePage: TInputOptionWizardPage;
  DataDirPage: TInputDirWizardPage;
#endif

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

function ElevatedDir: string;
begin
  Result := ExpandConstant('{commonpf64}\{#ProductName}');
end;

#if DataDirectory == ""
// New installs choose where the signed-in user's dataset lives. An upgrade keeps
// the existing data where it is; moving it is done from 设置 → 数据.
procedure InitializeWizard;
begin
  DataModePage := CreateInputOptionPage(wpSelectDir,
    '数据存放位置', '选择 Timelens 保存活动记录、快照和报告的位置。',
    '数据加密保存在这台电脑上。如果这个 Windows 用户已有 Timelens 数据，会继续使用原来的位置。以后也可以在 设置 → 数据 中迁移。',
    True, False);
  DataModePage.Add('当前用户的应用数据目录（AppData\Local\Timelens，推荐）');
  DataModePage.Add('自定义位置');
  DataModePage.SelectedValueIndex := 0;
  DataDirPage := CreateInputDirPage(DataModePage.ID,
    '自定义数据位置', '选择一个空文件夹保存数据。',
    '安装目录里只能使用其中的 Data 文件夹。安装器会创建该文件夹，并授予当前用户写入权限。',
    False, '');
  DataDirPage.Add('数据文件夹：');
end;

function UseCustomDataDirectory: Boolean;
begin
  Result := (WizardForm.PrevAppDir = '') and (DataModePage.SelectedValueIndex = 1);
end;

function ShouldSkipPage(PageID: Integer): Boolean;
begin
  Result := ((PageID = DataModePage.ID) and (WizardForm.PrevAppDir <> '')) or
    ((PageID = DataDirPage.ID) and not UseCustomDataDirectory);
end;

function IsSameOrInside(const Path, Parent: string): Boolean;
var
  P, Q: string;
begin
  P := Lowercase(RemoveBackslashUnlessRoot(Path));
  Q := Lowercase(RemoveBackslashUnlessRoot(Parent));
  Result := (P = Q) or (Pos(AddBackslash(Q), P) = 1);
end;

function IsEmptyDirectory(const Path: string): Boolean;
var
  Find: TFindRec;
begin
  Result := True;
  if FindFirst(AddBackslash(Path) + '*', Find) then
  try
    repeat
      if (Find.Name <> '.') and (Find.Name <> '..') then
      begin
        Result := False;
        Break;
      end;
    until not FindNext(Find);
  finally
    FindClose(Find);
  end;
end;

function DataDirectoryValue: string;
begin
  Result := RemoveBackslashUnlessRoot(ExpandFileName(Trim(DataDirPage.Values[0])));
end;

procedure CurPageChanged(CurPageID: Integer);
begin
  if (CurPageID = DataDirPage.ID) and (Trim(DataDirPage.Values[0]) = '') then
    if IsSameOrInside(WizardDirValue, ElevatedDir) then
      DataDirPage.Values[0] := ExtractFileDrive(WizardDirValue) + '\TimelensData'
    else
      DataDirPage.Values[0] := AddBackslash(WizardDirValue) + 'Data';
end;

function DataDirectoryProblem: string;
var
  Path, AppDir: string;
begin
  Result := '';
  Path := DataDirectoryValue;
  AppDir := RemoveBackslashUnlessRoot(WizardDirValue);
  if (Length(Path) <= 3) or (Pos('"', Path) > 0) or not IsFixedLocalPath(Path) then
    Result := '数据文件夹必须位于本地固定磁盘，且不能是磁盘根目录。'
  else if (Pos('\onedrive', Lowercase(Path)) > 0) or (Pos('\dropbox', Lowercase(Path)) > 0) or
    (Pos('\icloud drive', Lowercase(Path)) > 0) or (Pos('\google drive', Lowercase(Path)) > 0) then
    Result := '数据文件夹不能放在云同步目录里。'
  else if IsSameOrInside(Path, ElevatedDir) then
    Result := '数据文件夹不能放在受保护的采集器目录（' + ElevatedDir + '）里。'
  else if IsSameOrInside(Path, AppDir) and (CompareText(Path, AddBackslash(AppDir) + 'Data') <> 0) then
    Result := '安装目录里只能使用 ' + AddBackslash(AppDir) + 'Data 文件夹。'
  else if FileExists(Path) then
    Result := '所选路径是一个文件，请选择文件夹。'
  else if DirExists(Path) and not IsEmptyDirectory(Path) then
    Result := '请选择空文件夹或新文件夹。Timelens 不会合并已有文件。';
end;
#endif

function NextButtonClick(CurPageID: Integer): Boolean;
#if DataDirectory == ""
var
  Problem: string;
#endif
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
#if DataDirectory == ""
  if CurPageID = DataDirPage.ID then
  begin
    Problem := DataDirectoryProblem;
    if Problem <> '' then
    begin
      MsgBox(Problem, mbError, MB_OK);
      Result := False;
    end;
  end;
#endif
end;

function PowerShellPath: string;
begin
  Result := ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe');
end;

function MaintenanceArguments: string;
begin
  Result := ' -TaskPath \{#TaskFolder}\ -ElevatedDir ' + AddQuotes(ElevatedDir);
  #if DataDirectory != ""
    Result := Result + ' -DataDirectory ' + AddQuotes('{#DataDirectory}');
  #endif
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Parameters, ErrorFile, Reason: string;
  Lines: TArrayOfString;
  ResultCode, Index: Integer;
begin
  Result := '';
  ExtractTemporaryFile('maintenance.ps1');
  ExtractTemporaryFile('path-safety.ps1');
  ErrorFile := ExpandConstant('{tmp}\prepare-error.txt');
  Parameters := '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File ' +
    AddQuotes(ExpandConstant('{tmp}\maintenance.ps1')) + ' -Mode PrepareUpgrade -InstallDir ' +
    AddQuotes(ExpandConstant('{app}')) + MaintenanceArguments + ' -ErrorFile ' + AddQuotes(ErrorFile);
  if not Exec(PowerShellPath, Parameters, '', SW_HIDE, ewWaitUntilTerminated, ResultCode) or (ResultCode <> 0) then
  begin
    Reason := '';
    if LoadStringsFromFile(ErrorFile, Lines) then
      for Index := 0 to GetArrayLength(Lines) - 1 do
        Reason := Trim(Reason + ' ' + Lines[Index]);
    Log('PrepareUpgrade failed (exit code ' + IntToStr(ResultCode) + '): ' + Reason);
    if Reason = '' then
      Result := '无法安全停止已有 Timelens。安装文件尚未替换，请关闭应用后重试。'
    else
      Result := '安装前检查未通过，安装文件尚未替换：' + #13#10#13#10 + Reason;
  end;
end;

procedure RegisterTimelensTasks;
var
  Parameters: string;
  ResultCode: Integer;
begin
  Parameters := '-NoProfile -NonInteractive -ExecutionPolicy Bypass -File ' +
    AddQuotes(ElevatedDir + '\internal\register-tasks.ps1') +
    ' -InstallDir ' + AddQuotes(ExpandConstant('{app}')) + ' -StartNow' + MaintenanceArguments;
#if DataDirectory == ""
  if UseCustomDataDirectory then
    Parameters := Parameters + ' -UserDataDirectory ' + AddQuotes(DataDirectoryValue);
#endif
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
      AddQuotes(ElevatedDir + '\internal\unregister-tasks.ps1') +
      ' -InstallDir ' + AddQuotes(ExpandConstant('{app}')) + MaintenanceArguments;
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
