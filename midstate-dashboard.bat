@echo off
REM midstate-dashboard.bat - live terminal dashboard for the Midstate pool miner (Windows).
REM Licensed under PolyForm Perimeter 1.0.0 (see LICENSE). Part of midstate-pool-miner.
REM
REM READ-ONLY remote viewer: GETs the PUBLIC pool API
REM (<base>/api/miner/<address> + <base>/api/history) once per refresh and draws
REM it. Never writes config, never touches the miner binary or the share/submit
REM path, never opens a socket to anything but the public pool over HTTPS. Worst
REM case it prints "endpoint unreachable / set your address". It cannot stop,
REM slow, or corrupt mining. It reads the POOL's stats, so it works with any
REM fleet version you mine with.
REM
REM Usage: midstate-dashboard.bat [--address ADDR] [--refresh N] [--once] [--no-color] [--update] [-h]
setlocal EnableExtensions EnableDelayedExpansion

set "MID_DASH_API=%MIDSTATE_POOL_API%"
if "%MID_DASH_API%"=="" set "MID_DASH_API=https://midstate.yamaduo.no"
set "MID_DASH_ADDR=%MIDSTATE_ADDR%"
set "MID_DASH_REFRESH=%MIDSTATE_REFRESH%"
if "%MID_DASH_REFRESH%"=="" set "MID_DASH_REFRESH=5"
set "MID_DASH_ONCE=0"
set "MID_DASH_NOCOLOR=0"
set "MID_DASH_UPDATE=0"
REM capture the script path NOW -- SHIFT in the parse loop also shifts %0
set "MID_DASH_SELF=%~f0"

:parse
if "%~1"=="" goto parsed
if /i "%~1"=="--address" ( set "MID_DASH_ADDR=%~2" & shift & shift & goto parse )
if /i "%~1"=="--refresh" ( set "MID_DASH_REFRESH=%~2" & shift & shift & goto parse )
if /i "%~1"=="--once" ( set "MID_DASH_ONCE=1" & shift & goto parse )
if /i "%~1"=="--no-color" ( set "MID_DASH_NOCOLOR=1" & shift & goto parse )
if /i "%~1"=="--update" ( set "MID_DASH_UPDATE=1" & shift & goto parse )
if /i "%~1"=="-h" goto help
if /i "%~1"=="--help" goto help
echo unknown argument: %~1 1>&2
goto help

