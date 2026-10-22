; The version comes from the release build's version resource, which build.rs
; fills from the workspace version in Cargo.toml.
#define CoreExe AddBackslash(SourcePath) + "..\target\release\timelens.exe"
#define AppVersion GetFileProductVersionString(CoreExe)
#if AppVersion == ""
  #error Build the workspace in release mode first: target\release\timelens.exe has no product version.
#endif
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
VersionInfoVersion={#GetVersionNumbersString(CoreExe)}
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
; under Program Files ({commonpf64}), whose ancestors only administrators can rename. The chosen
; installation directory then holds only ordinary-privilege programs and may be
; any folder on a fixed local disk.
UninstallFilesDir={commonpf64}\{#ProductName}
CloseApplications=yes
RestartApplications=no
SetupLogging=yes
; Setup follows the Windows display language like the app does, falling back to
; English, and asks only when it cannot match one.
ShowLanguageDialog=auto

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "chinesesimplified"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"
Name: "chinesetraditional"; MessagesFile: "compiler:Languages\ChineseTraditional.isl"
Name: "japanese"; MessagesFile: "compiler:Languages\Japanese.isl"
Name: "korean"; MessagesFile: "compiler:Languages\Korean.isl"

[CustomMessages]
english.DataModeCaption=Data location
english.DataModeDescription=Choose where Timelens keeps activity records and snapshots.
english.DataModeSubCaption=Data is stored encrypted on this PC. If this Windows user already has Timelens data, its current location is kept. You can move it later in Settings → Data & storage.
english.DataModeInstallDir=With the installation (the TimelensData folder in the installation folder, recommended)
english.DataModeAppData=This user's app data folder (AppData\Local\Timelens)
english.DataModeCustom=Custom location
english.DataDirCaption=Custom data location
english.DataDirDescription=Choose an empty folder for the data.
english.DataDirSubCaption=Inside the installation folder, only its TimelensData folder can be used. Setup creates the folder and lets the current user write to it.
english.DataDirPrompt=Data folder:
english.DataDirNotFixed=The data folder must be on a fixed local disk and cannot be the root of a drive.
english.DataDirCloud=The data folder cannot be inside a cloud-synced folder.
english.DataDirElevated=The data folder cannot be inside the protected collector folder (%1).
english.DataDirInsideApp=Inside the installation folder, only the %1 folder can be used.
english.DataDirIsFile=The selected path is a file. Choose a folder.
english.DataDirNotEmpty=%1 already contains files. Choose an empty or new folder; Timelens does not merge existing files. If this is data you kept earlier, choose the app data folder to keep using its original location.
english.InstallDirNotFixed=Timelens must be installed on a fixed local disk. Network drives, removable drives and UNC paths are not supported.
english.PrepareStopFailed=The running Timelens could not be stopped safely. No files have been replaced. Close the app and try again.
english.PrepareCheckFailed=The pre-installation check failed. No files have been replaced:
english.TaskRegistrationFailed=Could not set up the Timelens permission boundary and logon tasks (exit code %1).
english.UninstallCleanupFailed=Data or task cleanup did not finish, so uninstallation stopped. Check the data location and try again; the app files are kept for recovery.
english.UninstallDataQuestion=What should happen to the local data after uninstalling?
english.UninstallDataNote=External backups and exports are not affected. Deleting cannot be undone, and no hidden copy is kept.
english.UninstallDeleteData=Delete all local data and credentials
english.UninstallKeepData=Keep the encrypted data

chinesesimplified.DataModeCaption=数据存放位置
chinesesimplified.DataModeDescription=选择 Timelens 保存活动记录和快照的位置。
chinesesimplified.DataModeSubCaption=数据加密保存在这台电脑上。如果这个 Windows 用户已有 Timelens 数据，会继续使用原来的位置。以后也可以在 设置 → 数据与存储 中迁移。
chinesesimplified.DataModeInstallDir=跟随安装目录（安装目录下的 TimelensData 文件夹，推荐）
chinesesimplified.DataModeAppData=当前用户的应用数据目录（AppData\Local\Timelens）
chinesesimplified.DataModeCustom=自定义位置
chinesesimplified.DataDirCaption=自定义数据位置
chinesesimplified.DataDirDescription=选择一个空文件夹保存数据。
chinesesimplified.DataDirSubCaption=安装目录里只能使用其中的 TimelensData 文件夹。安装器会创建该文件夹，并授予当前用户写入权限。
chinesesimplified.DataDirPrompt=数据文件夹：
chinesesimplified.DataDirNotFixed=数据文件夹必须位于本地固定磁盘，且不能是磁盘根目录。
chinesesimplified.DataDirCloud=数据文件夹不能放在云同步目录里。
chinesesimplified.DataDirElevated=数据文件夹不能放在受保护的采集器目录（%1）里。
chinesesimplified.DataDirInsideApp=安装目录里只能使用 %1 文件夹。
chinesesimplified.DataDirIsFile=所选路径是一个文件，请选择文件夹。
chinesesimplified.DataDirNotEmpty=%1 里已有文件。请选择空文件夹或新文件夹，Timelens 不会合并已有文件。如果这是以前保留的数据，选择应用数据目录即可继续使用原来的位置。
chinesesimplified.InstallDirNotFixed=Timelens 必须安装到本地固定磁盘。网络盘、可移动盘和 UNC 路径不受支持。
chinesesimplified.PrepareStopFailed=无法安全停止已有 Timelens。安装文件尚未替换，请关闭应用后重试。
chinesesimplified.PrepareCheckFailed=安装前检查未通过，安装文件尚未替换：
chinesesimplified.TaskRegistrationFailed=无法建立 Timelens 权限边界与登录任务（退出码 %1）。
chinesesimplified.UninstallCleanupFailed=数据或任务清理未完成，卸载已停止。请检查数据位置后重试，应用文件保留用于恢复。
chinesesimplified.UninstallDataQuestion=卸载后如何处理本地数据？
chinesesimplified.UninstallDataNote=外部备份与导出文件不受影响。删除无法撤销，不创建隐藏副本。
chinesesimplified.UninstallDeleteData=删除全部本地数据及凭据
chinesesimplified.UninstallKeepData=保留加密数据

chinesetraditional.DataModeCaption=資料存放位置
chinesetraditional.DataModeDescription=選擇 Timelens 儲存活動記錄和快照的位置。
chinesetraditional.DataModeSubCaption=資料會加密儲存在這台電腦上。如果這個 Windows 使用者已有 Timelens 資料，會繼續使用原來的位置。之後也可以在 設定 → 資料與儲存 中搬移。
chinesetraditional.DataModeInstallDir=跟隨安裝目錄（安裝目錄下的 TimelensData 資料夾，建議）
chinesetraditional.DataModeAppData=目前使用者的應用程式資料目錄（AppData\Local\Timelens）
chinesetraditional.DataModeCustom=自訂位置
chinesetraditional.DataDirCaption=自訂資料位置
chinesetraditional.DataDirDescription=選擇一個空資料夾儲存資料。
chinesetraditional.DataDirSubCaption=安裝目錄裡只能使用其中的 TimelensData 資料夾。安裝程式會建立該資料夾，並授予目前使用者寫入權限。
chinesetraditional.DataDirPrompt=資料存放資料夾：
chinesetraditional.DataDirNotFixed=資料存放資料夾必須位於本機固定磁碟，且不能是磁碟根目錄。
chinesetraditional.DataDirCloud=資料存放資料夾不能放在雲端同步目錄裡。
chinesetraditional.DataDirElevated=資料存放資料夾不能放在受保護的採集器目錄（%1）裡。
chinesetraditional.DataDirInsideApp=安裝目錄裡只能使用 %1 資料夾。
chinesetraditional.DataDirIsFile=所選路徑是一個檔案，請選擇資料夾。
chinesetraditional.DataDirNotEmpty=%1 裡已有檔案。請選擇空資料夾或新資料夾，Timelens 不會合併已有檔案。如果這是以前保留的資料，選擇應用程式資料目錄即可繼續使用原來的位置。
chinesetraditional.InstallDirNotFixed=Timelens 必須安裝到本機固定磁碟。不支援網路磁碟機、卸除式磁碟機和 UNC 路徑。
chinesetraditional.PrepareStopFailed=無法安全停止已有的 Timelens。安裝檔案尚未替換，請關閉應用程式後重試。
chinesetraditional.PrepareCheckFailed=安裝前檢查未通過，安裝檔案尚未替換：
chinesetraditional.TaskRegistrationFailed=無法建立 Timelens 權限邊界與登入工作（結束代碼 %1）。
chinesetraditional.UninstallCleanupFailed=資料或工作清理未完成，解除安裝已停止。請檢查資料位置後重試，應用程式檔案保留用於復原。
chinesetraditional.UninstallDataQuestion=解除安裝後如何處理本機資料？
chinesetraditional.UninstallDataNote=外部備份與匯出檔案不受影響。刪除無法復原，也不會建立隱藏副本。
chinesetraditional.UninstallDeleteData=刪除全部本機資料及憑證
chinesetraditional.UninstallKeepData=保留加密資料

japanese.DataModeCaption=データの保存場所
japanese.DataModeDescription=Timelens がアクティビティの記録とスナップショットを保存する場所を選択してください。
japanese.DataModeSubCaption=データはこの PC に暗号化して保存されます。この Windows ユーザーに既存の Timelens データがある場合は、元の場所をそのまま使います。あとから［設定］→［データとストレージ］で移動することもできます。
japanese.DataModeInstallDir=インストール先に合わせる（インストール フォルダー内の TimelensData フォルダー、推奨）
japanese.DataModeAppData=現在のユーザーのアプリ データ フォルダー（AppData\Local\Timelens）
japanese.DataModeCustom=任意の場所
japanese.DataDirCaption=データの保存場所を指定
japanese.DataDirDescription=データを保存する空のフォルダーを選択してください。
japanese.DataDirSubCaption=インストール フォルダー内では、その中の TimelensData フォルダーだけを使用できます。セットアップはこのフォルダーを作成し、現在のユーザーに書き込み権限を付与します。
japanese.DataDirPrompt=データ フォルダー:
japanese.DataDirNotFixed=データ フォルダーはローカルの固定ディスク上にあり、ドライブのルート以外である必要があります。
japanese.DataDirCloud=データ フォルダーをクラウド同期フォルダー内に置くことはできません。
japanese.DataDirElevated=データ フォルダーを保護されたコレクター フォルダー（%1）内に置くことはできません。
japanese.DataDirInsideApp=インストール フォルダー内では %1 フォルダーだけを使用できます。
japanese.DataDirIsFile=選択したパスはファイルです。フォルダーを選択してください。
japanese.DataDirNotEmpty=%1 にはすでにファイルがあります。空のフォルダーか新しいフォルダーを選択してください。Timelens は既存のファイルを統合しません。以前に残したデータの場合は、アプリ データ フォルダーを選択すると元の場所をそのまま使えます。
japanese.InstallDirNotFixed=Timelens はローカルの固定ディスクにインストールする必要があります。ネットワーク ドライブ、リムーバブル ドライブ、UNC パスはサポートされていません。
japanese.PrepareStopFailed=実行中の Timelens を安全に停止できませんでした。ファイルはまだ置き換えられていません。アプリを終了してから再試行してください。
japanese.PrepareCheckFailed=インストール前のチェックに失敗しました。ファイルはまだ置き換えられていません:
japanese.TaskRegistrationFailed=Timelens の権限境界とログオン タスクを設定できませんでした（終了コード %1）。
japanese.UninstallCleanupFailed=データまたはタスクのクリーンアップが完了しなかったため、アンインストールを中止しました。データの場所を確認して再試行してください。アプリのファイルは復旧用に残してあります。
japanese.UninstallDataQuestion=アンインストール後、ローカル データをどうしますか?
japanese.UninstallDataNote=外部のバックアップとエクスポートしたファイルには影響しません。削除は元に戻せず、隠しコピーも作成しません。
japanese.UninstallDeleteData=すべてのローカル データと資格情報を削除する
japanese.UninstallKeepData=暗号化されたデータを残す

korean.DataModeCaption=데이터 저장 위치
korean.DataModeDescription=Timelens가 활동 기록과 스냅샷을 저장할 위치를 선택하세요.
korean.DataModeSubCaption=데이터는 이 PC에 암호화되어 저장됩니다. 이 Windows 사용자에게 기존 Timelens 데이터가 있으면 원래 위치를 그대로 사용합니다. 나중에 설정 → 데이터 및 저장소에서 옮길 수도 있습니다.
korean.DataModeInstallDir=설치 위치 따르기(설치 폴더 안의 TimelensData 폴더, 권장)
korean.DataModeAppData=현재 사용자의 앱 데이터 폴더(AppData\Local\Timelens)
korean.DataModeCustom=사용자 지정 위치
korean.DataDirCaption=사용자 지정 데이터 위치
korean.DataDirDescription=데이터를 저장할 빈 폴더를 선택하세요.
korean.DataDirSubCaption=설치 폴더 안에서는 그 안의 TimelensData 폴더만 사용할 수 있습니다. 설치 프로그램이 이 폴더를 만들고 현재 사용자에게 쓰기 권한을 부여합니다.
korean.DataDirPrompt=데이터 폴더:
korean.DataDirNotFixed=데이터 폴더는 로컬 고정 디스크에 있어야 하며 드라이브 루트일 수 없습니다.
korean.DataDirCloud=데이터 폴더는 클라우드 동기화 폴더 안에 둘 수 없습니다.
korean.DataDirElevated=데이터 폴더는 보호된 수집기 폴더(%1) 안에 둘 수 없습니다.
korean.DataDirInsideApp=설치 폴더 안에서는 %1 폴더만 사용할 수 있습니다.
korean.DataDirIsFile=선택한 경로는 파일입니다. 폴더를 선택하세요.
korean.DataDirNotEmpty=%1에 이미 파일이 있습니다. 빈 폴더나 새 폴더를 선택하세요. Timelens는 기존 파일을 병합하지 않습니다. 이전에 보관한 데이터라면 앱 데이터 폴더를 선택하면 원래 위치를 계속 사용할 수 있습니다.
korean.InstallDirNotFixed=Timelens는 로컬 고정 디스크에 설치해야 합니다. 네트워크 드라이브, 이동식 드라이브, UNC 경로는 지원되지 않습니다.
korean.PrepareStopFailed=실행 중인 Timelens를 안전하게 중지할 수 없습니다. 파일은 아직 교체되지 않았습니다. 앱을 닫고 다시 시도하세요.
korean.PrepareCheckFailed=설치 전 검사를 통과하지 못했습니다. 파일은 아직 교체되지 않았습니다:
korean.TaskRegistrationFailed=Timelens 권한 경계와 로그온 작업을 설정할 수 없습니다(종료 코드 %1).
korean.UninstallCleanupFailed=데이터 또는 작업 정리가 완료되지 않아 제거를 중지했습니다. 데이터 위치를 확인한 후 다시 시도하세요. 앱 파일은 복구를 위해 남겨 두었습니다.
korean.UninstallDataQuestion=제거 후 로컬 데이터를 어떻게 처리할까요?
korean.UninstallDataNote=외부 백업과 내보낸 파일은 영향을 받지 않습니다. 삭제는 되돌릴 수 없으며 숨겨진 사본도 만들지 않습니다.
korean.UninstallDeleteData=모든 로컬 데이터 및 자격 증명 삭제
korean.UninstallKeepData=암호화된 데이터 유지

[Files]
Source: "..\target\release\timelens.exe"; DestDir: "{app}"; DestName: "Timelens.exe"; Flags: ignoreversion
Source: "..\target\release\timelens-collector.exe"; DestDir: "{commonpf64}\{#ProductName}"; DestName: "Timelens.Collector.exe"; Flags: ignoreversion
Source: "maintenance.ps1"; DestDir: "{commonpf64}\{#ProductName}\internal"; Flags: ignoreversion
Source: "path-safety.ps1"; DestDir: "{commonpf64}\{#ProductName}\internal"; Flags: ignoreversion
Source: "register-tasks.ps1"; DestDir: "{commonpf64}\{#ProductName}\internal"; Flags: ignoreversion
Source: "unregister-tasks.ps1"; DestDir: "{commonpf64}\{#ProductName}\internal"; Flags: ignoreversion

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Icons]
Name: "{autoprograms}\{#ProductName}"; Filename: "{app}\Timelens.exe"
Name: "{autodesktop}\{#ProductName}"; Filename: "{app}\Timelens.exe"; Tasks: desktopicon
; The uninstaller itself stays under Program Files with the other elevated files;
; the installation directory gets a shortcut to it, named in Setup's language;
; path-safety.ps1 accepts each of these names.
Name: "{app}\{cm:UninstallProgram,{#ProductName}}"; Filename: "{uninstallexe}"

[UninstallDelete]
; Only the folder prepared for the dataset, once the dataset is gone or moved.
Type: dirifempty; Name: "{app}\TimelensData"

#if DataDirectory == ""
; The core already runs in the background from its logon task; this only shows
; its window, so a first-time user sees the app instead of a silent tray icon.
[Run]
Filename: "{app}\Timelens.exe"; Description: "{cm:LaunchProgram,{#ProductName}}"; Flags: postinstall nowait skipifsilent runasoriginaluser
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
// the existing data where it is; moving it is done from 设置 → 数据与存储.
procedure InitializeWizard;
begin
  DataModePage := CreateInputOptionPage(wpSelectDir,
    CustomMessage('DataModeCaption'), CustomMessage('DataModeDescription'),
    CustomMessage('DataModeSubCaption'),
    True, False);
  DataModePage.Add(CustomMessage('DataModeInstallDir'));
  DataModePage.Add(CustomMessage('DataModeAppData'));
  DataModePage.Add(CustomMessage('DataModeCustom'));
  DataModePage.SelectedValueIndex := 0;
  DataDirPage := CreateInputDirPage(DataModePage.ID,
    CustomMessage('DataDirCaption'), CustomMessage('DataDirDescription'),
    CustomMessage('DataDirSubCaption'),
    False, '');
  DataDirPage.Add(CustomMessage('DataDirPrompt'));
end;

function IsSameOrInside(const Path, Parent: string): Boolean;
var
  P, Q: string;
begin
  P := Lowercase(RemoveBackslashUnlessRoot(Path));
  Q := Lowercase(RemoveBackslashUnlessRoot(Parent));
  Result := (P = Q) or (Pos(AddBackslash(Q), P) = 1);
end;

// Installed into the protected collector directory itself (the default under
// Program Files), the installation directory cannot hold data.
function InstallDirHoldsData: Boolean;
begin
  Result := not IsSameOrInside(WizardDirValue, ElevatedDir);
end;

function UseInstallDataDirectory: Boolean;
begin
  Result := (WizardForm.PrevAppDir = '') and (DataModePage.SelectedValueIndex = 0) and InstallDirHoldsData;
end;

function UseCustomDataDirectory: Boolean;
begin
  Result := (WizardForm.PrevAppDir = '') and (DataModePage.SelectedValueIndex = 2);
end;

function ShouldSkipPage(PageID: Integer): Boolean;
begin
  Result := ((PageID = DataModePage.ID) and (WizardForm.PrevAppDir <> '')) or
    ((PageID = DataDirPage.ID) and not UseCustomDataDirectory);
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

function InstallDataDirectory: string;
begin
  Result := AddBackslash(RemoveBackslashUnlessRoot(WizardDirValue)) + 'TimelensData';
end;

function DataDirectoryValue: string;
begin
  if UseInstallDataDirectory then
    Result := InstallDataDirectory
  else
    Result := RemoveBackslashUnlessRoot(ExpandFileName(Trim(DataDirPage.Values[0])));
end;

procedure CurPageChanged(CurPageID: Integer);
begin
  if CurPageID = DataModePage.ID then
  begin
    DataModePage.CheckListBox.ItemEnabled[0] := InstallDirHoldsData;
    if not InstallDirHoldsData and (DataModePage.SelectedValueIndex = 0) then
      DataModePage.SelectedValueIndex := 1;
  end;
  if (CurPageID = DataDirPage.ID) and (Trim(DataDirPage.Values[0]) = '') then
    if IsSameOrInside(WizardDirValue, ElevatedDir) then
      DataDirPage.Values[0] := ExtractFileDrive(WizardDirValue) + '\TimelensData'
    else
      DataDirPage.Values[0] := InstallDataDirectory;
end;

function DataDirectoryProblem: string;
var
  Path, AppDir: string;
begin
  Result := '';
  Path := DataDirectoryValue;
  AppDir := RemoveBackslashUnlessRoot(WizardDirValue);
  if (Length(Path) <= 3) or (Pos('"', Path) > 0) or not IsFixedLocalPath(Path) then
    Result := CustomMessage('DataDirNotFixed')
  else if (Pos('\onedrive', Lowercase(Path)) > 0) or (Pos('\dropbox', Lowercase(Path)) > 0) or
    (Pos('\icloud drive', Lowercase(Path)) > 0) or (Pos('\google drive', Lowercase(Path)) > 0) then
    Result := CustomMessage('DataDirCloud')
  else if IsSameOrInside(Path, ElevatedDir) then
    Result := FmtMessage(CustomMessage('DataDirElevated'), [ElevatedDir])
  else if IsSameOrInside(Path, AppDir) and (CompareText(Path, InstallDataDirectory) <> 0) then
    Result := FmtMessage(CustomMessage('DataDirInsideApp'), [InstallDataDirectory])
  else if FileExists(Path) then
    Result := CustomMessage('DataDirIsFile')
  else if DirExists(Path) and not IsEmptyDirectory(Path) then
    Result := FmtMessage(CustomMessage('DataDirNotEmpty'), [Path]);
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
      CustomMessage('InstallDirNotFixed'),
      mbError,
      MB_OK
    );
    Result := False;
  end;
