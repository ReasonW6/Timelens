// The dark lens wizard: a one-click welcome page, a lens whose sweep fills with
// the installation, and a finished page where the pupil beats once. Included by
// Timelens.iss; the frames come from art\render_installer_art.py, whose frame
// counts and glow position the constants below must match.

type
  TPngFrames = array of TPngImage;

const
  IntroFrameCount = 41;
  ProgressStepCount = 25;
  BeatFrameCount = 8;
  FrameMs = 50;
  OrbitSlots = 48;
  OrbitPeriodMs = 2400;
  // The backdrop's glow centre, as a fraction of the wizard's client height.
  GlowCenter = 0.36;
  // Logical sizes at 100 % scaling; ScaleX/ScaleY adapt them to the DPI.
  LensSize = 120;
  OrbitRadius = 62;
  OrbitDotSize = 8;
  PillWidth = 168;
  PillHeight = 40;
  InkColor = $ECE6E4;
  Ink2Color = $BBAEA9;
  Ink3Color = $8B7C76;

var
  QuickInstall: Boolean;
  IntroFrames, ProgressFrames, BeatFrames: TPngFrames;
  OrbitX, OrbitY: array of Integer;
  AnimTimer: UINT_PTR;
  IntroFrame, BeatFrame, ShownStep, TargetStep: Integer;
  NextButtonLeft: Integer;
  WelcomeLens, InstallLens, FinishLens, OrbitDot: TBitmapImage;
  InstallPill, FinishPill: TBitmapButton;
  WelcomeNote, PercentText, InstallStatus: TNewStaticText;
  LaunchCheck: TNewCheckBox;

function SetTimer(hWnd: HWND; nIDEvent: UINT_PTR; uElapse: UINT; lpTimerFunc: NativeInt): UINT_PTR;
  external 'SetTimer@user32.dll stdcall';
function KillTimer(hWnd: HWND; uIDEvent: UINT_PTR): BOOL;
  external 'KillTimer@user32.dll stdcall';
function GetTickCount: DWORD;
  external 'GetTickCount@kernel32.dll stdcall';

