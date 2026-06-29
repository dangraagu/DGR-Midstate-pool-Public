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
REM  Override detection:  install-midstate-miner.bat gpu ^| cpu
REM  Set MODE=cpu^|gpu^|hybrid^|auto to choose the run mode (default auto).
REM  GPU drivers are NOT installed here - the gpu build needs your GPU's
REM  OpenCL runtime/driver already present; otherwise use the cpu build.
REM ============================================================

set "REPO=dangraagu/DGR-Midstate-pool-Public"
set "DIR=%LOCALAPPDATA%\midstate-miner"
set "CFG=%DIR%\address.txt"
if not exist "%DIR%" mkdir "%DIR%"

echo(
echo  === Midstate Pool Miner installer ===
echo(

REM --- 1. Pick the build variant (arg overrides auto-detect) ---
REM An NVIDIA card PREFERS the native CUDA build (gpu-cuda, fastest; JITs the
REM committed PTX via the driver, no toolkit); any other GPU vendor (AMD/Intel Arc)
REM selects the OpenCL gpu build; no GPU -> cpu. Every GPU build still runs CPU-only
REM at runtime if no device is found, and the download below falls back
REM gpu-cuda -> gpu -> cpu on a missing asset. MODE (default auto) is passed through
REM to mine-auto.bat at the end.
if not defined MODE set "MODE=auto"
set "VARIANT=%~1"
if not defined VARIANT (
  REM Pipe-free PowerShell (no '|' to mis-escape inside the for/f backticks):
  REM use .Name instead of "| Select-Object". NVIDIA (nvidia-smi OR an NVIDIA
  REM controller) -> gpu-cuda; other GPU vendor -> gpu; else cpu.
  for /f "usebackq delims=" %%i in (`powershell -NoProfile -Command "$nv=$false; try { if (Get-Command nvidia-smi -ErrorAction SilentlyContinue) { $nv=$true } } catch {}; $n=((Get-CimInstance Win32_VideoController).Name -join ','); if ($nv -or $n -match 'NVIDIA'){'gpu-cuda'} elseif ($n -match 'AMD|Radeon|Intel\(R\) Arc'){'gpu'} else {'cpu'}"`) do set "VARIANT=%%i"
)
if not defined VARIANT set "VARIANT=cpu"
REM Back-compat: an old 'nvidia' arg now maps to the CUDA build (preferred NVIDIA
REM path); 'cuda' is an explicit alias for the same.
if /i "%VARIANT%"=="nvidia" set "VARIANT=gpu-cuda"
if /i "%VARIANT%"=="cuda" set "VARIANT=gpu-cuda"
if /i not "%VARIANT%"=="cpu" if /i not "%VARIANT%"=="gpu" if /i not "%VARIANT%"=="gpu-cuda" (
  echo [X] Unknown build "%VARIANT%". Use one of: gpu-cuda ^| gpu ^| cpu
  pause & exit /b 1
)
echo Selected build: %VARIANT%  (mode=%MODE%)

if /i "%VARIANT%"=="cpu" ( set "EXE=midstate-miner.exe"
) else if /i "%VARIANT%"=="gpu-cuda" ( set "EXE=midstate-miner-gpu-cuda.exe"
) else ( set "EXE=midstate-miner-gpu.exe" )
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
REM FALLBACK CHAIN (mirrors mine-auto.bat / install-midstate-miner.sh): if the
REM selected asset 404s / fails, step DOWN to the next-best ALWAYS-published build so
REM an NVIDIA rig is never stranded on a missing asset: gpu-cuda -> gpu -> cpu.
REM Each step re-points EXE/BIN/URL before the SHA verify below tracks the same
REM build. Fail-closed: only the FINAL cpu failure aborts the install.
echo Downloading %EXE% ...
call :dl_one
if !errorlevel! NEQ 0 (
  if /i "%VARIANT%"=="gpu-cuda" (
    echo [!] 'gpu-cuda' build unavailable - falling back to the OpenCL gpu build.
    set "VARIANT=gpu"
    set "EXE=midstate-miner-gpu.exe"
    set "BIN=%DIR%\midstate-miner-gpu.exe"
    set "URL=https://github.com/%REPO%/releases/latest/download/midstate-miner-gpu.exe"
    echo Downloading !EXE! ...
    call :dl_one
  )
)
if !errorlevel! NEQ 0 (
  if /i not "%VARIANT%"=="cpu" (
    echo [!] '%VARIANT%' build unavailable - falling back to the cpu build.
    set "VARIANT=cpu"
    set "EXE=midstate-miner.exe"
    set "BIN=%DIR%\midstate-miner.exe"
    set "URL=https://github.com/%REPO%/releases/latest/download/midstate-miner.exe"
    REM The CPU binary rejects --mode gpu/hybrid; downgrade so it mines on CPU.
    if /i "%MODE%"=="gpu" set "MODE=auto"
    if /i "%MODE%"=="hybrid" set "MODE=auto"
    echo Downloading !EXE! ...
    call :dl_one
  )
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

REM --- 3a. Verify the downloaded binary against the release SHA256SUMS ---
REM Defence in depth on the FIRST fetch (TLS already authenticates the GitHub
REM CDN, but this also catches a truncated download / a tampered asset). FAIL
REM CLOSED: a missing checksums file, a missing/unlisted entry, no verifier, or a
REM hash mismatch deletes the unverified binary and aborts. Nothing is running
REM yet, so aborting can never brick a rig. The SHA256SUMS file is LF-only
REM (Linux sha256sum), so use the LF-safe PowerShell selector (mirrors
REM mine-auto.bat): match the line whose EXACT filename field == %EXE% and emit
REM its 64-hex digest.
echo Verifying %EXE% against the release SHA256SUMS ...
set "WANT="
set "SUMS=%DIR%\SHA256SUMS.install"
set "SUMS_URL=https://github.com/%REPO%/releases/latest/download/SHA256SUMS"
if exist "!SUMS!" del /f /q "!SUMS!" >nul 2>&1
REM Fetch SHA256SUMS with the SAME curl-then-PowerShell fallback the binary
REM download uses, so a Windows box WITHOUT curl can still verify (a no-curl box
REM would otherwise false-fail with an empty WANT). Still FAIL-CLOSED: if neither
REM tool produces the file, !SUMS! is absent, WANT stays empty, and the guard
REM below aborts + deletes the unverified binary.
where curl >nul 2>&1
if !errorlevel!==0 (
  curl -L -f -s -o "!SUMS!" "!SUMS_URL!" >nul 2>&1
) else (
  powershell -NoProfile -Command "try { Invoke-WebRequest -Uri '!SUMS_URL!' -OutFile '!SUMS!' -UseBasicParsing } catch { exit 1 }" >nul 2>&1
)
if exist "!SUMS!" (
  for /f "usebackq delims=" %%a in (`powershell -NoProfile -Command "$a='%EXE%'; $h=''; foreach($ln in (Get-Content -LiteralPath '!SUMS!')){ $p=@($ln -split '\s+' ^| Where-Object { $_ -ne '' }); if($p.Count -ge 2 -and ($p[1] -eq $a -or $p[1] -eq ('*'+$a)) -and $p[0] -match '^[0-9A-Fa-f]{64}$'){ $h=$p[0]; break } }; $h"`) do set "WANT=%%a"
  del /f /q "!SUMS!" >nul 2>&1
)
if not defined WANT (
  echo(
  echo [X] Could not verify %EXE% ^(no SHA256SUMS published, or %EXE% not listed^).
  echo     Refusing to install an unverified binary.
  echo     Releases: https://github.com/%REPO%/releases/latest
  echo(
  if exist "%BIN%" del /f /q "%BIN%" >nul 2>&1
  pause
  exit /b 1
)
set "GOT="
for /f "usebackq delims=" %%h in (`powershell -NoProfile -Command "try { (Get-FileHash -Algorithm SHA256 -LiteralPath '%BIN%').Hash.ToLower() } catch { '' }"`) do set "GOT=%%h"
if not defined GOT (
  echo [X] No usable verifier ^(Get-FileHash failed^) - refusing the install.
  if exist "%BIN%" del /f /q "%BIN%" >nul 2>&1
  pause
  exit /b 1
)
if /i not "!GOT!"=="!WANT!" (
  echo [X] SHA-256 verify FAILED for %EXE% ^(got !GOT! want !WANT!^) - aborting install.
  if exist "%BIN%" del /f /q "%BIN%" >nul 2>&1
  pause
  exit /b 1
)
echo   -^> verified ^(!WANT!^).

REM --- 3b. Also fetch the auto-update launcher next to this file ---
REM Fetch mine-auto.bat from the RELEASE ASSET, NOT raw.githubusercontent main.
REM The raw main blob is the committed file, which .gitattributes stores LF-only
REM (`*.bat text eol=crlf` only normalises on the runner/working-tree, not the raw
REM API blob) - a freshly-installed rig would then run an LF .bat until its first
REM self-update. The release asset is CRLF (staged from the runner working tree),
REM SHA-covered by SHA256SUMS, and is the EXACT same artifact mine-auto.bat's own
REM self-update path pulls - so install and self-update converge on identical bytes.
set "MINEAUTO_URL=https://github.com/%REPO%/releases/latest/download/mine-auto.bat"
echo Fetching the auto-update launcher ...
where curl >nul 2>&1
if !errorlevel!==0 (
  curl -L -f -s -o "%~dp0mine-auto.bat" "!MINEAUTO_URL!"
) else (
  powershell -NoProfile -Command "try { Invoke-WebRequest -Uri '!MINEAUTO_URL!' -OutFile '%~dp0mine-auto.bat' -UseBasicParsing } catch {}"
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
exit /b 0

REM ============================================================
REM  Subroutines
REM ============================================================

REM Download %URL% -> %BIN% using curl (preferred) or PowerShell, returning the
REM tool's exit code so the fallback chain above can step down on a 404/failure.
REM Used for the gpu-cuda -> gpu -> cpu asset fallback. Note: a `goto :eof` / RETURN
REM here propagates the LAST command's errorlevel to the caller's `if errorlevel`.
:dl_one
where curl >nul 2>&1
if !errorlevel!==0 (
  curl -L -f -o "%BIN%" "%URL%"
) else (
  powershell -NoProfile -Command "try { Invoke-WebRequest -Uri '%URL%' -OutFile '%BIN%' -UseBasicParsing } catch { exit 1 }"
)
goto :eof
