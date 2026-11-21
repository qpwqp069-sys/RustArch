#define MyAppName "RustArch"
#define MyAppVersion "0.4.0"
#define MyAppPublisher "RustArch"
#define MyAppExeName "RustArch.exe"

[Setup]
; Постоянный паспорт приложения — НЕ МЕНЯЙ его в v5, v6 и т.д.,
; чтобы новые версии обновляли старую на месте без дублей
AppId={{7B4D92A1-4F3E-4C8B-9A12-6E5D8F1C3A90}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} {#MyAppVersion}
AppPublisher={#MyAppPublisher}
; По умолчанию ставится в AppData\Local\Programs\RustArch без прав админа,
; но юзер может выбрать любую другую папку
DefaultDirName={autopf}\{#MyAppName}
DisableDirPage=no
UsePreviousAppDir=yes
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Куда сохранить готовый Setup.exe
OutputDir=dist
OutputBaseFilename=RustArch_Setup_v{#MyAppVersion}
SetupIconFile=app_icon.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
UninstallDisplayName={#MyAppName}
; Плотное сжатие установщика и современный интерфейс Windows
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
; Сам закроет запущенный RustArch перед обновлением или удалением
CloseApplications=yes
RestartApplications=no

[Languages]
Name: "russian"; MessagesFile: "compiler:Languages\Russian.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"
Name: "contextmenu"; Description: "Добавить «Открыть в RustArch» в меню правой кнопки мыши (ПКМ)"; GroupDescription: "Интеграция с Windows:"

[Files]
Source: "target\release\rust_archiver.exe"; DestDir: "{app}"; DestName: "{#MyAppExeName}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Registry]
; Добавляет пункт в ПКМ, если юзер оставил галочку при установке
Root: HKCU; Subkey: "Software\Classes\*\shell\RustArch"; ValueType: string; ValueName: ""; ValueData: "Открыть в RustArch"; Tasks: contextmenu; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\*\shell\RustArch"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\{#MyAppExeName}"",0"; Tasks: contextmenu
Root: HKCU; Subkey: "Software\Classes\*\shell\RustArch\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExeName}"" ""%1"""; Tasks: contextmenu

Root: HKCU; Subkey: "Software\Classes\Directory\shell\RustArch"; ValueType: string; ValueName: ""; ValueData: "Открыть в RustArch"; Tasks: contextmenu; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\Directory\shell\RustArch"; ValueType: string; ValueName: "Icon"; ValueData: """{app}\{#MyAppExeName}"",0"; Tasks: contextmenu
Root: HKCU; Subkey: "Software\Classes\Directory\shell\RustArch\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExeName}"" ""%1"""; Tasks: contextmenu

; Гарантированно чистит реестр при удалении, даже если ПКМ включили уже внутри Настроек программы
Root: HKCU; Subkey: "Software\Classes\*\shell\RustArch"; Flags: dontcreatekey uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\Directory\shell\RustArch"; Flags: dontcreatekey uninsdeletekey

[UninstallDelete]
; При полном удалении сносит папку с config.json (при обновлении на v5 не срабатывает — настройки остаются)
Type: filesandordirs; Name: "{userappdata}\RustArch"

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(MyAppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent