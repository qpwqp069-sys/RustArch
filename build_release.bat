@echo off
chcp 65001 >nul
setlocal EnableDelayedExpansion
cd /d "%~dp0"

echo ========================================
echo      RustArch — Автосборка релиза
echo ========================================
echo.

set "VER="
for /f "tokens=2 delims==" %%a in ('findstr /r "^version" Cargo.toml') do (
    if not defined VER (
        for %%v in (%%a) do set "VER=%%~v"
    )
)

if not defined VER (
    echo [ОШИБКА] Не удалось прочитать версию из Cargo.toml
    pause
    exit /b 1
)

echo Версия из Cargo.toml: v!VER!
echo.

echo [1/3] Сборка cargo build --release...
cargo build --release
if errorlevel 1 (
    echo [ОШИБКА] Сборка Rust завершилась с ошибкой.
    pause
    exit /b 1
)

if not exist "dist" mkdir dist

echo [2/3] Копирование Portable-версии...
copy /Y "target\release\rust_archiver.exe" "dist\RustArch_Portable_v!VER!.exe" >nul
if errorlevel 1 (
    echo [ОШИБКА] Не удалось скопировать Portable-файл.
    pause
    exit /b 1
)

echo [3/3] Поиск Inno Setup и сборка установщика...
set "ISCC="
if exist "%ProgramFiles(x86)%\Inno Setup 6\ISCC.exe" set "ISCC=%ProgramFiles(x86)%\Inno Setup 6\ISCC.exe"
if not defined ISCC if exist "%ProgramFiles%\Inno Setup 6\ISCC.exe" set "ISCC=%ProgramFiles%\Inno Setup 6\ISCC.exe"
if not defined ISCC if exist "%LocalAppData%\Programs\Inno Setup 6\ISCC.exe" set "ISCC=%LocalAppData%\Programs\Inno Setup 6\ISCC.exe"

if not defined ISCC (
    where iscc >nul 2>&1
    if not errorlevel 1 set "ISCC=iscc"
)

if not defined ISCC (
    echo [ПРЕДУПРЕЖДЕНИЕ] ISCC.exe не найден. Portable готов, а installer.iss скомпилируй вручную через Ctrl+F9.
) else (
    "!ISCC!" /Q "installer.iss"
    if errorlevel 1 (
        echo [ОШИБКА] Inno Setup не смог собрать installer.iss.
        pause
        exit /b 1
    )
)

echo.
echo ========================================
echo Готово! Файлы релиза лежат в папке dist\:
dir /B "dist\*v!VER!*.exe"
echo ========================================
pause