function LoadPng(const Name: string): TPngImage;
begin
  Result := TPngImage.Create;
  Result.LoadFromFile(ExpandConstant('{tmp}\') + Name);
end;

function LoadFrames(const Prefix: string; Count: Integer): TPngFrames;
var
  Index: Integer;
begin
  SetArrayLength(Result, Count);
  for Index := 0 to Count - 1 do
    Result[Index] := LoadPng(Prefix + Format('%.2d', [Index]) + '.png');
end;

// The orbit, as offsets from the lens centre, without trigonometry: rotate a
// point by 7.5 degrees at a time.
procedure BuildOrbit;
var
  Index: Integer;
  X, Y, NextX: Extended;
begin
  SetArrayLength(OrbitX, OrbitSlots);
  SetArrayLength(OrbitY, OrbitSlots);
  X := 0;
  Y := -ScaleY(OrbitRadius);
  for Index := 0 to OrbitSlots - 1 do
  begin
    OrbitX[Index] := Round(X);
    OrbitY[Index] := Round(Y);
    NextX := X * 0.9914448613738104 - Y * 0.1305261922200516;
    Y := X * 0.1305261922200516 + Y * 0.9914448613738104;
    X := NextX;
  end;
end;

// Where a control sits inside the wizard's client area, so every page can centre
// the lens on the same spot of the backdrop's glow.
function FormLeft(Control: TWinControl): Integer;
begin
  Result := 0;
  while (Control <> nil) and (Control <> WizardForm) do
  begin
    Result := Result + Control.Left;
    Control := Control.Parent;
  end;
end;

function FormTop(Control: TWinControl): Integer;
begin
  Result := 0;
  while (Control <> nil) and (Control <> WizardForm) do
  begin
    Result := Result + Control.Top;
    Control := Control.Parent;
  end;
end;

function LensCenterX: Integer;
begin
  Result := WizardForm.ClientWidth div 2;
end;

function LensCenterY: Integer;
begin
  Result := Round(WizardForm.ClientHeight * GlowCenter);
end;

// The top of the content under the lens, in wizard client coordinates.
function BelowLens: Integer;
begin
  Result := LensCenterY + ScaleY(LensSize) div 2;
end;

function NewLens(Page: TWinControl; Frame: TPngImage): TBitmapImage;
begin
  Result := TBitmapImage.Create(WizardForm);
  Result.Parent := Page;
  Result.BackColor := clNone;
  Result.Stretch := True;
  Result.SetBounds(LensCenterX - FormLeft(Page) - ScaleX(LensSize) div 2,
    LensCenterY - FormTop(Page) - ScaleY(LensSize) div 2, ScaleX(LensSize), ScaleY(LensSize));
  Result.PngImage := Frame;
end;

function NewText(Parent: TWinControl; const Caption, FontName: string; Size, Color: Integer;
  Bold: Boolean; Top: Integer): TNewStaticText;
begin
  Result := TNewStaticText.Create(WizardForm);
  Result.Parent := Parent;
  Result.AutoSize := False;
  Result.WordWrap := True;
  Result.Alignment := taCenter;
  // Keep our own colors; the dark style would otherwise repaint every label.
  Result.StyleElements := [seClient, seBorder];
  Result.Font.Name := FontName;
  Result.Font.Size := Size;
  Result.Font.Color := Color;
  if Bold then
    Result.Font.Style := [fsBold];
  Result.Caption := Caption;
  Result.SetBounds(ScaleX(24), Top - FormTop(Parent), Parent.ClientWidth - ScaleX(48), ScaleY(Size * 2 + 6));
end;

// An amber pill that stands in for the Next button: reachable by Tab, Enter and
// screen readers. TBitmapButton keeps a 2-pixel margin for its focus rectangle.
function NewPill(Page: TWinControl; const Name, Caption: string; Top: Integer; OnClick: TNotifyEvent): TBitmapButton;
begin
  Result := TBitmapButton.Create(WizardForm);
  Result.Parent := Page;
  Result.Stretch := True;
  Result.Cursor := crHand;
  Result.Caption := Caption;
  Result.SetBounds(LensCenterX - FormLeft(Page) - ScaleX(PillWidth) div 2 - 2, Top - FormTop(Page),
    ScaleX(PillWidth) + 4, ScaleY(PillHeight) + 4);
  Result.PngImage := LoadPng(Name);
  Result.OnClick := OnClick;
end;

procedure ClickNext;
begin
  WizardForm.NextButton.OnClick(WizardForm.NextButton);
end;

procedure InstallPillClick(Sender: TObject);
begin
  QuickInstall := True;
  ClickNext;
end;

procedure CustomizeClick(Sender: TObject; const Link: string; LinkType: TSysLinkType);
begin
  QuickInstall := False;
  ClickNext;
end;

procedure FinishPillClick(Sender: TObject);
begin
  ClickNext;
end;

procedure BuildWelcomePage;
var
  Page: TWinControl;
  Link: TNewLinkLabel;
begin
  Page := WizardForm.WelcomePage;
  WizardForm.WelcomeLabel1.Visible := False;
  WizardForm.WelcomeLabel2.Visible := False;
  WizardForm.WizardBitmapImage.Visible := False;
  WelcomeLens := NewLens(Page, IntroFrames[0]);
  NewText(Page, 'Timelens', 'Segoe UI Semibold', 24, InkColor, False, BelowLens + ScaleY(8));
  NewText(Page, '看清每一天的时间去了哪里', 'Microsoft YaHei UI', 10, Ink2Color, False, BelowLens + ScaleY(52));
  InstallPill := NewPill(Page, 'button-install.png', '立即安装', BelowLens + ScaleY(86), @InstallPillClick);
  Link := TNewLinkLabel.Create(WizardForm);
  Link.Parent := Page;
  Link.Font.Name := 'Microsoft YaHei UI';
  Link.Caption := '<a id="customize">自定义安装位置</a>';
  Link.OnLinkClick := @CustomizeClick;
  Link.Left := LensCenterX - FormLeft(Page) - Link.Width div 2;
  Link.Top := BelowLens + ScaleY(136) - FormTop(Page);
  // Where the quick path installs, in the button row the welcome page leaves empty.
  WelcomeNote := NewText(WizardForm, '', 'Microsoft YaHei UI', 8, Ink3Color, False,
    WizardForm.CancelButton.Top - ScaleY(4));
  WelcomeNote.Height := WizardForm.ClientHeight - WelcomeNote.Top;
end;

procedure BuildInstallingPage;
var
  Page: TWinControl;
begin
  Page := WizardForm.InstallingPage;
  InstallLens := NewLens(Page, ProgressFrames[0]);
  OrbitDot := TBitmapImage.Create(WizardForm);
  OrbitDot.Parent := Page;
  OrbitDot.BackColor := clNone;
  OrbitDot.Stretch := True;
  OrbitDot.SetBounds(0, 0, ScaleX(OrbitDotSize), ScaleY(OrbitDotSize));
  OrbitDot.PngImage := LoadPng('orbit-dot.png');
  PercentText := NewText(Page, '0%', 'Segoe UI Semibold', 22, InkColor, False, BelowLens + ScaleY(12));
  InstallStatus := NewText(Page, '', 'Microsoft YaHei UI', 10, Ink2Color, False, BelowLens + ScaleY(58));
end;

procedure BuildFinishedPage;
var
  Page: TWinControl;
begin
  Page := WizardForm.FinishedPage;
  WizardForm.FinishedHeadingLabel.Visible := False;
  WizardForm.FinishedLabel.Visible := False;
  WizardForm.WizardBitmapImage2.Visible := False;
  FinishLens := NewLens(Page, ProgressFrames[ProgressStepCount]);
  NewText(Page, '准备就绪', 'Microsoft YaHei UI', 18, InkColor, True, BelowLens + ScaleY(8));
  NewText(Page, 'Timelens 会在每次登录时自动启动，在通知区域安静运行。', 'Microsoft YaHei UI', 10,
    Ink2Color, False, BelowLens + ScaleY(50));
  FinishPill := NewPill(Page, 'button-finish.png', '完成', BelowLens + ScaleY(116), @FinishPillClick);
end;

procedure LaunchCheckClick(Sender: TObject);
begin
  WizardForm.RunList.Checked[0] := LaunchCheck.Checked;
end;

// The "打开 Timelens" entry from [Run] as a plain checkbox: the styled run list
// paints an opaque box over the backdrop. The list itself stays out of sight but
// enabled, since Setup reads the choice from it.
procedure PlaceLaunchCheck;
var
  Page: TWinControl;
begin
  Page := WizardForm.FinishedPage;
  WizardForm.RunList.Left := -WizardForm.RunList.Width - ScaleX(100);
  if WizardForm.RunList.Items.Count = 0 then
    Exit;
  LaunchCheck := TNewCheckBox.Create(WizardForm);
  LaunchCheck.Parent := Page;
  LaunchCheck.StyleElements := [seClient, seBorder];
  LaunchCheck.Font.Color := Ink2Color;
  LaunchCheck.Caption := WizardForm.RunList.ItemCaption[0];
  LaunchCheck.Checked := WizardForm.RunList.Checked[0];
  LaunchCheck.OnClick := @LaunchCheckClick;
  LaunchCheck.Width := ScaleX(140);
  LaunchCheck.Height := ScaleY(22);
  LaunchCheck.Left := LensCenterX - FormLeft(Page) - ScaleX(56);
  LaunchCheck.Top := BelowLens + ScaleY(82) - FormTop(Page);
end;

// What the quick path will do, shown under the welcome page's buttons.
function QuickInstallNote: string;
begin
  if WizardForm.PrevAppDir <> '' then
    Result := '将更新 ' + WizardDirValue + ' 中的 Timelens，数据位置保持不变'
  else
  begin
    Result := '安装到 ' + WizardDirValue + #13#10 + '数据保存在 ';
#if DataDirectory == ""
    if UseInstallDataDirectory or UseCustomDataDirectory then
      Result := Result + DataDirectoryValue
    else
      Result := Result + 'AppData\Local\Timelens';
#else
    Result := Result + '{#DataDirectory}';
#endif
  end;
end;

procedure UpdatePercent(Step: Integer);
begin
  PercentText.Caption := IntToStr(Step * 100 div ProgressStepCount) + '%';
end;

procedure AnimTick(Wnd: HWND; Msg: UINT; IdEvent: UINT_PTR; Time: DWORD);
var
  Slot: Integer;
begin
  case WizardForm.CurPageID of
    wpWelcome:
      if IntroFrame < IntroFrameCount - 1 then
      begin
        IntroFrame := IntroFrame + 1;
        WelcomeLens.PngImage := IntroFrames[IntroFrame];
      end;
    wpInstalling:
      begin
        if ShownStep < TargetStep then
        begin
          ShownStep := ShownStep + 1;
          InstallLens.PngImage := ProgressFrames[ShownStep];
          UpdatePercent(ShownStep);
        end;
        Slot := (GetTickCount mod OrbitPeriodMs) * OrbitSlots div OrbitPeriodMs;
        OrbitDot.Left := InstallLens.Left + InstallLens.Width div 2 + OrbitX[Slot] - OrbitDot.Width div 2;
        OrbitDot.Top := InstallLens.Top + InstallLens.Height div 2 + OrbitY[Slot] - OrbitDot.Height div 2;
      end;
    wpFinished:
      if BeatFrame < BeatFrameCount then
      begin
        FinishLens.PngImage := BeatFrames[BeatFrame];
        BeatFrame := BeatFrame + 1;
      end;
  end;
end;

procedure SetTargetProgress(Fraction: Extended);
begin
  TargetStep := Round(Fraction * ProgressStepCount);
  if TargetStep > ProgressStepCount then
    TargetStep := ProgressStepCount;
end;

<event('InitializeWizard')>
procedure InitializeDarkWizard;
begin
  QuickInstall := True;
  ExtractTemporaryFiles('{tmp}\lens-*.png');
  ExtractTemporaryFile('orbit-dot.png');
  ExtractTemporaryFile('button-install.png');
  ExtractTemporaryFile('button-finish.png');
  IntroFrames := LoadFrames('lens-intro-', IntroFrameCount);
  ProgressFrames := LoadFrames('lens-progress-', ProgressStepCount + 1);
  BeatFrames := LoadFrames('lens-done-', BeatFrameCount);
  BuildOrbit;
  NextButtonLeft := WizardForm.NextButton.Left;
  BuildWelcomePage;
  BuildInstallingPage;
  BuildFinishedPage;
  Log(Format('Dark wizard: client %dx%d, welcome page %dx%d at %d', [WizardForm.ClientWidth,
    WizardForm.ClientHeight, WizardForm.WelcomePage.ClientWidth, WizardForm.WelcomePage.ClientHeight,
    FormTop(WizardForm.WelcomePage)]));
  AnimTimer := SetTimer(0, 0, FrameMs, CreateCallback(@AnimTick));
end;

<event('DeinitializeSetup')>
procedure DeinitializeDarkWizard;
begin
  if AnimTimer <> 0 then
    KillTimer(0, AnimTimer);
  AnimTimer := 0;
end;

// The quick path skips straight from the welcome page to the installation.
<event('ShouldSkipPage')>
function SkipForQuickInstall(PageID: Integer): Boolean;
begin
  Result := QuickInstall and (PageID <> wpWelcome) and (PageID <> wpFinished);
end;

<event('CurPageChanged')>
procedure DarkPageChanged(CurPageID: Integer);
var
  OnOwnPage: Boolean;
begin
  // The welcome and finished pages carry their own amber button instead. The Next
  // button stays the default there, out of sight and out of the Tab order, so
  // Enter still works.
  OnOwnPage := (CurPageID = wpWelcome) or (CurPageID = wpFinished);
  if OnOwnPage then
    WizardForm.NextButton.Left := -WizardForm.NextButton.Width - ScaleX(100)
  else
    WizardForm.NextButton.Left := NextButtonLeft;
  WizardForm.NextButton.TabStop := not OnOwnPage;
  WizardForm.NextButton.Visible := CurPageID <> wpInstalling;
  WizardForm.BackButton.Visible := not OnOwnPage and (CurPageID <> wpInstalling);
  WizardForm.CancelButton.Visible := not OnOwnPage;
  WelcomeNote.Visible := CurPageID = wpWelcome;
  case CurPageID of
    wpWelcome:
      begin
        QuickInstall := True;
        WelcomeNote.Caption := QuickInstallNote;
        WizardForm.ActiveControl := InstallPill;
      end;
    wpInstalling:
      begin
        // A full-height page: no header, the lens on the glow like the welcome page.
        WizardForm.MainPanel.Visible := False;
        WizardForm.Bevel1.Visible := False;
        WizardForm.InnerNotebook.SetBounds(0, 0, WizardForm.InnerPage.ClientWidth, WizardForm.InnerPage.ClientHeight);
        InstallLens.Left := LensCenterX - FormLeft(WizardForm.InstallingPage) - InstallLens.Width div 2;
        InstallLens.Top := LensCenterY - FormTop(WizardForm.InstallingPage) - InstallLens.Height div 2;
        PercentText.SetBounds(ScaleX(24), BelowLens + ScaleY(12) - FormTop(WizardForm.InstallingPage),
          WizardForm.InstallingPage.ClientWidth - ScaleX(48), PercentText.Height);
        InstallStatus.SetBounds(ScaleX(24), BelowLens + ScaleY(58) - FormTop(WizardForm.InstallingPage),
          WizardForm.InstallingPage.ClientWidth - ScaleX(48), InstallStatus.Height);
        WizardForm.StatusLabel.Visible := False;
        WizardForm.FilenameLabel.Visible := False;
        WizardForm.ProgressGauge.Visible := False;
        InstallStatus.Caption := '正在复制程序文件…';
      end;
    wpFinished:
      begin
        BeatFrame := 0;
        PlaceLaunchCheck;
        WizardForm.ActiveControl := FinishPill;
      end;
  end;
end;

<event('CurInstallProgressChanged')>
procedure DarkProgressChanged(CurProgress, MaxProgress: Integer);
begin
  // Copying files is the first 85 %; the logon tasks take the rest.
  if MaxProgress > 0 then
    SetTargetProgress(0.85 * CurProgress / MaxProgress);
end;

<event('CurStepChanged')>
procedure DarkStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
  begin
    SetTargetProgress(0.92);
    InstallStatus.Caption := '正在设置目录权限和登录任务…';
  end
  else if CurStep = ssDone then
    SetTargetProgress(1);
end;
