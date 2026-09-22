@echo off
setlocal
cd /d "%~dp0"

REM Discord Quest Helper - dev launcher.
REM The repo pins pnpm 11.18.0 via the "packageManager" field, but a global
REM pnpm 9 cannot parse pnpm-workspace.yaml. This routes every nested "pnpm"
REM call through corepack's pinned version.

set "PNPM_SHIM=%TEMP%\dqh-pnpm-shim"
if not exist "%PNPM_SHIM%" mkdir "%PNPM_SHIM%"
> "%PNPM_SHIM%\pnpm.cmd" echo @echo off
>>"%PNPM_SHIM%\pnpm.cmd" echo corepack pnpm %%*
set "PATH=%PNPM_SHIM%;%PATH%"

if not exist "node_modules" (
  echo [dqh] node_modules missing - installing dependencies...
  call pnpm install --frozen-lockfile
  if errorlevel 1 (
    echo [dqh] pnpm install failed.
    endlocal
    exit /b 1
  )
)

echo [dqh] Launching Tauri dev. The first Rust build can take a few minutes...
call pnpm run tauri:dev

set "EXIT_CODE=%ERRORLEVEL%"
echo [dqh] Tauri dev exited with code %EXIT_CODE%.
endlocal
exit /b %EXIT_CODE%