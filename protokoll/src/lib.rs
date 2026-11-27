//! Paketformat der Jamcenter – gemeinsam für Relay-Server und App.
//!
//! Jedes UDP-Paket beginnt mit einem 12-Byte-Kopf:
//!   0..2  Kennung "KB"
//!   2     Protokollversion
//!   3     Typ
//!   4..12 Sitzungsnummer des Absenders (u64, little endian)
//!
//! Typen < 0x10 sind Verwaltung zwischen App und Server (Klartext, keine Inhalte).
//! Typen >= 0x10 gehen von App zu App (über den Server oder direkt) und sind
//! Ende-zu-Ende verschlüsselt: nach dem Kopf folgt ein u32-Zähler (Teil der Nonce),
//! dann der verschlüsselte Inhalt mit 16-Byte-Prüfsumme. Der Server leitet diese
//! Pakete unverändert weiter und kann sie nicht lesen.

pub mod klang;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

pub const KENNUNG: [u8; 2] = *b"KB";
pub const VERSION: u8 = 1;
pub const KOPF: usize = 12;
/// Kopf + Zähler = zusätzliche Daten (AAD) der Verschlüsselung
pub const KOPF_VERSCHLUESSELT: usize = 16;

pub const T_HALLO: u8 = 0x01;
pub const T_TSCHUESS: u8 = 0x02;
pub const T_STAND: u8 = 0x03;
pub const T_PRUEFUNG: u8 = 0x04;
/// App -> Server: Metronom des Raums umstellen
pub const T_METRONOM: u8 = 0x05;
/// App -> Server: Aufnahme des Raums starten/stoppen
pub const T_AUFNAHME: u8 = 0x06;
/// App -> Server: eigener Ton für die Aufnahme (nur während einer Aufnahme, Klartext – der
/// Server muss ihn lesen, um die Aufnahme zu mischen)
pub const T_AUFNAHME_TON: u8 = 0x07;
/// App -> Server: Spur (Audiodatei auf dem Server) für den ganzen Raum starten/stoppen
pub const T_SPUR: u8 = 0x08;
pub const T_AUDIO: u8 = 0x10;
pub const T_PING: u8 = 0x11;

pub const STATUS_OK: u8 = 0;
pub const STATUS_VOLL: u8 = 1;
pub const STATUS_VERALTET: u8 = 2;

/// Höchstzahl Teilnehmer je Raum (vorerst nur 1:1).
pub const MAX_TEILNEHMER: usize = 2;
pub const MAX_PAKET: usize = 1400;
pub const MAX_NAME: usize = 40;
/// Ports, auf denen der Server lauscht. 3478 ist der STUN/Teams-Port (auch in gesperrten
/// Netzen meist offen), 443 der QUIC-Port, 47800 ein freier Ausweichport.
pub const STANDARD_PORTS: [u16; 3] = [3478, 443, 47800];
/// Vorgabe für das Feld „Server“. Im Code bewusst leer; eigene Builds können sie beim Bauen
/// setzen (Umgebungsvariable JAMCENTER_SERVER, z. B. über ziel.local.ps1 / hochladen.ps1).
pub const STANDARD_SERVER: &str = match option_env!("JAMCENTER_SERVER") {
    Some(s) => s,
    None => "",
};
/// TCP-Port für Liste und Download der Aufnahmen
pub const AUFNAHME_TCP_PORT: u16 = 47800;
/// höchstens so viele Proben (je Kanal) in einem Aufnahme-Paket
pub const AUFNAHME_MAX_FRAMES: usize = 300;

pub fn kopf_schreiben(buf: &mut Vec<u8>, typ: u8, sitzung: u64) {
    buf.clear();
    buf.extend_from_slice(&KENNUNG);
    buf.push(VERSION);
    buf.push(typ);
    buf.extend_from_slice(&sitzung.to_le_bytes());
}

/// Liefert (Typ, Sitzung), wenn Kennung und Version stimmen.
pub fn kopf_lesen(buf: &[u8]) -> Option<(u8, u64)> {
    if buf.len() < KOPF || buf[0..2] != KENNUNG || buf[2] != VERSION {
        return None;
    }
    Some((buf[3], u64::from_le_bytes(buf[4..12].try_into().ok()?)))
}

