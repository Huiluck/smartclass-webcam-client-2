; SC Webcam Windows installer (Inno Setup 6).
; Built by .github/workflows/release.yml, which passes /DAppVersion and /DArch.

#define AppName "SC Webcam"

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef Arch
  #define Arch "x64"
#endif

[Setup]
AppId={{7B3E9A1D-2C4F-4E5A-9B6D-1F0A3C5E7D9B}
AppName={#AppName}
AppVersion={#AppVersion}
; {autopf} resolves per install mode: Program Files for an all-users
; (elevated) install, %LOCALAPPDATA%\Programs for a per-user install.
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
; Offer "for all users / just for me" up front; choosing all users triggers
; the UAC prompt, per-user stays unelevated.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
OutputDir=..\..
OutputBaseFilename=scwc-client-v{#AppVersion}-windows-{#Arch}-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
#if Arch == "arm64"
ArchitecturesAllowed=arm64
ArchitecturesInstallIn64BitMode=arm64
#else
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
#endif

[Files]
; Paths are relative to this script: app/installer/windows/ -> app/.
Source: "..\..\build\windows\{#Arch}\runner\Release\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\sc_webcam.exe"