:help
echo midstate-dashboard.bat - live Midstate pool miner dashboard ^(read-only viewer^)
echo.
echo   --address ADDR  your Midstate payout address ^(default: %%MIDSTATE_ADDR%% or
echo                   the address the miner saved under %%LOCALAPPDATA%%\midstate-miner\address.txt^)
echo   --refresh N     seconds between refreshes ^(default: 5^)
echo   --once          print one frame and exit
echo   --no-color      disable color
echo   --update        self-update this script from the latest release ^(fail-closed^)
echo   -h, --help      this help
echo.
echo Pool API base: %%MIDSTATE_POOL_API%% overrides the default ^(https://midstate.yamaduo.no^).
echo Reads only the PUBLIC pool stats API -- never the miner binary or share path.
endlocal & exit /b 0

:parsed
REM strip a trailing slash off the API base so "<base>/api/..." never doubles up
if "%MID_DASH_API:~-1%"=="/" set "MID_DASH_API=%MID_DASH_API:~0,-1%"

REM resolve payout address: --address / MIDSTATE_ADDR > saved file
if "%MID_DASH_ADDR%"=="" (
  set "_CFG=%LOCALAPPDATA%\midstate-miner\address.txt"
  if exist "!_CFG!" set /p MID_DASH_ADDR=<"!_CFG!"
)

if "%MID_DASH_UPDATE%"=="1" goto selfupdate

REM Hand off to the PowerShell render: extract the PS section (every line after
REM the marker) to a temp script and run it. A temp render script in %TEMP% is
REM harmless -- it never touches the miner, its config, or the share path.
set "PSL="
for /f "delims=:" %%a in ('findstr /n /b /c:"REM PS_SECTION_BELOW" "%MID_DASH_SELF%"') do if not defined PSL set "PSL=%%a"
set "PSF=%TEMP%\midstate-dash-%RANDOM%%RANDOM%.ps1"
more +%PSL% "%MID_DASH_SELF%" > "%PSF%"
powershell -NoProfile -ExecutionPolicy Bypass -File "%PSF%"
set "RC=%ERRORLEVEL%"
del "%PSF%" >nul 2>&1
endlocal & exit /b %RC%

:selfupdate
set "MID_DL=https://github.com/dangraagu/DGR-Midstate-pool-Public/releases/latest/download"
powershell -NoProfile -ExecutionPolicy Bypass -Command "$ErrorActionPreference='Stop'; $name='midstate-dashboard.bat'; $dl=$env:MID_DL; $self=$env:MID_DASH_SELF; try { $sums=(Invoke-WebRequest -UseBasicParsing -Uri \"$dl/SHA256SUMS\" -TimeoutSec 15).Content; $want=($sums -split \"`n\" | Where-Object { $_ -match ('(?i)\s\*?'+[regex]::Escape($name)+'\s*$') } | ForEach-Object { ($_ -split '\s+')[0] } | Select-Object -First 1); if(-not $want){ Write-Error 'not in SHA256SUMS; refusing'; exit 1 }; $tmp=\"$self.new\"; Invoke-WebRequest -UseBasicParsing -Uri \"$dl/$name\" -OutFile $tmp -TimeoutSec 30; $got=(Get-FileHash -Algorithm SHA256 -LiteralPath $tmp).Hash; if($got -ne $want){ Remove-Item $tmp -Force; Write-Error 'checksum mismatch; kept current'; exit 1 }; Copy-Item -LiteralPath $self -Destination \"$self.bak\" -Force; Move-Item -LiteralPath $tmp -Destination $self -Force; Write-Host \"updated $name (prior at $name.bak)\" } catch { Write-Error $_; exit 1 }"
endlocal & exit /b %ERRORLEVEL%

REM PS_SECTION_BELOW
$api     = $env:MID_DASH_API
$address = $env:MID_DASH_ADDR
$refresh = [int]($env:MID_DASH_REFRESH); if ($refresh -lt 1) { $refresh = 5 }
$once    = $env:MID_DASH_ONCE -eq '1'
$nocolor = $env:MID_DASH_NOCOLOR -eq '1'
# force '.' decimals + no locale group separators, identical on every machine
try { [System.Threading.Thread]::CurrentThread.CurrentCulture = [System.Globalization.CultureInfo]::InvariantCulture } catch {}

$script:prevGood = $null
$script:prevTs   = $null

function HR([double]$v) {
  $u='H/s'
  if     ($v -ge 1e12){ $v/=1e12; $u='TH/s' }
  elseif ($v -ge 1e9 ){ $v/=1e9 ; $u='GH/s' }
  elseif ($v -ge 1e6 ){ $v/=1e6 ; $u='MH/s' }
  elseif ($v -ge 1e3 ){ $v/=1e3 ; $u='kH/s' }
  '{0:F2} {1}' -f $v,$u
}
function UP([long]$s) {
  $h=[math]::Floor($s/3600); $m=[math]::Floor(($s%3600)/60); $x=$s%60
  if($h -gt 0){ "{0}h {1}m {2}s" -f $h,$m,$x } elseif($m -gt 0){ "{0}m {1}s" -f $m,$x } else { "{0}s" -f $x }
}
function ADDR([string]$a) { if($a -and $a.Length -gt 14){ $a.Substring(0,6)+'..'+$a.Substring($a.Length-4) } else { $a } }

function Col([string]$txt,[string]$c) {
  if($nocolor){ Write-Host -NoNewline $txt } else { Write-Host -NoNewline $txt -ForegroundColor $c }
}
function Line([string[]]$parts,[string[]]$cols) {
  for($i=0;$i -lt $parts.Count;$i++){ Col $parts[$i] $cols[$i] }
  $pad = 0
  try { $pad = [Console]::WindowWidth - 1 } catch { $pad = 60 }
  $len = ($parts -join '').Length
  if($len -lt $pad){ Write-Host (' ' * ($pad-$len)) } else { Write-Host '' }
}

# pull the LAST history sample (pool_hs / net_hs at "now")
function HistLast {
  try {
    $h = Invoke-RestMethod -Uri "$api/api/history" -TimeoutSec 6
    if($h.samples -and $h.samples.Count -gt 0){
      $s = $h.samples[$h.samples.Count-1]
      return @([double]$s.pool_hs, [double]$s.net_hs)
    }
  } catch {}
  return @(0.0, 0.0)
}

function Draw {
  if(-not $once){ try { [Console]::SetCursorPosition(0,0) } catch {} }

  if(-not $address){
    Line @('  Midstate Pool Miner') @('Cyan')
    Line @('') @('Gray')
    Line @('  * set your payout address') @('Yellow')
    Line @('') @('Gray')
    Line @('  Pass --address <ADDR>, set MIDSTATE_ADDR, or run the miner once') @('Gray')
    Line @('  (it saves your address under %LOCALAPPDATA%\midstate-miner\address.txt).') @('Gray')
    if(-not $once){ Line @('  retrying every '+$refresh+'s - Ctrl-C to quit') @('DarkGray') }
    return
  }

  $r = $null
  try { $r = Invoke-RestMethod -Uri "$api/api/miner/$address" -TimeoutSec 6 } catch { $r = $null }

  if($null -eq $r){
    Line @('  Midstate Pool Miner','   '+(ADDR $address)) @('Cyan','DarkGray')
    Line @('') @('Gray')
    Line @('  * pool API unreachable') @('Red')
    Line @('  '+"$api/api/miner/$address") @('DarkGray')
    Line @('') @('Gray')
    Line @('  Is '+$api+' reachable, and is the address correct?') @('Gray')
    Line @('  Override the base with MIDSTATE_POOL_API, the address with --address.') @('Gray')
    Line @('  (A brand-new miner has no stats until its first accepted share.)') @('DarkGray')
    if(-not $once){ Line @('  retrying every '+$refresh+'s - Ctrl-C to quit') @('DarkGray') }
    return
  }

  $addr = $r.address; if(-not $addr){ $addr=$address }
  $h5=[double]$r.hr5m_hs; $h1=[double]$r.hr1h_hs; $h6=[double]$r.hr6h_hs
  $acc=[double]$r.shares_accepted; $rej=[double]$r.shares_rejected
  $lastdiff=[double]$r.last_difficulty
  $workers=[int]$r.connected_workers; $contrib=[double]$r.contribution_pct
  $eph=$r.est_csd_per_hour; $eps=$r.est_csd_this_session
  $sess=[long]$r.session_secs
  # CSD amounts arrive as strings — print as-is, defaulting to '0'
  $pend=$r.pending_csd; if($null -eq $pend){ $pend='0' }
  $owed=$r.owed_csd;    if($null -eq $owed){ $owed='0' }
  $life=$r.lifetime_paid_csd; if($null -eq $life){ $life='0' }

  $total = $acc + $rej
  $rejpct = if($total -gt 0){ $rej*100/$total } else { 0 }
  $rejcol = if($rejpct -gt 5){'Red'} elseif($rejpct -gt 1){'Yellow'} else {'Green'}

  $now=[int][DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
  $rate='-'
  if($null -ne $script:prevGood -and $now -gt $script:prevTs){
    $d=$acc-$script:prevGood; if($d -lt 0){$d=0}
    $rate=('{0:F1}/min' -f ($d*60/($now-$script:prevTs)))
  }
  $script:prevGood=$acc; $script:prevTs=$now

  $hist = HistLast
  $poolHs = [double]$hist[0]; $netHs = [double]$hist[1]
  $dom = if($netHs -gt 0){ $poolHs*100/$netHs } else { 0 }

  Line @('  Midstate Pool Miner','   '+(ADDR $addr)) @('Cyan','DarkGray')
  Line @('  Pool    ',$api,'    Uptime  ',(UP $sess)) @('Gray','White','Gray','White')
  Line @('  ----------------------------------------------------') @('DarkGray')
  Line @('  HASHRATE   5m ',(HR $h5),'  1h ',(HR $h1),'  6h ',(HR $h6)) @('White','Cyan','Gray','Cyan','Gray','Cyan')
  Line @('  ----------------------------------------------------') @('DarkGray')
  Line @('  SHARES     acc ',("{0:F0}" -f $acc),'  rej ',("{0:F0}" -f $rej),'  reject% ',('{0:F2}%' -f $rejpct)) @('White','Green','Gray',$rejcol,'Gray',$rejcol)
  Line @('             rate ',$rate) @('Gray','White')
  Line @('  ----------------------------------------------------') @('DarkGray')
  Line @('  POOL       workers ',("{0}" -f $workers),'   contrib ',('{0:F2}%' -f $contrib),'   diff ',('{0:G}' -f $lastdiff)) @('White','Cyan','Gray','Cyan','Gray','Cyan')
  Line @('  ----------------------------------------------------') @('DarkGray')
  Line @('  EARNINGS   est/h ',("{0}" -f $eph),'   est/session ',("{0}" -f $eps)) @('White','Cyan','Gray','Cyan')
  Line @('             pending ',("{0}" -f $pend),'   owed ',("{0}" -f $owed),'   paid ',("{0}" -f $life)) @('Gray','White','Gray','White','Gray','White')
  Line @('  ----------------------------------------------------') @('DarkGray')
  Line @('  NETWORK    pool ',(HR $poolHs),'  net ',(HR $netHs),'  dom ',('{0:F2}%' -f $dom)) @('White','Cyan','Gray','Cyan','Gray','Cyan')
  if(-not $once){ Line @('  refresh '+$refresh+'s - Ctrl-C to quit') @('DarkGray') }
}

if($once){ Draw; return }
try { [Console]::CursorVisible=$false } catch {}
try { Clear-Host } catch {}
try {
  while($true){ Draw; Start-Sleep -Seconds $refresh }
} finally {
  try { [Console]::CursorVisible=$true } catch {}
}
