@echo off
setlocal EnableExtensions

rem One-click local smoke-test launcher (Windows, native Rust target).
rem Edit these two ports if they are already occupied.
set "STABLE_PORT=9223"
set "PTB_PORT=9224"

pushd "%~dp0.."
if errorlevel 1 goto :fail

where corepack >nul 2>nul
if errorlevel 1 (
  echo ERROR: Corepack was not found. Install Node.js with Corepack enabled.
  goto :fail
)
where rustc >nul 2>nul
if errorlevel 1 (
  echo ERROR: rustc was not found. Install the Rust toolchain first.
  goto :fail
)

set "TARGET_TRIPLE="
for /f "tokens=2 delims=: " %%T in ('rustc -vV ^| findstr /b "host:"') do set "TARGET_TRIPLE=%%T"
if defined TAURI_TARGET_TRIPLE set "TARGET_TRIPLE=%TAURI_TARGET_TRIPLE%"
if defined CARGO_BUILD_TARGET set "TARGET_TRIPLE=%CARGO_BUILD_TARGET%"
if not defined TARGET_TRIPLE (
  echo ERROR: Could not determine the Rust target triple.
  goto :fail
)

echo.
echo Building Helper sidecars for %TARGET_TRIPLE%...
call corepack pnpm run sync-version
if errorlevel 1 goto :fail
call corepack pnpm run build:runner
if errorlevel 1 goto :fail
call corepack pnpm run build:cdp-launcher
if errorlevel 1 goto :fail

set "CDP_LAUNCHER=%CD%\src-tauri\binaries\waybridge-%TARGET_TRIPLE%.exe"
if not exist "%CDP_LAUNCHER%" (
  echo ERROR: CDP launcher was not built: "%CDP_LAUNCHER%"
  goto :fail
)

echo.
echo Starting Discord Stable with CDP on port %STABLE_PORT%...
echo If it is already running without CDP, confirm the restart dialog.
start "" /wait "%CDP_LAUNCHER%" --port %STABLE_PORT% --channel stable
if errorlevel 1 goto :fail
call :wait_for_cdp %STABLE_PORT%
if errorlevel 1 (
  echo ERROR: Stable did not become CDP-ready on port %STABLE_PORT%.
  goto :fail
)

echo.
echo Starting Discord PTB with CDP on port %PTB_PORT%...
echo If it is already running without CDP, confirm the restart dialog.
start "" /wait "%CDP_LAUNCHER%" --port %PTB_PORT% --channel ptb
if errorlevel 1 goto :fail
call :wait_for_cdp %PTB_PORT%
if errorlevel 1 (
  echo ERROR: PTB did not become CDP-ready on port %PTB_PORT%.
  echo Confirm PTB is installed and that the port is not occupied by another app.
  goto :fail
)

echo.
echo Both clients are CDP-ready. Starting Discord Quest Helper...
echo Stable: %STABLE_PORT%   PTB: %PTB_PORT%
echo In Helper, keep the global default at %STABLE_PORT%; add the second account on %PTB_PORT%.
call corepack pnpm exec node scripts/run-tauri-dev.js
set "EXIT_CODE=%ERRORLEVEL%"
goto :finish

:wait_for_cdp
set "CHECK_PORT=%~1"
powershell -NoProfile -ExecutionPolicy Bypass -Command "$uri='http://127.0.0.1:%CHECK_PORT%/json/version'; $deadline=(Get-Date).AddSeconds(60); do { try { $response=Invoke-WebRequest -UseBasicParsing -Uri $uri -TimeoutSec 2; if ($response.StatusCode -eq 200) { exit 0 } } catch {}; Start-Sleep -Seconds 1 } while ((Get-Date) -lt $deadline); exit 1"
exit /b %ERRORLEVEL%

:fail
set "EXIT_CODE=1"

:finish
popd
if not "%EXIT_CODE%"=="0" pause
exit /b %EXIT_CODE%
