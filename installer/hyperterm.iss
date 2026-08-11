; HyperTerm Windows Installer
; Built with Inno Setup 6 (https://jrsoftware.org/isinfo.php)
; CI invokes this via: ISCC.exe installer\hyperterm.iss
; (see .github/workflows/build.yml, job "installer")

#define MyAppName "HyperTerm"
#define MyAppVersion "0.1.0-phase1"
#define MyAppPublisher "Siro"
#define MyAppURL "https://github.com/rahmatsobrian/hyperterm"
#define MyAppExeName "hyperterm.exe"

[Setup]
AppId={{8F2C6B1E-9D4A-4E3C-8B1A-3F5C7D9E1A22}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
; Minimum OS: Windows 7 SP1, per spec.
MinVersion=6.1sp1
OutputDir=output
OutputBaseFilename=HyperTerm-Setup-{#MyAppVersion}
SetupIconFile=..\resources\icon.ico
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesInstallIn64BitMode=x64
ArchitecturesAllowed=x86 x64

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "addtopath"; Description: "Add HyperTerm to PATH (so `hyperterm` works from any terminal)"; GroupDescription: "Additional options"

[Files]
; The x64 binary is the default; CI builds the x86 job separately and this
; script is intentionally single-arch per build invocation (matches the
; artifact produced by the "installer" CI job, which builds
; x86_64-pc-windows-msvc). See ROADMAP.md for a combined x86+x64 installer.
Source: "..\target\x86_64-pc-windows-msvc\release\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\CHANGELOG.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{group}\{cm:UninstallProgram,{#MyAppName}}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Registry]
Root: HKLM; Subkey: "SYSTEM\CurrentControlSet\Control\Session Manager\Environment"; \
    ValueType: expandsz; ValueName: "Path"; ValueData: "{olddata};{app}"; \
    Tasks: addtopath; Check: NeedsAddPath(ExpandConstant('{app}'))

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(MyAppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent runascurrentuser; Parameters: "--help"

[Code]
function NeedsAddPath(Param: string): boolean;
var
  OrigPath: string;
begin
  if not RegQueryStringValue(HKEY_LOCAL_MACHINE,
    'SYSTEM\CurrentControlSet\Control\Session Manager\Environment', 'Path', OrigPath)
  then begin
    Result := True;
    exit;
  end;
  Result := Pos(';' + Param + ';', ';' + OrigPath + ';') = 0;
end;