pub struct Leser<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Leser<'a> {
    pub fn neu(buf: &'a [u8], pos: usize) -> Self {
        Leser { buf, pos }
    }
    pub fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.buf.get(self.pos..self.pos + n)?;
        self.pos += n;
        Some(s)
    }
    pub fn u8(&mut self) -> Option<u8> {
        Some(self.bytes(1)?[0])
    }
    pub fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.bytes(2)?.try_into().ok()?))
    }
    pub fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.bytes(4)?.try_into().ok()?))
    }
    pub fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.bytes(8)?.try_into().ok()?))
    }
    pub fn f32(&mut self) -> Option<f32> {
        Some(f32::from_bits(self.u32()?))
    }
    pub fn text(&mut self) -> Option<String> {
        let n = self.u8()? as usize;
        Some(String::from_utf8_lossy(self.bytes(n)?).into_owned())
    }
    pub fn adresse(&mut self) -> Option<SocketAddr> {
        match self.u8()? {
            4 => {
                let b: [u8; 4] = self.bytes(4)?.try_into().ok()?;
                Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(b)), self.u16()?))
            }
            6 => {
                let b: [u8; 16] = self.bytes(16)?.try_into().ok()?;
                Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(b)), self.u16()?))
            }
            _ => None,
        }
    }
    pub fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos.min(self.buf.len())..]
    }
}

pub fn text_schreiben(buf: &mut Vec<u8>, t: &str) {
    let mut n = t.len().min(MAX_NAME);
    while !t.is_char_boundary(n) {
        n -= 1;
    }
    buf.push(n as u8);
    buf.extend_from_slice(&t.as_bytes()[..n]);
}

pub fn adresse_schreiben(buf: &mut Vec<u8>, a: &SocketAddr) {
    match a.ip() {
        IpAddr::V4(ip) => {
            buf.push(4);
            buf.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                buf.push(4);
                buf.extend_from_slice(&v4.octets());
            } else {
                buf.push(6);
                buf.extend_from_slice(&ip.octets());
            }
        }
    }
    buf.extend_from_slice(&a.port().to_le_bytes());
}

/// Anmeldung/Lebenszeichen der App beim Server (einmal je Sekunde).
#[derive(Debug, Clone)]
pub struct Hallo {
    /// Raumkennung = Hash aus Raumname und Passwort (der Server kennt beides nicht)
    pub raum: [u8; 32],
    /// Adressnachweis vom Server (0 beim ersten Mal)
    pub keks: u64,
    pub name: String,
    pub lokal: Vec<SocketAddr>,
    /// Uhr der App in µs (für den Uhrabgleich, kommt im Stand zurück)
    pub zeit: u64,
}