#if DataDirectory == ""
  if (CurPageID = DataDirPage.ID) or ((CurPageID = DataModePage.ID) and UseInstallDataDirectory) then
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
      Result := CustomMessage('PrepareStopFailed')
    else
      Result := CustomMessage('PrepareCheckFailed') + #13#10#13#10 + Reason;
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
  if UseInstallDataDirectory or UseCustomDataDirectory then
    Parameters := Parameters + ' -UserDataDirectory ' + AddQuotes(DataDirectoryValue);
#endif
  if not Exec(PowerShellPath, Parameters, '', SW_HIDE, ewWaitUntilTerminated, ResultCode) or
     (ResultCode <> 0) then
  begin
    TaskRegistrationExitCode := 9;
    RaiseException(FmtMessage(CustomMessage('TaskRegistrationFailed'), [IntToStr(ResultCode)]));
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
      RaiseException(CustomMessage('UninstallCleanupFailed'));
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
    Choice := TaskDialogMsgBox(CustomMessage('UninstallDataQuestion'),
      CustomMessage('UninstallDataNote'), mbConfirmation,
      MB_YESNOCANCEL, [CustomMessage('UninstallDeleteData'), CustomMessage('UninstallKeepData')], 0);
    Result := (Choice = IDYES) or (Choice = IDNO);
    DeleteDataOnUninstall := Choice = IDYES;
  end;
end;
