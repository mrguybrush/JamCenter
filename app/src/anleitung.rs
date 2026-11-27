//! Anleitung zum eigenen Jamcenter-Server und die Dateien, die man dafür braucht.

pub const TEXT_ROH: &str = r#"JAMCENTER – EIGENEN SERVER EINRICHTEN
by .audiosport

Wozu der Server da ist
Der Server ist nur Treffpunkt und Uhr: Er bringt zwei Räume zusammen, führt den Metronom-Takt
und leitet den Ton weiter, falls keine direkte Verbindung möglich ist. Er sieht nur den
Raumnamen als Prüfsumme – der Ton ist Ende-zu-Ende verschlüsselt. Er braucht kaum Leistung
(ein kleiner VPS reicht, ca. 2 MB Arbeitsspeicher je Raum).

Voraussetzungen
- ein Rechner mit fester öffentlicher Adresse (Mietserver/VPS oder ein eigener PC mit Portfreigabe)
- UDP-Ports 3478, 443 und 47800 von außen erreichbar (einer genügt, mehrere sind robuster:
  3478 ist der Teams/STUN-Port und auch in gesperrten Netzen meist offen)
- möglichst nah an beiden Standorten (gleiche Region), denn jeder Umweg kostet Laufzeit

1. Dateien speichern
Unten auf „Dateien speichern …“ klicken und einen Ordner wählen. Gespeichert werden:
  jamcenter-relay            Server für Linux (64 Bit, ohne weitere Abhängigkeiten)
  jamcenter-relay.exe        Server für Windows
  jamcenter-relay.service    Dienstbeschreibung für systemd (Linux)
  installieren.sh            Einrichtungsskript für Linux
  ANLEITUNG.txt              dieser Text

2a. Linux (empfohlen)
  Dateien auf den Server kopieren, z. B. vom Windows-Rechner aus:
    scp jamcenter-relay jamcenter-relay.service installieren.sh benutzer@SERVER:~/
  Auf dem Server anmelden und einrichten:
    ssh benutzer@SERVER
    sudo sh installieren.sh
  Das Skript kopiert das Programm nach /usr/local/bin, richtet den Dienst „jamcenter-relay“ ein
  (startet automatisch, Echtzeit-Priorität, ohne Root-Rechte) und gibt bei ufw die Ports frei.
  Prüfen:   systemctl status jamcenter-relay
  Protokoll: journalctl -u jamcenter-relay -f
  Bietet der Anbieter eine eigene Firewall an (Webportal), dort ebenfalls UDP 3478, 443 und
  47800 eingehend erlauben.

2b. Windows
  jamcenter-relay.exe auf dem Server-PC starten (Fenster offen lassen; für Dauerbetrieb über die
  Aufgabenplanung „Beim Start“ ausführen). Firewall-Regel (Eingabeaufforderung als Administrator):
    netsh advfirewall firewall add rule name=Jamcenter dir=in action=allow protocol=UDP localport=3478,443,47800
  Steht der PC hinter einem Router, dort die UDP-Ports an diesen PC weiterleiten.

Andere Ports
  jamcenter-relay --ports 3478,50000
  (in der .service-Datei die Zeile ExecStart anpassen). In Jamcenter dann „server:50000“ eintragen –
  mit Port wird nur dieser eine benutzt, ohne Port probiert Jamcenter 3478, 443 und 47800.

3. In Jamcenter eintragen
  Im Feld „Server“ den Namen oder die IP-Adresse des Servers eintragen (beide Räume denselben).
  Leer lassen = voreingestellter Jamcenter-Server.

Fehlersuche
- „Server nicht erreichbar“: Ports in Server-Firewall, Anbieter-Firewall und Firewall vor Ort prüfen
  (ausgehend UDP zu diesen Ports muss erlaubt sein).
- „Raum ist schon belegt“: Ein Raum fasst zwei Teilnehmer – anderen Raumnamen wählen.
- Port 443 belegt (z. B. Webserver mit HTTP/3): Der Server nutzt dann nur die übrigen Ports.
"#;

/// Text mit Windows-Zeilenenden (für das Textfeld und ANLEITUNG.txt)
pub fn text() -> String {
    TEXT_ROH.replace('\n', "\r\n")
}

pub const DIENST: &str = include_str!("../../server/jamcenter-relay.service");
pub const SKRIPT: &str = include_str!("../../server/installieren.sh");
pub const RELAY_LINUX: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/relay-linux"));
pub const RELAY_WINDOWS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/relay-windows.exe"));

/// Alle Dateien in einen Ordner schreiben; liefert die Liste der geschriebenen Dateien
pub fn speichern(ordner: &std::path::Path) -> Result<Vec<String>, String> {
    let mut fertig = Vec::new();
    let mut schreiben = |name: &str, daten: &[u8]| -> Result<(), String> {
        if daten.is_empty() {
            return Ok(());
        }
        std::fs::write(ordner.join(name), daten).map_err(|e| format!("{name}: {e}"))?;
        fertig.push(name.to_string());
        Ok(())
    };
    schreiben("jamcenter-relay", RELAY_LINUX)?;
    schreiben("jamcenter-relay.exe", RELAY_WINDOWS)?;
    // Linux-Dateien mit LF-Zeilenenden
    schreiben("jamcenter-relay.service", DIENST.replace("\r\n", "\n").as_bytes())?;
    schreiben("installieren.sh", SKRIPT.replace("\r\n", "\n").as_bytes())?;
    schreiben("ANLEITUNG.txt", text().as_bytes())?;
    Ok(fertig)
}

#[cfg(test)]
mod tests {
    #[test]
    fn dateien_speichern() {
        let ordner = std::env::temp_dir().join(format!("jamcenter-test-{}", std::process::id()));
        std::fs::create_dir_all(&ordner).unwrap();
        let liste = super::speichern(&ordner).unwrap();
        for n in ["jamcenter-relay.service", "installieren.sh", "ANLEITUNG.txt"] {
            assert!(liste.iter().any(|x| x == n), "{n} fehlt");
        }
        let sh = std::fs::read_to_string(ordner.join("installieren.sh")).unwrap();
        assert!(!sh.contains('\r'), "Skript mit CR");
        assert!(super::text().contains("  jamcenter-relay.exe"), "Einrückung fehlt");
        let _ = std::fs::remove_dir_all(&ordner);
    }
}