impl Hallo {
    pub fn schreiben(&self, buf: &mut Vec<u8>, sitzung: u64) {
        kopf_schreiben(buf, T_HALLO, sitzung);
        buf.extend_from_slice(&self.raum);
        buf.extend_from_slice(&self.keks.to_le_bytes());
        text_schreiben(buf, &self.name);
        buf.push(self.lokal.len().min(4) as u8);
        for a in self.lokal.iter().take(4) {
            adresse_schreiben(buf, a);
        }
        buf.extend_from_slice(&self.zeit.to_le_bytes());
    }
    pub fn lesen(buf: &[u8]) -> Option<Hallo> {
        let mut l = Leser::neu(buf, KOPF);
        let raum: [u8; 32] = l.bytes(32)?.try_into().ok()?;
        let keks = l.u64()?;
        let name = l.text()?;
        let n = l.u8()?.min(4);
        let mut lokal = Vec::new();
        for _ in 0..n {
            lokal.push(l.adresse()?);
        }
        let zeit = l.u64().unwrap_or(0);
        Some(Hallo { raum, keks, name, lokal, zeit })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Teilnehmer {
    pub sitzung: u64,
    pub name: String,
    pub oeffentlich: SocketAddr,
    pub lokal: Vec<SocketAddr>,
}

/// Metronom eines Raums. Der Server ist die Uhr: Schlag k erklingt zur Serverzeit
/// `start_us + k * 60e6 / bpm`; Schlag 0, takt, 2·takt … sind betont.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Metronom {
    pub an: bool,
    pub bpm: f32,
    pub takt: u8,
    pub start_us: u64,
    /// zählt jede Änderung hoch
    pub stand: u32,
}

impl Metronom {
    pub fn schreiben(&self, buf: &mut Vec<u8>) {
        buf.push(self.an as u8);
        buf.extend_from_slice(&self.bpm.to_le_bytes());
        buf.push(self.takt);
        buf.extend_from_slice(&self.start_us.to_le_bytes());
        buf.extend_from_slice(&self.stand.to_le_bytes());
    }
    pub fn lesen(l: &mut Leser) -> Option<Metronom> {
        Some(Metronom { an: l.u8()? != 0, bpm: l.f32()?, takt: l.u8()?, start_us: l.u64()?, stand: l.u32()? })
    }
}

/// Wunsch einer App, das Metronom zu ändern
pub fn metronom_wunsch_schreiben(buf: &mut Vec<u8>, sitzung: u64, an: bool, bpm: f32, takt: u8) {
    kopf_schreiben(buf, T_METRONOM, sitzung);
    buf.push(an as u8);
    buf.extend_from_slice(&bpm.to_le_bytes());
    buf.push(takt);
}

pub fn metronom_wunsch_lesen(buf: &[u8]) -> Option<(bool, f32, u8)> {
    let mut l = Leser::neu(buf, KOPF);
    Some((l.u8()? != 0, l.f32()?, l.u8()?))
}

/// Aufnahme eines Raums (läuft auf dem Server)
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Aufnahme {
    pub an: bool,
    /// Serverzeit des Beginns
    pub start_us: u64,
    /// Name = Datum und Uhrzeit des Starts, z. B. 2026_12_01_1538
    pub name: String,
}

impl Aufnahme {
    pub fn schreiben(&self, buf: &mut Vec<u8>) {
        buf.push(self.an as u8);
        buf.extend_from_slice(&self.start_us.to_le_bytes());
        text_schreiben(buf, &self.name);
    }
    pub fn lesen(l: &mut Leser) -> Option<Aufnahme> {
        Some(Aufnahme { an: l.u8()? != 0, start_us: l.u64()?, name: l.text()? })
    }
}

/// Wunsch einer App: Aufnahme starten (mit Ablage-Kennung, Namen und – wenn das Metronom mit
/// aufgenommen werden soll – dessen Klangform) oder stoppen
pub fn aufnahme_wunsch_schreiben(buf: &mut Vec<u8>, sitzung: u64, an: bool, kennung: &[u8; 32], name: &str, metronom_klang: Option<u8>) {
    kopf_schreiben(buf, T_AUFNAHME, sitzung);
    buf.push(an as u8);
    buf.extend_from_slice(kennung);
    text_schreiben(buf, name);
    buf.push(metronom_klang.map_or(0xFF, |k| k));
}

pub fn aufnahme_wunsch_lesen(buf: &[u8]) -> Option<(bool, [u8; 32], String, Option<u8>)> {
    let mut l = Leser::neu(buf, KOPF);
    let (an, kennung, name) = (l.u8()? != 0, l.bytes(32)?.try_into().ok()?, l.text()?);
    let klang = l.u8().filter(|&k| k != 0xFF);
    Some((an, kennung, name, klang))
}

/// Ton für die Aufnahme: Folgenummer, Serverzeit der ersten Probe, Rate, Kanäle, PCM16
pub fn aufnahme_ton_schreiben(buf: &mut Vec<u8>, sitzung: u64, seq: u32, zeit_us: u64, rate: u32, kanaele: u8, proben: &[i16]) {
    kopf_schreiben(buf, T_AUFNAHME_TON, sitzung);
    buf.extend_from_slice(&seq.to_le_bytes());
    buf.extend_from_slice(&zeit_us.to_le_bytes());
    buf.extend_from_slice(&rate.to_le_bytes());
    buf.push(kanaele);
    for &p in proben {
        buf.extend_from_slice(&p.to_le_bytes());
    }
}

pub struct AufnahmeTon<'a> {
    pub seq: u32,
    pub zeit_us: u64,
    pub rate: u32,
    pub kanaele: u8,
    pub daten: &'a [u8],
}

