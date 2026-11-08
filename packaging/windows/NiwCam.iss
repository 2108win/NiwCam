; NiwCam installer (Inno Setup 6). Built by scripts\package-release.ps1:
;   ISCC /DAppVersion=0.1.3 /DSourceDir=<staged app folder> /DOutputDir=build\dist NiwCam.iss
; One .exe: installs the app to Program Files, registers the virtual camera for
; Frame Server, adds Start menu / desktop shortcuts and an Apps & features entry.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceDir
  #error SourceDir (the staged CamStudio app folder) is required
#endif
#ifndef OutputDir
  #define OutputDir "."
#endif

#define VCamClsid "{72F5B2FD-C48E-449B-95CE-E8F13BF200CB}"
#define DShowClsid "{7D2B9C41-3A6E-4F58-B0D1-9E4C2F6A8B13}"
#define DShowCategory "{860BB310-5D01-11d0-BD3B-00A0C911CE86}"

[Setup]
AppId={{6E3C8C2A-3F0B-4B8E-9C61-5A2D7F1E9B44}
AppName=NiwCam
AppVersion={#AppVersion}
AppVerName=NiwCam {#AppVersion}
AppPublisher=Win Lã
AppPublisherURL=https://github.com/2108win/NiwCam
AppSupportURL=https://github.com/2108win/NiwCam/issues
AppUpdatesURL=https://github.com/2108win/NiwCam/releases
DefaultDirName={autopf}\NiwCam
DisableProgramGroupPage=yes
DisableDirPage=auto
; The virtual camera is registered machine-wide (Frame Server only reads HKLM).
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Windows 10 2004 (build 19041) or newer, 64-bit. Windows 11 gets the Media Foundation virtual
; camera (every app sees it); Windows 10 gets a DirectShow filter instead (docs/windows10.md).
; The engine statically imports DnsServiceBrowse/Resolve (mDNS), added in build 18362, so 1809
; cannot load it; 19041 is the supported floor.
MinVersion=10.0.19041
OutputDir={#OutputDir}
OutputBaseFilename=NiwCam-Setup-{#AppVersion}
SetupIconFile=..\..\app\Assets\NiwCam.ico
UninstallDisplayIcon={app}\App\NiwCam.exe
UninstallDisplayName=NiwCam
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
CloseApplications=no
RestartIfNeededByRun=no

[Languages]
Name: "vi"; MessagesFile: "Vietnamese.isl"
Name: "en"; MessagesFile: "compiler:Default.isl"

[CustomMessages]
vi.DesktopIcon=Tạo biểu tượng trên màn hình nền
en.DesktopIcon=Create a desktop shortcut
vi.StartWithWindows=Chạy cùng Windows (ẩn ở khay, chỉ bật camera khi có ứng dụng dùng)
en.StartWithWindows=Start with Windows (in the tray; the camera only wakes when an app uses it)
vi.LaunchNow=Mở NiwCam ngay
en.LaunchNow=Open NiwCam now
vi.ShortcutComment=Dùng webcam hoặc điện thoại làm camera ảo ở độ phân giải gốc
en.ShortcutComment=Use a webcam or a phone as a native-resolution virtual camera

[Tasks]
Name: "desktopicon"; Description: "{cm:DesktopIcon}"; Flags: unchecked
Name: "autostart"; Description: "{cm:StartWithWindows}"; Flags: unchecked

[Files]
Source: "{#SourceDir}\*"; DestDir: "{app}\App"; Excludes: "CamStudioVCam.dll,CamStudioDShow64.dll,CamStudioDShow32.dll,\adb\*"; Flags: ignoreversion recursesubdirs createallsubdirs
; adb only when it changed: an adb server still running from App\adb (restarted by any adb client
; after StopCameraUsers) locks adb.exe, and a file that cannot be replaced aborts the whole update
; (2026-10-20: rolled back 0.1.8 on the user's PC, leaving new files out).
Source: "{#SourceDir}\adb\*"; DestDir: "{app}\App\adb"; Flags: comparetimestamp recursesubdirs createallsubdirs
; The virtual camera lives next to the app folder and is registered for Frame Server
; (regserver = regsvr32 on install, unregister on uninstall). Windows 11 only.
Source: "{#SourceDir}\CamStudioVCam.dll"; DestDir: "{app}"; Flags: ignoreversion regserver 64bit; Check: IsWindows11
; Windows 10: the DirectShow source filter, for 64-bit and for 32-bit apps.
Source: "{#SourceDir}\CamStudioDShow64.dll"; DestDir: "{app}"; Flags: ignoreversion regserver 64bit; Check: not IsWindows11
Source: "{#SourceDir}\CamStudioDShow32.dll"; DestDir: "{app}"; Flags: ignoreversion regserver 32bit; Check: not IsWindows11

[Icons]
Name: "{autoprograms}\NiwCam"; Filename: "{app}\App\NiwCam.exe"; Comment: "{cm:ShortcutComment}"
Name: "{autodesktop}\NiwCam"; Filename: "{app}\App\NiwCam.exe"; Comment: "{cm:ShortcutComment}"; Tasks: desktopicon

[Registry]
; Same value the app writes from its own "Khởi động cùng Windows" switch.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "NiwCam"; ValueData: """{app}\App\NiwCam.exe"" --tray"; Tasks: autostart; Flags: uninsdeletevalue

[Run]
Filename: "{app}\App\NiwCam.exe"; Description: "{cm:LaunchNow}"; Flags: nowait postinstall skipifsilent runasoriginaluser
; An update started by the app (/SILENT /RELAUNCH=show|tray, app\Services\Updater.cs): open it again.
Filename: "{app}\App\NiwCam.exe"; Parameters: "{code:RelaunchArgs}"; Flags: nowait runasoriginaluser; Check: RelaunchAfterUpdate

; Before 0.1.14 the app was "Cam Studio" (CamStudio.exe): its shortcuts and its own files go. The camera DLLs keep
; their names (their registration is recorded in the uninstall log).
[InstallDelete]
Type: files; Name: "{autoprograms}\Cam Studio.lnk"
Type: files; Name: "{autodesktop}\Cam Studio.lnk"
Type: files; Name: "{app}\App\CamStudio.*"
Type: files; Name: "{app}\App\CamStudioEngine.dll"

[UninstallDelete]
Type: filesandordirs; Name: "{app}\App"

[Code]
function IsWindows11(): Boolean;
var
  V: TWindowsVersion;
begin
  GetWindowsVersionEx(V);
  Result := V.Build >= 22000;
end;

function RelaunchAfterUpdate(): Boolean;
begin
  Result := WizardSilent and (ExpandConstant('{param:RELAUNCH|}') <> '');
end;

function RelaunchArgs(Param: String): String;
begin
  // --updated: the app does not install another update by itself in that session.
  if CompareText(ExpandConstant('{param:RELAUNCH|}'), 'tray') = 0 then
    Result := '--tray --updated'
  else
    Result := '--updated';
end;

// Frame Server keeps the virtual camera DLL loaded and the app keeps its files open:
// close both before files are replaced or removed.
procedure StopCameraUsers();
var
  Code: Integer;
  Adb: String;
begin
  // CamStudio.exe is what the app was called before 0.1.14: the one an update starts from.
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM CamStudio.exe', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM NiwCam.exe', '', SW_HIDE, ewWaitUntilTerminated, Code);
  // The adb server the app starts for Android phones on a cable runs from App\adb. Stop only
  // that one: "adb kill-server" would stop any server on the port (Android Studio's too).
  Adb := ExpandConstant('{app}\App\adb\adb.exe');
  if FileExists(Adb) then
    Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'),
      '-NoProfile -NonInteractive -Command "Get-Process adb -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq ''' + Adb + ''' } | Stop-Process -Force"',
      '', SW_HIDE, ewWaitUntilTerminated, Code);
  Exec(ExpandConstant('{sys}\sc.exe'), 'stop FrameServerMonitor', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Exec(ExpandConstant('{sys}\sc.exe'), 'stop FrameServer', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Sleep(1500);
end;

// Leftovers of the older zip + Install.cmd installs: a timestamped DLL copy and
// their own Apps & features entry.
procedure RemoveOldZipInstall();
var
  FindRec: TFindRec;
  Dir: String;
begin
  Dir := ExpandConstant('{autopf}\CamStudio');
  if FindFirst(Dir + '\CamStudioVCam-*.dll', FindRec) then
  begin
    try
      repeat
        DeleteFile(Dir + '\' + FindRec.Name);
      until not FindNext(FindRec);
    finally
      FindClose(FindRec);
    end;
  end;
  DeleteFile(Dir + '\Uninstall.ps1');
  RegDeleteKeyIncludingSubkeys(HKLM, 'SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\CamStudio');
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  StopCameraUsers();
  RemoveOldZipInstall();
  Result := '';
end;

function InitializeUninstall(): Boolean;
begin
  StopCameraUsers();
  Result := True;
end;

// Before 0.1.14 the autostart value was "CamStudio" and pointed at CamStudio.exe, which this install
// removes: point a value of ours at NiwCam.exe in its place (the app does the same on its first start,
// for the case where this runs under another account's HKCU).
procedure CurStepChanged(CurStep: TSetupStep);
var
  RunKey, OldValue, Args: String;
begin
  if CurStep <> ssPostInstall then Exit;
  RunKey := 'Software\Microsoft\Windows\CurrentVersion\Run';
  if RegQueryStringValue(HKCU, RunKey, 'CamStudio', OldValue) then
  begin
    Args := '';
    if Pos('--tray', OldValue) > 0 then Args := ' --tray';
    RegWriteStringValue(HKCU, RunKey, 'NiwCam', '"' + ExpandConstant('{app}\App\NiwCam.exe') + '"' + Args);
    RegDeleteValue(HKCU, RunKey, 'CamStudio');
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
  begin
    // regserver already unregistered the classes; make sure nothing points at the deleted DLLs.
    RegDeleteKeyIncludingSubkeys(HKLM, 'SOFTWARE\Classes\CLSID\{#VCamClsid}');
    RegDeleteKeyIncludingSubkeys(HKLM, 'SOFTWARE\Classes\CLSID\{#DShowClsid}');
    RegDeleteKeyIncludingSubkeys(HKLM, 'SOFTWARE\Classes\CLSID\{#DShowCategory}\Instance\{#DShowClsid}');
    RegDeleteKeyIncludingSubkeys(HKLM, 'SOFTWARE\Classes\WOW6432Node\CLSID\{#DShowClsid}');
    RegDeleteKeyIncludingSubkeys(HKLM, 'SOFTWARE\Classes\WOW6432Node\CLSID\{#DShowCategory}\Instance\{#DShowClsid}');
    RegDeleteValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Run', 'CamStudio');
    RegDeleteValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Run', 'NiwCam');
  end;
end;
