#!/bin/sh
# Jamcenter-Server einrichten (Linux mit systemd, z. B. Debian/Ubuntu)
# Aufruf im Ordner mit den gespeicherten Dateien:  sudo sh installieren.sh
set -e
cd "$(dirname "$0")"
if [ "$(id -u)" != "0" ]; then
    echo "Bitte mit sudo bzw. als root starten: sudo sh installieren.sh"
    exit 1
fi
# alter Dienst (frühere Version „Klangbrücke“) belegt sonst die Ports
systemctl disable --now klangbruecke-relay >/dev/null 2>&1 || true
# lame wandelt Aufnahmen in MP3 um (ohne lame bleiben sie als WAV)
if ! command -v lame >/dev/null 2>&1 && command -v apt-get >/dev/null 2>&1; then
    apt-get install -y lame >/dev/null 2>&1 || echo "Hinweis: lame nicht installiert – Aufnahmen bleiben WAV."
fi
install -m 755 jamcenter-relay /usr/local/bin/jamcenter-relay
install -m 644 jamcenter-relay.service /etc/systemd/system/jamcenter-relay.service
systemctl daemon-reload
systemctl enable jamcenter-relay >/dev/null 2>&1
systemctl restart jamcenter-relay
if command -v ufw >/dev/null 2>&1; then
    for p in 3478 443 47800; do ufw allow "$p/udp" comment Jamcenter >/dev/null; done
    ufw allow 47800/tcp comment Jamcenter-Aufnahmen >/dev/null
    echo "Firewall (ufw): UDP 3478, 443 und 47800 sowie TCP 47800 (Aufnahmen) freigegeben."
else
    echo "Hinweis: Keine ufw gefunden – UDP 3478, 443, 47800 und TCP 47800 ggf. in der Firewall des Anbieters freigeben."
fi
sleep 1
systemctl --no-pager --lines=5 status jamcenter-relay || true
echo
echo "Fertig. In Jamcenter unter „Server“ die Adresse dieses Rechners eintragen (Name oder IP)."
