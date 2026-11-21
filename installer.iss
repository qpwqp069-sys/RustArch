#define MyAppName "RustArch"
#define MyAppVersion "0.5.0"
#define MyAppPublisher "RustArch"
#define MyAppExeName "RustArch.exe"

[Setup]
; Постоянный паспорт приложения — НЕ МЕНЯЙ его, чтобы новые версии обновляли старую на месте
AppId={{7B4D92A1-4F3E-4C8B-9A12-6E5D8F1C3A90}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
DefaultDirName={autopf}\{#MyAppName}
DisableDirPage=no
UsePreviousAppDir=yes
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=dist
OutputBaseFilename=RustArch_Setup_v{#MyAppVersion}
SetupIconFile=app_icon.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
UninstallDisplayName={#MyAppName}
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
RestartApplications=no
ChangesAssociations=yes

[Languages]
Name: "russian"; MessagesFile: "compiler:Languages\Russian.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"
Name: "contextmenu"; Description: "Добавить «Открыть в RustArch» в меню правой кнопки мыши (ПКМ)"; GroupDescription: "Интеграция с Windows:"
Name: "fileassoc"; Description: "Открывать двойным кликом .001 (части Discord), .xz, .zst, .br, .lz4"; GroupDescription: "Интеграция с Windows:"

[Files]
Source: "target\release\rust_archiver.exe"; DestDir: "{app}"; DestName: "{#MyAppExeName}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Registry]
; Контекстное меню ПКМ
Root: HKCU; Subkey: "Software\Classes\*\shell\RustArch"; ValueType: string; ValueName: ""; ValueData: "Открыть в RustArch"; Tasks: contextmenu; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\*\shell\RustArch"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\{#MyAppExeName}"",0"; Tasks: contextmenu
Root: HKCU; Subkey: "Software\Classes\*\shell\RustArch\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExeName}"" ""%1"""; Tasks: contextmenu

Root: HKCU; Subkey: "Software\Classes\Directory\shell\RustArch"; ValueType: string; ValueName: ""; ValueData: "Открыть в RustArch"; Tasks: contextmenu; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\Directory\shell\RustArch"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\{#MyAppExeName}"",0"; Tasks: contextmenu
Root: HKCU; Subkey: "Software\Classes\Directory\shell\RustArch\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExeName}"" ""%1"""; Tasks: contextmenu

; Ассоциации файлов (двойной клик)
Root: HKCU; Subkey: "Software\Classes\RustArch.Archive"; ValueType: string; ValueName: ""; ValueData: "Архив RustArch"; Tasks: fileassoc; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\RustArch.Archive\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExeName}"",0"; Tasks: fileassoc
Root: HKCU; Subkey: "Software\Classes\RustArch.Archive\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExeName}"" ""%1"""; Tasks: fileassoc

Root: HKCU; Subkey: "Software\Classes\.001"; ValueType: string; ValueName: ""; ValueData: "RustArch.Archive"; Tasks: fileassoc; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\.xz"; ValueType: string; ValueName: ""; ValueData: "RustArch.Archive"; Tasks: fileassoc; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\.zst"; ValueType: string; ValueName: ""; ValueData: "RustArch.Archive"; Tasks: fileassoc; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\.br"; ValueType: string; ValueName: ""; ValueData: "RustArch.Archive"; Tasks: fileassoc; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\.lz4"; ValueType: string; ValueName: ""; ValueData: "RustArch.Archive"; Tasks: fileassoc; Flags: uninsdeletekey

; Гарантированная очистка реестра при удалении
Root: HKCU; Subkey: "Software\Classes\*\shell\RustArch"; Flags: dontcreatekey uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\Directory\shell\RustArch"; Flags: dontcreatekey uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\RustArch.Archive"; Flags: dontcreatekey uninsdeletekey

[UninstallDelete]
Type: filesandordirs; Name: "{userappdata}\RustArch"

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(MyAppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent