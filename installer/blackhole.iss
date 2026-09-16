; Blackhole — Windows x64 installer (Inno Setup 6)
; Built by build-installer.sh, which stages everything under dist/ first.

#define AppName "Blackhole"
#define AppVersion GetEnv("BLACKHOLE_VERSION")
#if AppVersion == ""
  #define AppVersion "0.1.0"
#endif
#define AppPublisher "thowd22"
#define AppURL "https://github.com/thowd22/Blackhole"

[Setup]
AppId={{7D2B5C1E-3F4A-4C1B-9E6A-BLACKHOLE0001}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppURL}
AppSupportURL={#AppURL}/issues
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
OutputDir=..\dist
OutputBaseFilename=Blackhole-{#AppVersion}-x64-setup
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; The model weights are int4 and do not compress; everything else is small.
Compression=lzma2/fast
SolidCompression=no
LZMAUseSeparateProcess=yes
DiskSpanning=no
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
WizardStyle=modern
UninstallDisplayIcon={app}\blackhole.exe
SetupIconFile=blackhole.ico
CloseApplications=yes
RestartApplications=no
MinVersion=10.0.18362

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "startup"; Description: "Start Blackhole when I sign in"; GroupDescription: "Options:"
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Options:"; Flags: unchecked

[Files]
Source: "..\dist\stage\blackhole.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "blackhole.ico"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\dist\stage\onnxruntime.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\dist\stage\DirectML.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\dist\stage\llama-3.2-3b\model_q4f16.onnx"; DestDir: "{app}\llama-3.2-3b"; Flags: ignoreversion
Source: "..\dist\stage\llama-3.2-3b\model_q4f16.onnx.data"; DestDir: "{app}\llama-3.2-3b"; Flags: ignoreversion nocompression
Source: "..\dist\stage\llama-3.2-3b\tokenizer.json"; DestDir: "{app}\llama-3.2-3b"; Flags: ignoreversion
Source: "..\dist\stage\README.md"; DestDir: "{app}"; Flags: ignoreversion isreadme
Source: "..\dist\stage\LICENSE-MODELS.txt"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\blackhole.exe"
Name: "{group}\Uninstall {#AppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\blackhole.exe"; Tasks: desktopicon

[Registry]
; Same key the app's "Start at login" menu item manages.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "Blackhole"; ValueData: """{app}\blackhole.exe"""; Flags: uninsdeletevalue; Tasks: startup

[Run]
Filename: "{app}\blackhole.exe"; Description: "Launch {#AppName} now"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "taskkill.exe"; Parameters: "/IM blackhole.exe /F"; Flags: runhidden; RunOnceId: "killapp"

[UninstallDelete]
; Runtime files the app unpacks beside itself if the install folder is writable.
Type: files; Name: "{app}\onnxruntime.dll"
Type: files; Name: "{app}\DirectML.dll"

[Code]
// The vault (%LOCALAPPDATA%\Blackhole) holds the user's documents and index:
// never removed silently. Offer once at uninstall.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Vault: String;
begin
  if CurUninstallStep = usPostUninstall then
  begin
    Vault := ExpandConstant('{localappdata}\Blackhole');
    if DirExists(Vault) then
      if MsgBox('Also delete your vault (indexed documents and settings) in' + #13#10 + Vault + '?',
                mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
        DelTree(Vault, True, True, True);
  end;
end;
