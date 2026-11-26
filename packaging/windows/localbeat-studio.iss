; LocalBeat Studio — Per-user Windows installer (Inno Setup 6)
;
; Installs localbeat-studio.exe and localbeat.exe to:
;   %LOCALAPPDATA%\Programs\LocalBeat Studio
; with zero UAC admin prompt, creates a Start Menu shortcut, adds the install
; folder to the user's PATH, and registers the `localbeat://` URI handler in
; HKCU so clicking "Open in LocalBeat Studio" on the invite landing page
; launches Studio directly into enrollment.
;
; Built via: pwsh -File tools\build-windows-installer.ps1

#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif

#define AppName "LocalBeat Studio"
#define AppPublisher "LocalBeat"
#define AppURL "https://jimbo-yo.github.io/LocalBeat/"
#define AppExeName "localbeat-studio.exe"
#define CliExeName "localbeat.exe"

[Setup]
AppId={{8E9B3C4D-2F6A-4B19-9D7E-1A5C8F3E6B20}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppURL}
AppSupportURL=https://github.com/jimbo-yo/LocalBeatStudio/issues
AppUpdatesURL=https://github.com/jimbo-yo/LocalBeatStudio/releases
DefaultDirName={localappdata}\Programs\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
; Per-user install: no UAC prompt for non-technical family members.
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\..\dist
OutputBaseFilename=LocalBeatStudio-{#AppVersion}-windows-x64-setup
SetupIconFile=..\..\gui\assets\localbeat.ico
UninstallDisplayIcon={app}\{#AppExeName}
UninstallDisplayName={#AppName}
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
ChangesEnvironment=yes
CloseApplications=yes
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "addtopath"; Description: "Add the localbeat CLI to my user PATH"; GroupDescription: "Command line:"; Flags: checkedonce

[Files]
Source: "..\..\target\release\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\target\release\{#CliExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Comment: "Music library, player, CD ripper and phone sync"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon; Comment: "Music library, player, CD ripper and phone sync"

[Registry]
; Register per-user `localbeat://` URI handler (no admin/UAC required).
Root: HKCU; Subkey: "Software\Classes\localbeat"; ValueType: string; ValueName: ""; ValueData: "URL:LocalBeat Protocol"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\localbeat"; ValueType: string; ValueName: "URL Protocol"; ValueData: ""
Root: HKCU; Subkey: "Software\Classes\localbeat\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: """{app}\{#AppExeName}"",0"
Root: HKCU; Subkey: "Software\Classes\localbeat\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#AppExeName}"" ""%1"""

[Run]
Filename: "{app}\{#AppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(AppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent

[Code]
const
  EnvKey = 'Environment';

function NeedsAddPath(Param: string): boolean;
var
  OrigPath: string;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, EnvKey, 'Path', OrigPath) then
  begin
    Result := True;
    exit;
  end;
  Result := Pos(';' + Uppercase(Param) + ';', ';' + Uppercase(OrigPath) + ';') = 0;
end;

procedure AddDirToUserPath(Dir: string);
var
  OrigPath: string;
begin
  if not NeedsAddPath(Dir) then exit;
  if not RegQueryStringValue(HKEY_CURRENT_USER, EnvKey, 'Path', OrigPath) then
    OrigPath := '';
  if (Length(OrigPath) > 0) and (OrigPath[Length(OrigPath)] <> ';') then
    OrigPath := OrigPath + ';';
  RegWriteExpandStringValue(HKEY_CURRENT_USER, EnvKey, 'Path', OrigPath + Dir);
end;

procedure RemoveDirFromUserPath(Dir: string);
var
  OrigPath, UpperOrig, UpperDir, NewPath: string;
  P: Integer;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, EnvKey, 'Path', OrigPath) then exit;
  UpperOrig := ';' + Uppercase(OrigPath) + ';';
  UpperDir := ';' + Uppercase(Dir) + ';';
  P := Pos(UpperDir, UpperOrig);
  if P = 0 then exit;
  NewPath := Copy(OrigPath, 1, P - 1) + Copy(OrigPath, P + Length(Dir) + 1, MaxInt);
  while (Length(NewPath) > 0) and (NewPath[1] = ';') do
    Delete(NewPath, 1, 1);
  while (Length(NewPath) > 0) and (NewPath[Length(NewPath)] = ';') do
    Delete(NewPath, Length(NewPath), 1);
  StringChangeEx(NewPath, ';;', ';', True);
  RegWriteExpandStringValue(HKEY_CURRENT_USER, EnvKey, 'Path', NewPath);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep = ssPostInstall) and WizardIsTaskSelected('addtopath') then
    AddDirToUserPath(ExpandConstant('{app}'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    RemoveDirFromUserPath(ExpandConstant('{app}'));
end;
