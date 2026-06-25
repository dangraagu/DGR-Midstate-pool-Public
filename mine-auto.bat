@echo off
setlocal EnableExtensions EnableDelayedExpansion
title Midstate Pool Miner - auto-update
color 0a

REM --- What this is -------------------------------------------------------
REM Opt-in Midstate miner launcher: mines on THIS machine, to YOUR own
REM payout address, only while you choose to run it. Not silent or hidden,
REM and does not install or run itself on anyone else's computer. Standard
REM pool miner for the public Midstate proof-of-work chain. See README.
REM ------------------------------------------------------------------------

REM ============================================================
REM  Self-updating launcher. Leave this window open.
REM   * Runs the miner to your address. Midstate's PoW is a
REM     SEQUENTIAL BLAKE3 chain (GPU-resistant), so a good CPU is
REM     only a few x behind a GPU - both builds are worth running.
REM     The gpu build runs OpenCL (and, in hybrid/auto, CPU+GPU together)
REM     in ONE process; the binary's --mode picks cpu/gpu/hybrid/auto.
REM   * Checks GitHub for the latest release every CHECK_MIN
REM     minutes. A new version is gated through THREE checks
REM     before it ever runs (brick-safe hardening):
REM       1. semver compare (the miner's own `check-update`, so
REM          0.1.10 is correctly newer than 0.1.9 - a string
REM          compare got this wrong),
REM       2. download to a TEMP path "%BIN%.new" (NEVER onto the
REM          live running binary - a partial download onto %BIN%
REM          would corrupt it),
REM       3. SHA-256 verify against the release SHA256SUMS (the
REM          miner's own `verify-file`) BEFORE the atomic swap.
REM     A failed verify deletes the temp and keeps the running
REM     binary; the rig never executes an unverified download.
REM   * Liveness is checked on a SHORT cadence (LIVE_SEC),
REM     decoupled from the slow update poll, with ESCALATING
REM     BACKOFF so a crash-looping rig doesn't hammer.
REM  MODE (cpu^|gpu^|hybrid^|auto, default auto) picks which build to download AND
REM  which --mode to run. `auto` auto-detects a GPU: present -^> GPU build (runs
REM  hybrid CPU+GPU), else the CPU build. Force a build via the first arg (cpu^|gpu):
REM     set MODE=hybrid ^& mine-auto.bat      (GPU build, hybrid CPU+GPU)
REM     mine-auto.bat cpu                     (force the CPU build)
REM
REM  Env knobs (all optional):
REM     MODE          cpu ^| gpu ^| hybrid ^| auto       (default auto)
REM     CHECK_MIN     update-poll period in minutes      (default 15)
REM     LIVE_SEC      liveness-check period in seconds    (default 30)
REM     MAX_RESTARTS  rapid restarts before backing off   (default 5)
REM     MIDSTATE_ON_CRASH  path to a .bat run once when the
REM                   restart cap is hit (driver reset, etc.)
REM ============================================================

set "REPO=dangraagu/DGR-Midstate-pool-Public"

REM --- MODE + build-variant resolution --------------------------------------
REM MODE drives the binary's --mode AND which build we download. Default auto.
if not defined MODE set "MODE=auto"
if /i not "%MODE%"=="cpu" if /i not "%MODE%"=="gpu" if /i not "%MODE%"=="hybrid" if /i not "%MODE%"=="auto" (
  echo [X] Unknown MODE "%MODE%". Use one of: cpu ^| gpu ^| hybrid ^| auto
  pause & exit /b 1
)

REM Which BUILD to fetch: first arg wins (cpu^|gpu); else derive from MODE; for
REM auto, detect an NVIDIA/OpenCL GPU and pick the gpu build if present. The gpu
REM build still runs CPU-only if no device is found at runtime (degrades), and the
REM updater falls back to the cpu asset if the gpu asset is missing - never a brick.
set "VARIANT=%~1"
if not defined VARIANT (
  if /i "%MODE%"=="cpu" ( set "VARIANT=cpu"
  ) else if /i "%MODE%"=="gpu" ( set "VARIANT=gpu"
  ) else if /i "%MODE%"=="hybrid" ( set "VARIANT=gpu"
  ) else (
    REM auto: probe for a GPU (NVIDIA name match is a good proxy; any GPU vendor
    REM with an OpenCL ICD also works at runtime). Default cpu on no/false detect.
    set "VARIANT=cpu"
    for /f "usebackq delims=" %%g in (`powershell -NoProfile -Command "$n=((Get-CimInstance Win32_VideoController).Name -join ','); if ($n -match 'NVIDIA|AMD|Radeon|Intel\(R\) Arc'){'gpu'} else {'cpu'}"`) do set "VARIANT=%%g"
  )
)
REM Back-compat: an old caller passing 'nvidia' maps to the gpu build.
if /i "%VARIANT%"=="nvidia" set "VARIANT=gpu"
if /i not "%VARIANT%"=="cpu" if /i not "%VARIANT%"=="gpu" (
  echo [X] Unknown build "%VARIANT%". Use one of: gpu ^| cpu
  pause & exit /b 1
)

set "DIR=%LOCALAPPDATA%\midstate-miner"
if /i "%VARIANT%"=="cpu" ( set "EXE=midstate-miner.exe" ) else ( set "EXE=midstate-miner-gpu.exe" )
set "BIN=%DIR%\%EXE%"
set "CFG=%DIR%\address.txt"
if not defined CHECK_MIN set "CHECK_MIN=15"
if not defined LIVE_SEC set "LIVE_SEC=30"
if not defined MAX_RESTARTS set "MAX_RESTARTS=5"
if not exist "%DIR%" mkdir "%DIR%"

REM -- Launcher self-update: this .bat can refresh ITSELF so a launcher-side fix
REM    reaches the rig, not just the miner binary. SELF_NAME is the release-asset
REM    basename + the SHA256SUMS key; SELF_NEW is the verified staging copy written
REM    mid-run by :update_launcher_self. SELF_SHA persists the wanted SHA-256 beside
REM    SELF_NEW so the startup trampoline can RE-VERIFY the staged bytes before
REM    promoting them onto the live launcher (no-brick: a truncated/corrupt SELF_NEW
REM    is rejected, NOT promoted). Both SELF_NEW and SELF_SHA sit BESIDE %~f0 (same
REM    volume) so staging is an atomic rename, never a cross-volume copy that can
REM    leave a half-written file.
set "SELF_NAME=mine-auto.bat"
set "SELF_NEW=%~f0.new"
set "SELF_SHA=%~f0.new.sha"
REM Bound the trampoline helper's promote-move retries so a persistently-failing
REM swap (locked file, AV, disk full) can never re-fire forever: after this many
REM attempts the helper parks SELF_NEW as .fail and mines on the OLD launcher.
set "SELF_MAX_PROMOTE=5"

echo(
echo  === Midstate Pool Miner - auto-update (build: %VARIANT%) ===
echo(

REM --- payout address (reuse the saved one, else prompt) ---
REM Midstate uses a long hex MSS payout address. We accept whatever hex the user
REM provides and do NOT validate a fixed length (it is NOT a 40-hex addr).
set "ADDR="
if exist "%CFG%" set /p ADDR=<"%CFG%"
if not defined ADDR (
  set /p ADDR=Enter your Midstate payout address ^(hex^):
  > "%CFG%" echo !ADDR!
)
if not defined ADDR ( echo [X] No address entered. & pause & exit /b 1 )

REM The v0.1.1 binary handles ALL hardware itself: --mode picks cpu/gpu/hybrid/auto,
REM the OpenCL backend drives every GPU device, and the hybrid backend runs CPU+GPU
REM concurrently in ONE process. So there is no per-card fan-out and no
REM --device/--gpu-id/--log-dir here (the binary does not take those).
echo Mining to !ADDR! (mode=%MODE%, build=%VARIANT%).
echo Auto-checking GitHub for updates every %CHECK_MIN% min (liveness every %LIVE_SEC%s). Keep this open.
echo(

set "INSTALLED=none"
set "RESTARTS=0"
set "BACKOFF=0"
set "HOOK_FIRED=0"
set "ELAPSED=0"

REM -- NO-BRICK launcher promote (startup trampoline) ---------------------------
REM If a PRIOR run staged a verified new launcher at %SELF_NEW%, promote it now -
REM BEFORE any miner is spawned, so nothing can be bricked. We must NOT move over
REM our own running file in-process (cmd.exe reads the .bat by byte offset; a
REM mid-run replace DERAILS the running loop). Instead we WRITE A SEPARATE HELPER
REM .cmd (simple, robust quoting - no fragile inline nesting), launch it detached,
REM and exit. The helper:
REM   1. waits for THIS process (passed PID) to fully exit,
REM   2. backs up the current launcher to %~f0.bak,
REM   3. moves the staged file onto %~f0 with a BOUNDED retry loop,
REM   4. relaunches the launcher with the original args, then deletes itself.
REM Because we exit FIRST, our file is replaced only after we are gone - this can
REM never corrupt an in-flight run.
REM
REM NO-BRICK RE-VERIFY: stage BESIDE %~f0 (same volume = atomic rename, never a
REM partial file), AND here RE-VERIFY %SELF_NEW%'s SHA-256 against the digest
REM persisted at stage time (%SELF_SHA%) BEFORE handing it to the promote helper.
REM Any mismatch / missing digest / hash failure -> DISCARD the staged file (+ its
REM .sha) and fall through to a normal startup on the GOOD live launcher
REM (FAIL-CLOSED - never a brick). A size-only guard is NOT enough: a truncated
REM but non-zero file would pass it and brick the launcher with
REM `. was unexpected at this time`.
if exist "%SELF_NEW%" (
  for %%S in ("%SELF_NEW%") do set "SELF_NEW_SZ=%%~zS"
  if "!SELF_NEW_SZ!"=="0" (
    REM Zero-byte staged file is anomalous: discard it, keep the running launcher.
    del /f /q "%SELF_NEW%" >nul 2>&1
    del /f /q "%SELF_SHA%" >nul 2>&1
  ) else (
    REM Re-verify the staged bytes against the digest persisted at stage time.
    set "SELF_NEW_WANT="
    if exist "%SELF_SHA%" set /p SELF_NEW_WANT=<"%SELF_SHA%"
    set "SELF_NEW_GOT="
    if defined SELF_NEW_WANT (
      for /f "usebackq delims=" %%h in (`powershell -NoProfile -Command "try { (Get-FileHash -Algorithm SHA256 -LiteralPath '%SELF_NEW%').Hash.ToLower() } catch { '' }"`) do set "SELF_NEW_GOT=%%h"
    )
    if not defined SELF_NEW_WANT (
      echo [%time%] launcher self-update: staged launcher has NO persisted digest - discarding it, keeping the running launcher ^(fail-closed^).
      del /f /q "%SELF_NEW%" >nul 2>&1
      del /f /q "%SELF_SHA%" >nul 2>&1
    ) else if not defined SELF_NEW_GOT (
      echo [%time%] launcher self-update: cannot re-hash the staged launcher ^(Get-FileHash failed^) - discarding it, keeping the running launcher ^(fail-closed^).
      del /f /q "%SELF_NEW%" >nul 2>&1
      del /f /q "%SELF_SHA%" >nul 2>&1
    ) else if /i not "!SELF_NEW_GOT!"=="!SELF_NEW_WANT!" (
      echo [%time%] [X] launcher self-update: staged launcher SHA-256 MISMATCH ^(got !SELF_NEW_GOT! want !SELF_NEW_WANT!^) - TRUNCATED/corrupt, discarding it. Keeping the running launcher ^(fail-closed, no brick^).
      del /f /q "%SELF_NEW%" >nul 2>&1
      del /f /q "%SELF_SHA%" >nul 2>&1
    ) else (
      echo [%time%] launcher self-update: a verified new launcher is staged ^(SHA-256 re-checked^) - applying via safe pre-spawn handoff, then relaunching.
      set "SELF_HELPER=%DIR%\midstate-launcher-promote.cmd"
      set "SELF_CRUMB=%DIR%\midstate-launcher-promote.log"
      REM Build the helper line-by-line with SIMPLE quoting. We deliberately use a
      REM GOTO-based bounded retry (NOT a for/!flag! loop): a generated .cmd would
      REM need escaped delayed-expansion bangs, which are brittle and silently
      REM collapsed - exactly the kind of escape bug that bricks. The label form
      REM needs NO delayed expansion: %%TRIES%% is written literally and re-read
      REM fresh on each :promote_retry pass. The helper waits a few seconds (so our
      REM `exit /b 0` below lands first and our handle on %~f0 is released), backs
      REM up, then promotes with up to SELF_MAX_PROMOTE move attempts. On a
      REM persistently-failing move it parks the staged file as .fail and relaunches
      REM the OLD launcher (so the rig keeps mining and the promote can never re-fire
      REM forever). It checks the relaunch and leaves a breadcrumb on failure.
      REM NOTE: a single-% token (e.g. %~f0, %*) is expanded HERE by the parent and
      REM baked into the helper as the launcher path / original args; a double-%
      REM token (%%~f0, %%TRIES%%, %%date%%) stays literal so it is evaluated by the
      REM HELPER at run time. %%~f0 therefore = the helper's own path (self-delete).
      > "!SELF_HELPER!" echo @echo off
      >>"!SELF_HELPER!" echo ping -n 4 127.0.0.1 ^>nul
      >>"!SELF_HELPER!" echo copy /Y "%~f0" "%~f0.bak" ^>nul 2^>^&1
      >>"!SELF_HELPER!" echo set "TRIES=0"
      >>"!SELF_HELPER!" echo :promote_retry
      >>"!SELF_HELPER!" echo set /a TRIES+=1
      >>"!SELF_HELPER!" echo move /Y "%SELF_NEW%" "%~f0" ^>nul 2^>^&1
      >>"!SELF_HELPER!" echo if not errorlevel 1 goto promote_ok
      >>"!SELF_HELPER!" echo if %%TRIES%% GEQ %SELF_MAX_PROMOTE% goto promote_fail
      >>"!SELF_HELPER!" echo ping -n 3 127.0.0.1 ^>nul
      >>"!SELF_HELPER!" echo goto promote_retry
      >>"!SELF_HELPER!" echo :promote_ok
      >>"!SELF_HELPER!" echo del /f /q "%SELF_SHA%" ^>nul 2^>^&1
      >>"!SELF_HELPER!" echo start "" cmd /c ""%~f0" %*"
      >>"!SELF_HELPER!" echo if errorlevel 1 ^(^> "%SELF_CRUMB%" echo [promote] relaunch of UPDATED launcher failed at %%date%% %%time%% - rerun mine-auto.bat manually.^)
      >>"!SELF_HELPER!" echo goto promote_done
      >>"!SELF_HELPER!" echo :promote_fail
      >>"!SELF_HELPER!" echo move /Y "%SELF_NEW%" "%SELF_NEW%.fail" ^>nul 2^>^&1
      >>"!SELF_HELPER!" echo del /f /q "%SELF_SHA%" ^>nul 2^>^&1
      >>"!SELF_HELPER!" echo ^> "%SELF_CRUMB%" echo [promote] promote-move FAILED after %SELF_MAX_PROMOTE% attempts at %%date%% %%time%% - staged file parked as .fail; mining on the OLD launcher.
      >>"!SELF_HELPER!" echo start "" cmd /c ""%~f0" %*"
      >>"!SELF_HELPER!" echo :promote_done
      >>"!SELF_HELPER!" echo del /f /q "%%~f0" ^>nul 2^>^&1
      REM Launch the helper detached and exit immediately so our file is free.
      start "" /b cmd /c ""!SELF_HELPER!""
      exit /b 0
    )
  )
)

REM Run an update check immediately so we start on the latest published build.
call :update_check

:loop
REM --- fast path: keep the miners alive with escalating backoff ---
if not "!INSTALLED!"=="none" (
  tasklist /FI "IMAGENAME eq %EXE%" 2>nul | find /I "%EXE%" >nul
  if errorlevel 1 (
    REM No miner process is running.
    if !RESTARTS! GEQ %MAX_RESTARTS% (
      if !BACKOFF!==0 ( set "BACKOFF=5" ) else ( set /a BACKOFF=!BACKOFF!*3 )
      if !BACKOFF! GTR 60 set "BACKOFF=60"
      echo [%time%] miners crash-looping ^(!RESTARTS! restarts^) - backing off !BACKOFF!s before retry.
      if !HOOK_FIRED!==0 ( call :run_crash_hook & set "HOOK_FIRED=1" )
      powershell -NoProfile -Command "Start-Sleep -Seconds !BACKOFF!"
    )
    echo [%time%] miners not running - restarting
    call :start_miners
    set /a RESTARTS=!RESTARTS!+1
  ) else (
    REM Healthy this tick: decay the crash-loop state.
    if !RESTARTS! GTR 0 ( set "RESTARTS=0" & set "BACKOFF=0" & set "HOOK_FIRED=0" )
  )
)

REM --- slow path: poll for a new release every CHECK_MIN minutes ---
REM We tick every LIVE_SEC; accumulate elapsed seconds (ELAPSED is initialised
REM to 0 before the loop) and run the update check when we cross CHECK_MIN*60.
set /a ELAPSED=!ELAPSED!+%LIVE_SEC%
set /a UPDATE_EVERY=%CHECK_MIN%*60
if !ELAPSED! GEQ !UPDATE_EVERY! (
  set "ELAPSED=0"
  call :update_check
)

powershell -NoProfile -Command "Start-Sleep -Seconds %LIVE_SEC%"
goto loop

REM ============================================================
REM  Subroutines
REM ============================================================

:update_check
REM Resolve the latest version from the releases/latest/download/ CDN asset
REM latest-version.txt, NOT api.github.com. The unauthenticated API caps at
REM 60 req/hr/IP, so ~20 rigs behind ONE public IP (a farm) get HTTP 403, an empty
REM tag, and the whole farm SILENTLY stops updating. The CDN download path has no
REM such per-IP limit. On offline/404 LATEST stays empty and we cleanly no-op
REM (keep mining) - we do NOT fall back to the rate-limited API.
set "LATEST="
for /f "usebackq delims=" %%v in (`powershell -NoProfile -Command "try { $t=(Invoke-WebRequest -Uri 'https://github.com/%REPO%/releases/latest/download/latest-version.txt' -Headers @{'User-Agent'='midstate-miner'} -UseBasicParsing).Content; ($t -split \"`n\")[0].Trim().TrimStart('v') } catch { '' }"`) do set "LATEST=%%v"
if not defined LATEST goto :eof

REM Decide whether LATEST is newer than INSTALLED. Prefer the miner's OWN
REM check-update (one tested semver compare: 0.1.10 > 0.1.9). If the installed
REM binary is missing or predates the subcommand (first hardened update), fall
REM back to a plain string inequality.
set "DOUPDATE=0"
if exist "%BIN%" (
  "%BIN%" check-update --help >nul 2>&1
  if !errorlevel!==0 (
    REM Subcommand present: exit 0 means "update available".
    "%BIN%" check-update --current "!INSTALLED!" --latest "!LATEST!" >nul 2>&1
    if !errorlevel!==0 ( set "DOUPDATE=1" )
  ) else (
    if not "!LATEST!"=="!INSTALLED!" set "DOUPDATE=1"
  )
) else (
  if not "!LATEST!"=="!INSTALLED!" set "DOUPDATE=1"
)
if "!DOUPDATE!"=="0" goto :eof

echo [%time%] update: !INSTALLED! -^> !LATEST!  ^(verify, then swap + restart^)

REM 1. Download the new binary to a TEMP path - NEVER onto the live %BIN%.
REM    CPU-FALLBACK (mirrors mine-auto.sh download_verify_swap): if a non-cpu
REM    (nvidia) asset 404s / fails to download, re-point EXE/BIN to the CPU build
REM    (midstate-miner.exe, always published) and download THAT instead, so a
REM    Windows rig is NEVER stranded on a missing GPU asset. The re-point updates
REM    VARIANT/EXE/BIN before the SHA lookup + swap below, so the SHA256SUMS key
REM    (%EXE%), the staged temp (%BIN%.new), and the final swap (%BIN%) all track
REM    cpu from here on. Brick-safe: still a TEMP-path download, fail-closed if
REM    even the cpu download fails (keep the running binary, retry next poll).
set "NEWBIN=%BIN%.new"
if exist "!NEWBIN!" del /f /q "!NEWBIN!" >nul 2>&1
curl -L -f -o "!NEWBIN!" "https://github.com/%REPO%/releases/latest/download/%EXE%"
if not !errorlevel!==0 (
  if /i not "%VARIANT%"=="cpu" (
    echo [%time%] [!] '%VARIANT%' build unavailable ^(download failed / 404^). Falling back to the cpu build.
    if exist "!NEWBIN!" del /f /q "!NEWBIN!" >nul 2>&1
    set "VARIANT=cpu"
    set "EXE=midstate-miner.exe"
    set "BIN=%DIR%\midstate-miner.exe"
    set "NEWBIN=%DIR%\midstate-miner.exe.new"
    REM The CPU binary rejects --mode gpu/hybrid; downgrade an explicit GPU mode to
    REM auto so the fallback rig mines on CPU instead of erroring on every start.
    if /i "%MODE%"=="gpu" set "MODE=auto"
    if /i "%MODE%"=="hybrid" set "MODE=auto"
    if exist "!NEWBIN!" del /f /q "!NEWBIN!" >nul 2>&1
    curl -L -f -o "!NEWBIN!" "https://github.com/%REPO%/releases/latest/download/midstate-miner.exe"
    if not !errorlevel!==0 (
      echo [%time%] cpu fallback download failed; keeping current, will retry.
      if exist "!NEWBIN!" del /f /q "!NEWBIN!" >nul 2>&1
      goto :eof
    )
  ) else (
    echo [%time%] download failed; keeping current, will retry.
    if exist "!NEWBIN!" del /f /q "!NEWBIN!" >nul 2>&1
    goto :eof
  )
)

REM 2. Look up the expected SHA-256 from the release SHA256SUMS.
set "WANT="
set "SUMS=%DIR%\SHA256SUMS.tmp"
if exist "!SUMS!" del /f /q "!SUMS!" >nul 2>&1
curl -L -f -s -o "!SUMS!" "https://github.com/%REPO%/releases/latest/download/SHA256SUMS"
if exist "!SUMS!" (
  REM SHA256SUMS lines are "<hex>  <filename>" (sha256sum style; field 1 = 64-hex
  REM digest, field 2 = filename, optionally "*"-prefixed for binary mode). The
  REM published file is generated by Linux `sha256sum` and is LF-ONLY. Do NOT use
  REM `findstr /e` here: /e only recognises CRLF line ends, so on the real LF-only
  REM file it matches NOTHING -> WANT stays empty -> we refuse EVERY update -> the
  REM whole Windows fleet freezes. Use PowerShell instead (LF-safe), mirroring the
  REM Linux awk `$2==a || $2=="*"a {print $1}`: select the line whose EXACT filename
  REM field equals %EXE% (so midstate-miner.exe never matches a
  REM midstate-miner-linux line, a .sig, etc.) and emit its 64-hex digest. Any
  REM not-found / empty / malformed (non-64-hex) result -> empty WANT -> fail-closed.
  for /f "usebackq delims=" %%a in (`powershell -NoProfile -Command "$a='%EXE%'; $h=''; foreach($ln in (Get-Content -LiteralPath '!SUMS!')){ $p=@($ln -split '\s+' ^| Where-Object { $_ -ne '' }); if($p.Count -ge 2 -and ($p[1] -eq $a -or $p[1] -eq ('*'+$a)) -and $p[0] -match '^[0-9A-Fa-f]{64}$'){ $h=$p[0]; break } }; $h"`) do set "WANT=%%a"
  del /f /q "!SUMS!" >nul 2>&1
)

REM 3. Verify before swapping. Prefer the TRUSTED running %BIN%'s verify-file -
REM    never let the just-downloaded staged binary verify itself (a malicious
REM    download would pass its own check). If %BIN% is absent or PREDATES the
REM    verify-file subcommand, fall back to PowerShell Get-FileHash as the OS
REM    trusted verifier - so a pre-verify-file rig can still verify + auto-advance
REM    instead of freezing forever on the old binary.
REM FAIL CLOSED. No SHA256SUMS (or %EXE% not listed), a hash mismatch, or no usable
REM verifier at all REFUSE the update and keep whatever %BIN% exists. Live releases
REM always publish SHA256SUMS, so a missing one is anomalous, not routine. We NEVER
REM swap in an unverified binary.
if not defined WANT (
  echo [%time%] [X] refusing unverified update: no SHA256SUMS published ^(or %EXE% not listed in it^). Keeping the running binary.
  del /f /q "!NEWBIN!" >nul 2>&1
  goto :eof
)
set "VERIFIED=0"
if exist "%BIN%" (
  "%BIN%" verify-file --help >nul 2>&1
  if !errorlevel!==0 (
    "%BIN%" verify-file "!NEWBIN!" "!WANT!" >nul 2>&1
    if !errorlevel!==0 ( set "VERIFIED=1" ) else (
      echo [%time%] [X] SHA-256 verify FAILED for %EXE% - discarding it, keeping the running binary.
      del /f /q "!NEWBIN!" >nul 2>&1
      goto :eof
    )
  )
)
if "!VERIFIED!"=="0" (
  REM No trusted running-binary verifier (first install, or %BIN% predates
  REM verify-file). Use PowerShell Get-FileHash as the OS verifier. FAIL CLOSED if
  REM it is unavailable (empty hash) or the digest does not match.
  set "GOT="
  for /f "usebackq delims=" %%h in (`powershell -NoProfile -Command "try { (Get-FileHash -Algorithm SHA256 -LiteralPath '!NEWBIN!').Hash.ToLower() } catch { '' }"`) do set "GOT=%%h"
  if not defined GOT (
    echo [%time%] [X] refusing unverified update: have a SHA256SUMS digest but no usable verifier ^(no verify-file, Get-FileHash failed^). Keeping current.
    del /f /q "!NEWBIN!" >nul 2>&1
    goto :eof
  )
  if /i not "!GOT!"=="!WANT!" (
    echo [%time%] [X] SHA-256 verify FAILED for %EXE% ^(got !GOT! want !WANT!^) - discarding it.
    del /f /q "!NEWBIN!" >nul 2>&1
    goto :eof
  )
)

REM 4. Verified: stop miners, atomically swap the temp onto the live path, restart.
taskkill /IM "%EXE%" /F >nul 2>&1
move /Y "!NEWBIN!" "%BIN%" >nul
if not !errorlevel!==0 (
  echo [%time%] [X] could not swap in the new binary; keeping current.
  if exist "!NEWBIN!" del /f /q "!NEWBIN!" >nul 2>&1
  REM NO-STRAND: we already taskkill'd the running miner above, so a failed swap
  REM would leave the rig IDLE and re-fire this same failing update every poll
  REM (kill -^> idle -^> kill ...). Bring the OLD binary back up immediately on the
  REM existing %BIN% before bailing, mirroring mine-auto.sh's failed-update branch
  REM (`[ "$INSTALLED" != "none" ] && start_miners`). Only restart once we have an
  REM installed build (skip on a never-yet-installed first poll).
  if not "!INSTALLED!"=="none" call :start_miners
  goto :eof
)
set "INSTALLED=!LATEST!"
set "RESTARTS=0"
set "BACKOFF=0"
set "HOOK_FIRED=0"
call :start_miners
echo [%time%] now mining !INSTALLED! (build: %VARIANT%).
REM Best-effort: also refresh THIS launcher (stage a verified copy for the next
REM start). Runs AFTER mining is back up so it can never delay the restart; it is
REM fail-closed + no-brick (stages only - the actual promote happens via the safe
REM startup trampoline on the next run, never mid-loop).
call :update_launcher_self
goto :eof

REM -- Launcher self-update: download THIS launcher, VERIFY it with a TRUSTED
REM    verifier, and STAGE it at %SELF_NEW% for the no-brick startup trampoline to
REM    promote on the next run. We never replace %~f0 here mid-loop (that derails
REM    the running cmd). FAIL-CLOSED at every step: a download failure, a missing/
REM    unlisted SHA256SUMS entry, a hash mismatch, or no usable verifier all leave
REM    NO staged file (and delete any partial), so the running + on-disk launcher
REM    are untouched. Mirrors mine-auto.sh update_launcher_self.
:update_launcher_self
REM 1. Download the candidate launcher to a temp (NEVER onto %~f0 / %SELF_NEW%
REM    directly until verified). Use a scratch name, promote to %SELF_NEW% only
REM    after verify so a half-download is never seen as "staged" by the trampoline.
REM    CRITICAL (the brick fix): stage BESIDE %~f0 - i.e. on the SAME volume as the
REM    live launcher - NOT under %DIR% (=%LOCALAPPDATA%). The installer puts this
REM    .bat wherever the user clicked, often a DIFFERENT drive than %LOCALAPPDATA%;
REM    a %DIR%->%SELF_NEW% move then crosses volumes and degrades to a non-atomic
REM    copy+delete, so an interrupted copy can leave a TRUNCATED-but-non-zero
REM    %SELF_NEW% that a size-only trampoline would promote onto the live launcher =
REM    brick. A per-run %RANDOM% suffix avoids colliding with a concurrent run's
REM    scratch. Mirrors mine-auto.sh (which stages $SELF_PATH.new.$$ next to itself).
set "SELF_DL=%~f0.dl.%RANDOM%"
if exist "!SELF_DL!" del /f /q "!SELF_DL!" >nul 2>&1
curl -L -f -o "!SELF_DL!" "https://github.com/%REPO%/releases/latest/download/%SELF_NAME%"
if not !errorlevel!==0 (
  echo [%time%] launcher self-update: download failed; keeping current launcher.
  if exist "!SELF_DL!" del /f /q "!SELF_DL!" >nul 2>&1
  goto :eof
)

REM 2. Expected SHA-256 from the SAME release SHA256SUMS, keyed by %SELF_NAME%
REM    (LF-safe PowerShell selector, mirroring the binary path; empty => fail-closed).
set "SELF_WANT="
set "SELF_SUMS=%DIR%\SHA256SUMS.lself"
if exist "!SELF_SUMS!" del /f /q "!SELF_SUMS!" >nul 2>&1
curl -L -f -s -o "!SELF_SUMS!" "https://github.com/%REPO%/releases/latest/download/SHA256SUMS"
if exist "!SELF_SUMS!" (
  for /f "usebackq delims=" %%a in (`powershell -NoProfile -Command "$a='%SELF_NAME%'; $h=''; foreach($ln in (Get-Content -LiteralPath '!SELF_SUMS!')){ $p=@($ln -split '\s+' ^| Where-Object { $_ -ne '' }); if($p.Count -ge 2 -and ($p[1] -eq $a -or $p[1] -eq ('*'+$a)) -and $p[0] -match '^[0-9A-Fa-f]{64}$'){ $h=$p[0]; break } }; $h"`) do set "SELF_WANT=%%a"
  del /f /q "!SELF_SUMS!" >nul 2>&1
)
if not defined SELF_WANT (
  echo [%time%] launcher self-update: no SHA256SUMS entry for %SELF_NAME% - refusing ^(keeping current launcher^).
  del /f /q "!SELF_DL!" >nul 2>&1
  goto :eof
)

REM 3. Verify the temp with a TRUSTED verifier (never let the download verify
REM    itself). Prefer the freshly-swapped %BIN% verify-file; else PowerShell
REM    Get-FileHash (OS verifier). FAIL-CLOSED on mismatch or no verifier.
set "SELF_VERIFIED=0"
if exist "%BIN%" (
  "%BIN%" verify-file --help >nul 2>&1
  if !errorlevel!==0 (
    "%BIN%" verify-file "!SELF_DL!" "!SELF_WANT!" >nul 2>&1
    if !errorlevel!==0 ( set "SELF_VERIFIED=1" ) else (
      echo [%time%] launcher self-update: SHA-256 verify FAILED for %SELF_NAME% - discarding, keeping current launcher.
      del /f /q "!SELF_DL!" >nul 2>&1
      goto :eof
    )
  )
)
if "!SELF_VERIFIED!"=="0" (
  set "SELF_GOT="
  for /f "usebackq delims=" %%h in (`powershell -NoProfile -Command "try { (Get-FileHash -Algorithm SHA256 -LiteralPath '!SELF_DL!').Hash.ToLower() } catch { '' }"`) do set "SELF_GOT=%%h"
  if not defined SELF_GOT (
    echo [%time%] launcher self-update: have a digest but no usable verifier - refusing.
    del /f /q "!SELF_DL!" >nul 2>&1
    goto :eof
  )
  if /i not "!SELF_GOT!"=="!SELF_WANT!" (
    echo [%time%] launcher self-update: SHA-256 verify FAILED for %SELF_NAME% ^(got !SELF_GOT! want !SELF_WANT!^) - discarding.
    del /f /q "!SELF_DL!" >nul 2>&1
    goto :eof
  )
)

REM 4. Skip if the on-disk launcher already matches (no needless staging/churn).
set "SELF_CUR="
for /f "usebackq delims=" %%h in (`powershell -NoProfile -Command "try { (Get-FileHash -Algorithm SHA256 -LiteralPath '%~f0').Hash.ToLower() } catch { '' }"`) do set "SELF_CUR=%%h"
if /i "!SELF_CUR!"=="!SELF_WANT!" (
  del /f /q "!SELF_DL!" >nul 2>&1
  goto :eof
)

REM 5. Promote the VERIFIED temp to the staged slot %SELF_NEW%. Because %SELF_DL%
REM    and %SELF_NEW% now BOTH sit beside %~f0 (same volume), this is a true atomic
REM    rename, NOT a cross-volume copy - %SELF_NEW% is therefore either absent or
REM    byte-complete, never truncated. We FIRST persist the wanted digest to
REM    %SELF_SHA% so the startup trampoline can RE-VERIFY %SELF_NEW% before promoting
REM    it (defence in depth: even if some future bug truncates the staged file, the
REM    trampoline's SHA re-check rejects it and keeps the good live launcher). The
REM    trampoline applies it on the NEXT run, before anything is spawned.
del /f /q "%SELF_SHA%" >nul 2>&1
> "%SELF_SHA%" echo !SELF_WANT!
move /Y "!SELF_DL!" "%SELF_NEW%" >nul
if !errorlevel!==0 (
  echo [%time%] launcher self-update: staged verified %SELF_NAME% ^(+digest^) - it will be re-verified and applied on the next launcher start ^(no-brick^).
) else (
  echo [%time%] launcher self-update: could not stage new launcher; keeping current.
  if exist "!SELF_DL!" del /f /q "!SELF_DL!" >nul 2>&1
  del /f /q "%SELF_SHA%" >nul 2>&1
)
goto :eof

:start_miners
REM ONE process per rig. The binary handles all hardware via --mode (cpu/gpu/
REM hybrid/auto): the OpenCL backend drives every GPU device and the hybrid
REM backend runs CPU+GPU concurrently in-process. No --device/--gpu-id/--log-dir
REM (the v0.1.1 binary does not accept those).
start "Midstate miner (!INSTALLED!, %MODE%/%VARIANT%)" "%BIN%" --address !ADDR! --mode %MODE%
goto :eof

:run_crash_hook
if defined MIDSTATE_ON_CRASH (
  if exist "!MIDSTATE_ON_CRASH!" (
    echo [%time%] running MIDSTATE_ON_CRASH hook: !MIDSTATE_ON_CRASH!
    call "!MIDSTATE_ON_CRASH!"
  ) else (
    echo [%time%] MIDSTATE_ON_CRASH set but "!MIDSTATE_ON_CRASH!" not found - skipping.
  )
)
goto :eof
