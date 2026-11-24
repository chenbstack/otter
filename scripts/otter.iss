; Windows 安装程序(Inno Setup)。CI 里由 ISCC /DAppVersion=<版本> scripts/otter.iss 调用,产物 target/installer/Otter-<版本>-windows-x86_64-setup.exe
; 装到当前用户目录,不需要管理员权限;可选把安装目录加进用户 PATH,装完直接能用 otter 命令。
#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
AppId={{6B0F7C1E-3D52-4B8E-9A47-0E7C2D5A91F3}
AppName=Otter
AppVersion={#AppVersion}
AppPublisher=Otter
DefaultDirName={localappdata}\Programs\Otter
DefaultGroupName=Otter
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\target\installer
OutputBaseFilename=Otter-{#AppVersion}-windows-x86_64-setup
SetupIconFile=..\assets\logo\otter.ico
UninstallDisplayIcon={app}\otter-gui.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ChangesEnvironment=yes
CloseApplications=yes

[Files]
Source: "..\target\release\otter-gui.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\otter.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Otter"; Filename: "{app}\otter-gui.exe"
Name: "{autodesktop}\Otter"; Filename: "{app}\otter-gui.exe"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "创建桌面快捷方式"; Flags: unchecked
Name: "addtopath"; Description: "把 otter 命令加入 PATH(给 agent / 终端用)"

[Registry]
Root: HKCU; Subkey: "Environment"; ValueType: expandsz; ValueName: "Path"; ValueData: "{olddata};{app}"; Tasks: addtopath; Check: NeedsAddPath(ExpandConstant('{app}'))

[Run]
Filename: "{app}\otter-gui.exe"; Description: "运行 Otter"; Flags: nowait postinstall skipifsilent

[Code]
function NeedsAddPath(Param: string): Boolean;
var
  OrigPath: string;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', OrigPath) then
  begin
    Result := True;
    exit;
  end;
  Result := Pos(';' + Lowercase(Param) + ';', ';' + Lowercase(OrigPath) + ';') = 0;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  P: string;
  AppDir: string;
  I: Integer;
begin
  if CurUninstallStep <> usPostUninstall then exit;
  if not RegQueryStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', P) then exit;
  AppDir := ExpandConstant('{app}');
  I := Pos(';' + Lowercase(AppDir), Lowercase(P));
  if I > 0 then
  begin
    Delete(P, I, Length(AppDir) + 1);
    RegWriteExpandStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', P);
  end;
end;
