//! Aufnahmen auf dem Server: Liste, Download und Löschen über eine einfache TCP-Verbindung
//! (Port protokoll::AUFNAHME_TCP_PORT). Ablage-Kennung = Hash aus Raumname und Passwort.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::Duration;

#[derive(Clone)]
pub struct Eintrag {
    /// Dateiname (bei laufender Umwandlung ohne Endung)
    pub datei: String,
    pub groesse: u64,
    pub fertig: bool,
}

/// Servername ohne UDP-Port
pub fn host(server: &str) -> String {
    let s = server.trim();
    let s = if s.is_empty() { protokoll::STANDARD_SERVER } else { s };
    match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') && p.chars().all(|c| c.is_ascii_digit()) => h.to_string(),
        _ => s.to_string(),
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn verbinden(host: &str) -> Result<TcpStream, String> {
    let adressen = (host, protokoll::AUFNAHME_TCP_PORT).to_socket_addrs().map_err(|e| format!("Server nicht gefunden: {e}"))?;
    let mut fehler = String::from("Server nicht erreichbar");
    for a in adressen {
        match TcpStream::connect_timeout(&a, Duration::from_secs(5)) {
            Ok(v) => {
                let _ = v.set_read_timeout(Some(Duration::from_secs(30)));
                let _ = v.set_write_timeout(Some(Duration::from_secs(10)));
                return Ok(v);
            }
            Err(e) => fehler = format!("Server nicht erreichbar (TCP {}): {e}", protokoll::AUFNAHME_TCP_PORT),
        }
    }
    Err(fehler)
}

fn anfrage(host: &str, zeile: &str) -> Result<(BufReader<TcpStream>, String), String> {
    let mut v = verbinden(host)?;
    v.write_all(zeile.as_bytes()).map_err(|e| e.to_string())?;
    let mut r = BufReader::new(v);
    let mut kopf = String::new();
    r.read_line(&mut kopf).map_err(|e| e.to_string())?;
    let kopf = kopf.trim().to_string();
    if let Some(f) = kopf.strip_prefix("FEHLER") {
        return Err(f.trim().to_string());
    }
    if !kopf.starts_with("OK") {
        return Err("Unerwartete Antwort vom Server (Server noch nicht aktualisiert?)".into());
    }
    Ok((r, kopf))
}

pub fn liste(host: &str, kennung: &[u8; 32]) -> Result<Vec<Eintrag>, String> {
    let (mut r, _) = anfrage(host, &format!("JAMCENTER 1 LISTE {}\n", hex(kennung)))?;
    let mut text = String::new();
    r.read_to_string(&mut text).map_err(|e| e.to_string())?;
    Ok(text
        .lines()
        .filter_map(|z| {
            let mut t = z.split('\t');
            let datei = t.next()?.to_string();
            let groesse = t.next()?.parse().unwrap_or(0);
            let fertig = t.next()? == "fertig";
            Some(Eintrag { datei, groesse, fertig })
        })
        .collect())
}

pub fn holen(host: &str, kennung: &[u8; 32], datei: &str, ziel: &Path) -> Result<(), String> {
    let (mut r, kopf) = anfrage(host, &format!("JAMCENTER 1 HOLE {} {datei}\n", hex(kennung)))?;
    let laenge: u64 = kopf.split_whitespace().nth(1).and_then(|x| x.parse().ok()).ok_or("Antwort ohne Länge")?;
    let teil = ziel.with_extension("teil");
    let mut f = std::fs::File::create(&teil).map_err(|e| format!("{}: {e}", teil.display()))?;
    let n = std::io::copy(&mut (&mut r).take(laenge), &mut f).map_err(|e| e.to_string())?;
    drop(f);
    if n != laenge {
        let _ = std::fs::remove_file(&teil);
        return Err("Download abgebrochen".into());
    }
    let _ = std::fs::remove_file(ziel);
    std::fs::rename(&teil, ziel).map_err(|e| e.to_string())
}

/// Spur hochladen (Name nur ASCII, .mp3/.wav)
pub fn hochladen(host: &str, kennung: &[u8; 32], datei: &str, daten: &[u8]) -> Result<(), String> {
    let mut v = verbinden(host)?;
    let _ = v.set_write_timeout(Some(Duration::from_secs(120)));
    v.write_all(format!("JAMCENTER 1 HOCH {} {datei} {}\n", hex(kennung), daten.len()).as_bytes()).map_err(|e| e.to_string())?;
    v.write_all(daten).map_err(|e| format!("Übertragung: {e}"))?;
    let mut r = BufReader::new(v);
    let mut kopf = String::new();
    r.read_line(&mut kopf).map_err(|e| e.to_string())?;
    let kopf = kopf.trim();
    if let Some(f) = kopf.strip_prefix("FEHLER") {
        return Err(f.trim().to_string());
    }
    if kopf != "OK" {
        return Err("Unerwartete Antwort vom Server (Server noch nicht aktualisiert?)".into());
    }
    Ok(())
}

/// Aufnahme (Name „2026_12_01_1538…“) oder hochgeladene Spur?
pub fn ist_aufnahme(datei: &str) -> bool {
    let t: Vec<&str> = datei.split('_').collect();
    t.len() >= 4 && t[0].len() == 4 && t[3].len() >= 4 && t[0].chars().all(|c| c.is_ascii_digit())
}

pub fn groesse_text(b: u64) -> String {
    if b >= 1_000_000 {
        format!("{:.1} MB", b as f64 / 1e6).replace('.', ",")
    } else {
        format!("{} kB", (b / 1000).max(1))
    }
}

/// Datei als Datenstrom öffnen (zum Anhören ohne Speichern)
pub fn strom(host: &str, kennung: &[u8; 32], datei: &str) -> Result<std::io::Take<BufReader<TcpStream>>, String> {
    let (r, kopf) = anfrage(host, &format!("JAMCENTER 1 HOLE {} {datei}\n", hex(kennung)))?;
    let laenge: u64 = kopf.split_whitespace().nth(1).and_then(|x| x.parse().ok()).ok_or("Antwort ohne Länge")?;
    Ok(r.take(laenge))
}

/// geschätzte Länge in Sekunden (MP3 mit 192 kbit/s, WAV 48 kHz Stereo)
pub fn dauer_sek(datei: &str, groesse: u64) -> f64 {
    if datei.ends_with(".wav") {
        groesse.saturating_sub(44) as f64 / 192_000.0
    } else {
        groesse as f64 * 8.0 / 192_000.0
    }
}

pub fn zeit_text(s: f64) -> String {
    let s = s.max(0.0) as u64;
    format!("{}:{:02}", s / 60, s % 60)
}

pub fn loeschen(host: &str, kennung: &[u8; 32], datei: &str) -> Result<(), String> {
    anfrage(host, &format!("JAMCENTER 1 LOESCHEN {} {datei}\n", hex(kennung))).map(|_| ())
}

/// „2026_12_01_1538_120bpm_2.mp3“ -> „01.12.2026, 15:38 Uhr · 120 BPM (2)“
pub fn anzeige_name(datei: &str) -> String {
    let stamm = datei.trim_end_matches(".mp3").trim_end_matches(".wav");
    let t: Vec<&str> = stamm.split('_').collect();
    if t.len() >= 4 && t[3].len() == 4 {
        let mut text = format!("{}.{}.{}, {}:{} Uhr", t[2], t[1], t[0], &t[3][..2], &t[3][2..]);
        for z in &t[4..] {
            match z.strip_suffix("bpm") {
                Some(b) => text.push_str(&format!(" · {b} BPM")),
                None => text.push_str(&format!(" ({z})")),
            }
        }
        text
    } else {
        stamm.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namen() {
        assert_eq!(anzeige_name("2026_12_01_1538.mp3"), "01.12.2026, 15:38 Uhr");
        assert_eq!(anzeige_name("2026_12_01_1538_2.wav"), "01.12.2026, 15:38 Uhr (2)");
        assert_eq!(anzeige_name("2026_12_01_1538_120bpm.mp3"), "01.12.2026, 15:38 Uhr · 120 BPM");
        assert_eq!(host("server.de:3478"), "server.de");
        assert_eq!(host(""), protokoll::STANDARD_SERVER);
    }
}
