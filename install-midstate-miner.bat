@echo off
setlocal EnableExtensions EnableDelayedExpansion
title Midstate Pool Miner - one-click installer
color 0a

REM ============================================================
REM  Midstate Pool Miner - all-in-one installer for Windows.
REM  Double-click. It will:
REM    1. Detect your GPU (NVIDIA) or fall back to CPU. Midstate's
REM       PoW is a SEQUENTIAL BLAKE3 chain (GPU-resistant), so the
REM       CPU build is genuinely competitive - only a few x behind.
REM    2. Install the Microsoft VC++ runtime via winget if missing.
REM    3. Download the matching prebuilt miner from GitHub Releases.
REM    4. Ask for your Midstate payout address once (and remember it).
REM    5. Start mining to the pool.
REM  Override detection:  install-midstate-miner.bat nvidia ^| cpu
REM  GPU drivers are NOT installed here - the nvidia build needs your
REM  NVIDIA driver already present; otherwise use the cpu build.
REM ============================================================

set "REPO=dangraagu/DGR-Midstate-pool-Public"
set "DIR=%LOCALAPPDATA%\midstate-miner"
set "CFG=%DIR%\address.txt"
if not exist "%DIR%" mkdir "%DIR%"

echo(
echo  === Midstate Pool Miner installer ===
echo(

REM --- 1. Pick the build variant (arg overrides auto-detect) ---
set "VARIANT=%~1"
if not defined VARIANT (
  REM Pipe-free PowerShell (no '|' to mis-escape inside the for/f backticks):
  REM use .Name instead of "| Select-Object". NVIDIA -> nvidia, else cpu.
  for /f "usebackq delims=" %%i in (`powershell -NoProfile -Command "$n=((Get-CimInstance Win32_VideoController).Name -join ','); if ($n -match 'NVIDIA'){'nvidia'} else {'cpu'}"`) do set "VARIANT=%%i"
)
if not defined VARIANT set "VARIANT=cpu"
echo Selected build: %VARIANT%

if /i "%VARIANT%"=="cpu" ( set "EXE=midstate-miner.exe" ) else ( set "EXE=midstate-miner-%VARIANT%.exe" )
set "BIN=%DIR%\%EXE%"
set "URL=https://github.com/%REPO%/releases/latest/download/%EXE%"

REM --- 2. VC++ runtime via winget (best-effort; skipped if absent) ---
where winget >nul 2>&1
if !errorlevel!==0 (
  winget list --id Microsoft.VCRedist.2015+.x64 -e >nul 2>&1
  if !errorlevel! NEQ 0 (
    echo Installing Microsoft VC++ runtime...
    winget install --id Microsoft.VCRedist.2015+.x64 -e --silent --accept-source-agreements --accept-package-agreements
  ) else (
    echo VC++ runtime already present.
  )
) else (
  echo winget not found - skipping VC++ check ^(usually already installed^).
)

REM --- 3. Download the matching miner ---
echo Downloading %EXE% ...
where curl >nul 2>&1
if !errorlevel!==0 (
  curl -L -f -o "%BIN%" "%URL%"
) else (
  powershell -NoProfile -Command "try { Invoke-WebRequest -Uri '%URL%' -OutFile '%BIN%' -UseBasicParsing } catch { exit 1 }"
)
if !errorlevel! NEQ 0 (
  echo(
  echo [X] Download failed. Either no release is published yet, the
  echo     '%VARIANT%' build isn't in the latest release, or no network.
  echo     Releases: https://github.com/%REPO%/releases/latest
  echo     Tip: try another build, e.g.  install-midstate-miner.bat cpu
  echo(
  pause
  exit /b 1
)

REM --- 3b. Also fetch the auto-update launcher next to this file ---
echo Fetching the auto-update launcher ...
where curl >nul 2>&1
if !errorlevel!==0 (
  curl -L -f -s -o "%~dp0mine-auto.bat" "https://raw.githubusercontent.com/%REPO%/main/mine-auto.bat"
) else (
  powershell -NoProfile -Command "try { Invoke-WebRequest -Uri 'https://raw.githubusercontent.com/%REPO%/main/mine-auto.bat' -OutFile '%~dp0mine-auto.bat' -UseBasicParsing } catch {}"
)
echo   - mine-auto.bat      = auto-update launcher (recommended for 24/7)

REM --- 4. Midstate payout address: prompt once, remember thereafter ---
REM Midstate uses a long hex MSS payout address. We accept whatever hex the user
REM provides and do NOT validate a fixed length (it is NOT a 40-hex addr).
set "ADDR="
if exist "%CFG%" set /p ADDR=<"%CFG%"
if not defined ADDR (
  echo(
  echo Enter YOUR Midstate payout address ^(hex^) - where the pool sends
  echo your mining rewards:
  set /p ADDR=^>
  > "%CFG%" echo !ADDR!
)
if not defined ADDR (
  echo [X] No address entered. Re-run and provide your Midstate address.
  pause
  exit /b 1
)

REM --- 5. Mine (hand off to the self-updating launcher) ---
REM IMPORTANT: we do NOT run the raw binary here. Stranding a rig on an old
REM version is exactly what this fleet must avoid, so the one-click install ends
REM by handing off to mine-auto.bat - which keeps polling GitHub and swaps in
REM newer VERIFIED builds for as long as its window stays open. mine-auto.bat
REM reuses the address we just saved to %CFG% (no re-prompt).
echo(
echo Starting %VARIANT% miner via the self-updating launcher (mine-auto.bat).
echo Payout address: !ADDR!   ^(change it later by deleting: %CFG%^)
echo It auto-checks GitHub for updates and verifies each download before swapping it in.
echo Press Ctrl+C to stop.
echo(
if exist "%~dp0mine-auto.bat" (
  call "%~dp0mine-auto.bat" %VARIANT%
) else (
  REM FAIL-SAFE: launcher missing -> run the installed+verified binary directly
  REM so the rig still mines (just without auto-update). Re-download mine-auto.bat
  REM for 24/7 rigs.
  echo [!] mine-auto.bat not found next to the installer; running the installed
  echo     binary directly ^(no auto-update^).
  "%BIN%" --address !ADDR!
  echo(
  echo Miner stopped.
  pause
)
endlocal
