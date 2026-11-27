<#
    Baut Jamcenter und spielt es auf dem Server ein:
      - Server (Relay) für Linux (statisch, musl) als Dienst jamcenter-relay
      - Windows-App als Download (liegt nur auf dem Server, nicht in Git)
    Die App bettet die Server-Programme für die Anleitung „Eigener Server“ ein – deshalb erst Relay, dann App bauen.
    Aufruf:  powershell -NoProfile -File hochladen.ps1 [-NurApp] [-NurRelay]
    Ziel steht in ziel.local.ps1 (nicht in Git):
      $SshZiel      = 'root@1.2.3.4'                 # SSH-Zugang (Schlüssel), Pflicht
      $StandardServer = 'jam.example.org'            # Vorgabe im Feld „Server“ (optional)
      $DownloadPfad = '/var/www/.../Jamcenter.exe'   # leer = App nicht hochladen
      $DownloadBesitzer = 'www-data:www-data'
      $DownloadUrl  = 'https://…/Jamcenter.exe'
    Umgebungsvariable JAMCENTER_SSH überschreibt $SshZiel.
#>
param([switch]$NurApp, [switch]$NurRelay)
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot
$SshZiel = ''; $StandardServer = ''; $DownloadPfad = ''; $DownloadBesitzer = 'root:root'; $DownloadUrl = ''
if (Test-Path "$PSScriptRoot\ziel.local.ps1") { . "$PSScriptRoot\ziel.local.ps1" }
if ($env:JAMCENTER_SSH) { $SshZiel = $env:JAMCENTER_SSH }
if (-not $SshZiel -and -not $NurApp) { throw 'Kein $SshZiel (ziel.local.ps1 oder JAMCENTER_SSH)' }
if ($StandardServer) { $env:JAMCENTER_SERVER = $StandardServer }
$Sudo = if ($SshZiel -like 'root@*') { '' } else { 'sudo ' }

cargo build --release -p jamcenter-relay --target x86_64-unknown-linux-musl
if ($LASTEXITCODE -ne 0) { throw 'Build Relay (Linux) fehlgeschlagen' }
cargo build --release -p jamcenter-relay
if ($LASTEXITCODE -ne 0) { throw 'Build Relay (Windows) fehlgeschlagen' }

if (-not $NurApp) {
    & scp -o BatchMode=yes target/x86_64-unknown-linux-musl/release/jamcenter-relay server/jamcenter-relay.service "${SshZiel}:/tmp/"
    if ($LASTEXITCODE -ne 0) { throw 'Kopieren fehlgeschlagen' }
    # alter Dienst „klangbruecke-relay“ wird abgelöst (belegt sonst die Ports)
    $Befehl = "set -e; ${Sudo}systemctl disable --now klangbruecke-relay >/dev/null 2>&1 || true; " +
              "${Sudo}rm -f /etc/systemd/system/klangbruecke-relay.service /usr/local/bin/klangbruecke-relay; " +
              "${Sudo}install -m 755 /tmp/jamcenter-relay /usr/local/bin/jamcenter-relay; " +
              "${Sudo}install -m 644 /tmp/jamcenter-relay.service /etc/systemd/system/jamcenter-relay.service; " +
              "rm -f /tmp/jamcenter-relay /tmp/jamcenter-relay.service; " +
              "${Sudo}systemctl daemon-reload; ${Sudo}systemctl enable jamcenter-relay >/dev/null 2>&1; ${Sudo}systemctl restart jamcenter-relay; " +
              "for p in 3478 443 47800; do ${Sudo}ufw allow `$p/udp comment Jamcenter >/dev/null; done; " +
              "${Sudo}ufw allow 47800/tcp comment Jamcenter-Aufnahmen >/dev/null; " +
              "command -v lame >/dev/null || ${Sudo}apt-get install -y lame >/dev/null 2>&1 || echo 'lame fehlt'; " +
              "sleep 1; ${Sudo}systemctl is-active jamcenter-relay; ${Sudo}ss -ulnp | grep jamcenter"
    & ssh -o BatchMode=yes $SshZiel $Befehl
    if ($LASTEXITCODE -ne 0) { throw 'Einrichten fehlgeschlagen' }
    Write-Host 'Relay laeuft.'
}

if (-not $NurRelay) {
    cargo build --release -p jamcenter
    if ($LASTEXITCODE -ne 0) { throw 'Build App fehlgeschlagen' }
    # Programmsymbol (Logo) in die exe setzen – die GNU-Toolchain hat keinen Ressourcen-Compiler
    & powershell -NoProfile -File "$PSScriptRoot\icon_setzen.ps1" -Exe "$PSScriptRoot\target\release\jamcenter.exe"
    if ($LASTEXITCODE -ne 0) { throw 'Symbol setzen fehlgeschlagen' }
    if (-not $DownloadPfad) {
        Write-Host 'App gebaut: target\release\jamcenter.exe (kein $DownloadPfad in ziel.local.ps1 - nicht hochgeladen)'
        return
    }
    & scp -o BatchMode=yes target/release/jamcenter.exe "${SshZiel}:/tmp/Jamcenter.exe"
    if ($LASTEXITCODE -ne 0) { throw 'Kopieren fehlgeschlagen' }
    $Besitzer, $Gruppe = $DownloadBesitzer -split ':'
    $Befehl = "set -e; ${Sudo}install -m 644 -o $Besitzer -g $Gruppe /tmp/Jamcenter.exe '$DownloadPfad'; " +
              "rm -f /tmp/Jamcenter.exe; ls -la '$DownloadPfad'"
    & ssh -o BatchMode=yes $SshZiel $Befehl
    if ($LASTEXITCODE -ne 0) { throw 'App hochladen fehlgeschlagen' }
    if ($DownloadUrl) { Write-Host "Download: $DownloadUrl" }
}