pub fn aufnahme_ton_lesen(buf: &[u8]) -> Option<AufnahmeTon<'_>> {
    let mut l = Leser::neu(buf, KOPF);
    let (seq, zeit_us, rate, kanaele) = (l.u32()?, l.u64()?, l.u32()?, l.u8()?);
    if !(1..=2).contains(&kanaele) || !(8000..=192_000).contains(&rate) {
        return None;
    }
    let daten = l.rest();
    Some(AufnahmeTon { seq, zeit_us, rate, kanaele, daten: &daten[..daten.len() / (2 * kanaele as usize) * 2 * kanaele as usize] })
}

/// Spur eines Raums: eine Audiodatei aus der Ablage des Raums, die alle Apps selbst abspielen –
/// Position 0 erklingt zur Serverzeit `start_us` (wie das Metronom überall gleichzeitig)
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Spur {
    pub an: bool,
    /// Ablage-Kennung (Hash aus Raum und Passwort) und Dateiname
    pub kennung: [u8; 32],
    pub datei: String,
    pub start_us: u64,
    /// zählt jede Änderung hoch
    pub stand: u32,
}

impl Spur {
    pub fn schreiben(&self, buf: &mut Vec<u8>) {
        buf.push(self.an as u8);
        buf.extend_from_slice(&self.kennung);
        text_schreiben(buf, &self.datei);
        buf.extend_from_slice(&self.start_us.to_le_bytes());
        buf.extend_from_slice(&self.stand.to_le_bytes());
    }
    pub fn lesen(l: &mut Leser) -> Option<Spur> {
        Some(Spur { an: l.u8()? != 0, kennung: l.bytes(32)?.try_into().ok()?, datei: l.text()?, start_us: l.u64()?, stand: l.u32()? })
    }
}

/// Wunsch einer App: Spur abspielen (ab Sekunde `ab`) oder stoppen
pub fn spur_wunsch_schreiben(buf: &mut Vec<u8>, sitzung: u64, an: bool, kennung: &[u8; 32], datei: &str, ab_ms: u32) {
    kopf_schreiben(buf, T_SPUR, sitzung);
    buf.push(an as u8);
    buf.extend_from_slice(kennung);
    text_schreiben(buf, datei);
    buf.extend_from_slice(&ab_ms.to_le_bytes());
}

pub fn spur_wunsch_lesen(buf: &[u8]) -> Option<(bool, [u8; 32], String, u32)> {
    let mut l = Leser::neu(buf, KOPF);
    Some((l.u8()? != 0, l.bytes(32)?.try_into().ok()?, l.text()?, l.u32().unwrap_or(0)))
}

/// Antwort des Servers auf jedes Hallo.
#[derive(Debug, Clone)]
pub struct Stand {
    pub status: u8,
    /// Absenderadresse, wie der Server sie sieht (öffentliche Adresse hinter dem NAT)
    pub du: SocketAddr,
    pub andere: Vec<Teilnehmer>,
    /// Zeit aus dem Hallo der App (0 = Stand ohne Anlass, z. B. nach Metronom-Änderung)
    pub zeit_app: u64,
    /// Serveruhr in µs beim Absenden
    pub zeit_server: u64,
    pub metronom: Metronom,
    pub aufnahme: Aufnahme,
    pub spur: Spur,
}

impl Stand {
    pub fn schreiben(&self, buf: &mut Vec<u8>) {
        kopf_schreiben(buf, T_STAND, 0);
        buf.push(self.status);
        adresse_schreiben(buf, &self.du);
        buf.push(self.andere.len() as u8);
        for t in &self.andere {
            buf.extend_from_slice(&t.sitzung.to_le_bytes());
            text_schreiben(buf, &t.name);
            adresse_schreiben(buf, &t.oeffentlich);
            buf.push(t.lokal.len().min(4) as u8);
            for a in t.lokal.iter().take(4) {
                adresse_schreiben(buf, a);
            }
        }
        buf.extend_from_slice(&self.zeit_app.to_le_bytes());
        buf.extend_from_slice(&self.zeit_server.to_le_bytes());
        self.metronom.schreiben(buf);
        self.aufnahme.schreiben(buf);
        self.spur.schreiben(buf);
    }
    pub fn lesen(buf: &[u8]) -> Option<Stand> {
        let mut l = Leser::neu(buf, KOPF);
        let status = l.u8()?;
        let du = l.adresse()?;
        let n = l.u8()?;
        let mut andere = Vec::new();
        for _ in 0..n {
            let sitzung = l.u64()?;
            let name = l.text()?;
            let oeffentlich = l.adresse()?;
            let m = l.u8()?.min(4);
            let mut lokal = Vec::new();
            for _ in 0..m {
                lokal.push(l.adresse()?);
            }
            andere.push(Teilnehmer { sitzung, name, oeffentlich, lokal });
        }
        // ältere Server schicken die folgenden Felder nicht
        let zeit_app = l.u64().unwrap_or(0);
        let zeit_server = l.u64().unwrap_or(0);
        let metronom = Metronom::lesen(&mut l).unwrap_or_default();
        let aufnahme = Aufnahme::lesen(&mut l).unwrap_or_default();
        let spur = Spur::lesen(&mut l).unwrap_or_default();
        Some(Stand { status, du, andere, zeit_app, zeit_server, metronom, aufnahme, spur })
    }
}

pub fn pruefung_schreiben(buf: &mut Vec<u8>, keks: u64) {
    kopf_schreiben(buf, T_PRUEFUNG, 0);
    buf.extend_from_slice(&keks.to_le_bytes());
}

pub fn pruefung_lesen(buf: &[u8]) -> Option<u64> {
    Leser::neu(buf, KOPF).u64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hallo_und_stand() {
        let h = Hallo {
            raum: [7; 32],
            keks: 99,
            name: "Musikraum Ä".into(),
            lokal: vec!["192.168.1.5:5000".parse().unwrap()],
            zeit: 123,
        };
        let mut b = Vec::new();
        h.schreiben(&mut b, 42);
        assert_eq!(kopf_lesen(&b), Some((T_HALLO, 42)));
        let h2 = Hallo::lesen(&b).unwrap();
        assert_eq!(h2.name, h.name);
        assert_eq!(h2.lokal, h.lokal);
        assert_eq!(h2.keks, 99);

        let s = Stand {
            status: STATUS_OK,
            du: "1.2.3.4:5".parse().unwrap(),
            andere: vec![Teilnehmer {
                sitzung: 5,
                name: "B".into(),
                oeffentlich: "[2001:db8::1]:7".parse().unwrap(),
                lokal: vec![],
            }],
            zeit_app: 123,
            zeit_server: 456,
            metronom: Metronom { an: true, bpm: 96.5, takt: 3, start_us: 789, stand: 2 },
            aufnahme: Aufnahme { an: true, start_us: 5, name: "2026_12_01_1538".into() },
            spur: Spur { an: true, kennung: [3; 32], datei: "Schlagzeug_96bpm.mp3".into(), start_us: 77, stand: 4 },
        };
        s.schreiben(&mut b);
        let s2 = Stand::lesen(&b).unwrap();
        assert_eq!(s2.andere, s.andere);
        assert_eq!(s2.du, s.du);
        assert_eq!(s2.metronom, s.metronom);
        assert_eq!(s2.aufnahme, s.aufnahme);
        assert_eq!(s2.spur, s.spur);
        assert_eq!((s2.zeit_app, s2.zeit_server), (123, 456));
        assert_eq!(Hallo::lesen(&{ let mut b = Vec::new(); h.schreiben(&mut b, 1); b }).unwrap().zeit, 123);
    }
}
