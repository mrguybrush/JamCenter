//! Netz: Anmeldung am Server, direkte Verbindung (UDP-Lochstanzen), Wegwahl,
//! Ende-zu-Ende-Verschlüsselung und Versand der Audiopakete.
//!
//! Wegwahl: Beide Seiten pingen sich über den Server UND direkt (öffentliche und
//! lokale Adresse des Gegenübers). Sobald der direkte Weg antwortet und nicht
//! langsamer ist, gehen die Audiopakete direkt – sonst über den Server.

use crate::analyse::{Ergebnis, Spektrum, Waechter};
use crate::gemeinsam::{db_zu_faktor, uhr_us, Geteilt, MAX_KANAELE};
use crate::mischer::Mischer;
use crate::puffer::Empfang;
use chacha20poly1305::aead::AeadInPlace;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce, Tag};
use protokoll::*;
use sha2::{Digest, Sha256};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Höchstens so viele Proben je Paket (5,3 ms bei 48 kHz)
const MAX_FRAMES: usize = 256;
/// Stereo: 160 Frames × 2 Kanäle × 2 Byte × 2 (mit Kopie) = 1280 Byte, passt unter die MTU
const MAX_FRAMES_STEREO: usize = 160;
pub const CODEC_MONO: u8 = 0;
pub const CODEC_STEREO: u8 = 1;

fn normalisieren(raum: &str) -> String {
    raum.trim().to_lowercase()
}

/// Raumkennung für den Server: nur aus dem Namen (so kann die App melden,
/// wenn das Passwort der Gegenstelle nicht passt).
pub fn raum_kennung(raum: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"Klangbruecke-Raum|");
    h.update(normalisieren(raum).as_bytes());
    h.finalize().into()
}

/// Schlüssel aus Passwort und Raumname (PBKDF2, 100 000 Runden)
/// Ablage-Kennung der Aufnahmen: hängt an Raumname UND Passwort (der Server kennt beides nicht)
pub fn aufnahme_kennung(raum: &str, passwort: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"Jamcenter-Aufnahme|");
    h.update(schluessel(raum, passwort));
    h.finalize().into()
}

pub fn schluessel(raum: &str, passwort: &str) -> [u8; 32] {
    let salz = format!("Klangbruecke-Schluessel|{}", normalisieren(raum));
    let mut k = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(passwort.as_bytes(), salz.as_bytes(), 100_000, &mut k);
    k
}

#[derive(Clone)]
pub struct Krypto {
    c: ChaCha20Poly1305,
    pub sitzung: u64,
    zaehler: Arc<AtomicU32>,
}

fn nonce(sitzung: u64, z: u32) -> Nonce {
    let mut n = [0u8; 12];
    n[..8].copy_from_slice(&sitzung.to_le_bytes());
    n[8..].copy_from_slice(&z.to_le_bytes());
    *Nonce::from_slice(&n)
}

impl Krypto {
    pub fn neu(schluessel: &[u8; 32], sitzung: u64) -> Self {
        Krypto { c: ChaCha20Poly1305::new(schluessel.into()), sitzung, zaehler: Arc::new(AtomicU32::new(1)) }
    }

    pub fn versiegeln(&self, typ: u8, inhalt: &[u8], out: &mut Vec<u8>) {
        kopf_schreiben(out, typ, self.sitzung);
        let z = self.zaehler.fetch_add(1, Relaxed);
        out.extend_from_slice(&z.to_le_bytes());
        let start = out.len();
        out.extend_from_slice(inhalt);
        let (aad, rest) = out.split_at_mut(start);
        let tag = self.c.encrypt_in_place_detached(&nonce(self.sitzung, z), aad, rest).expect("Verschlüsselung");
        out.extend_from_slice(&tag);
    }

    /// Entschlüsselt im Puffer; liefert (Sitzung, Zähler, Inhalt)
    pub fn oeffnen<'a>(&self, paket: &'a mut [u8]) -> Option<(u64, u32, &'a [u8])> {
        if paket.len() < KOPF_VERSCHLUESSELT + 16 {
            return None;
        }
        let (_, sitzung) = kopf_lesen(paket)?;
        let z = u32::from_le_bytes(paket[12..16].try_into().ok()?);
        let n = paket.len();
        let (vorne, tag) = paket.split_at_mut(n - 16);
        let (aad, inhalt) = vorne.split_at_mut(KOPF_VERSCHLUESSELT);
        self.c.decrypt_in_place_detached(&nonce(sitzung, z), aad, inhalt, Tag::from_slice(tag)).ok()?;
        Some((sitzung, z, inhalt))
    }
}

pub fn ziel_kodieren(a: Option<SocketAddr>) -> u64 {
    match a {
        Some(SocketAddr::V4(v)) => (1u64 << 48) | ((u32::from(*v.ip()) as u64) << 16) | v.port() as u64,
        _ => 0,
    }
}

pub fn ziel_dekodieren(x: u64) -> Option<SocketAddr> {
    if x == 0 {
        return None;
    }
    Some(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::from((x >> 16) as u32), x as u16)))
}

/// Läuft im Aufnahme-Thread (bzw. im ASIO-Callback): Mischpult mit Effekten, Versand.
pub struct Sender {
    sock: Arc<UdpSocket>,
    krypto: Krypto,
    geteilt: Arc<Geteilt>,
    rate: u32,
    seq: u32,
    /// Vorgängerpaket (i16, verschachtelt) für den Verlustschutz und seine Kanalzahl
    vorher: Vec<i16>,
    vorher_kanaele: usize,
    aktuell: Vec<i16>,
    klar: Vec<u8>,
    paket: Vec<u8>,
    mono: Vec<f32>,
    kanal: Vec<f32>,
    spektrum: Spektrum,
    pub mischer: Mischer,
    /// Aufnahme: Folgenummer, Serverzeit der nächsten Probe, läuft
    aufn_seq: u32,
    aufn_t: f64,
    aufn_aktiv: bool,
    aufn_proben: Vec<i16>,
    aufn_paket: Vec<u8>,
    /// weitere Tonquellen (eigene Soundkarten/Loopback), als Kanäle hinter dem Haupteingang
    zusatz: Vec<crate::zusatz::Leser>,
    zusatz_puffer: Vec<Vec<f32>>,
}

impl Sender {
    pub fn neu(sock: Arc<UdpSocket>, krypto: Krypto, geteilt: Arc<Geteilt>, rate: u32) -> Self {
        Sender {
            sock,
            krypto,
            geteilt,
            rate,
            seq: 0,
            vorher: Vec::new(),
            vorher_kanaele: 1,
            aktuell: Vec::new(),
            klar: Vec::with_capacity(2048),
            paket: Vec::with_capacity(2048),
            mono: Vec::with_capacity(8192),
            kanal: Vec::with_capacity(8192),
            spektrum: Spektrum::neu(),
            mischer: Mischer::neu(rate),
            aufn_seq: 0,
            aufn_t: 0.0,
            aufn_aktiv: false,
            aufn_proben: Vec::with_capacity(AUFNAHME_MAX_FRAMES * 2),
            aufn_paket: Vec::with_capacity(1500),
            zusatz: Vec::new(),
            zusatz_puffer: (0..32).map(|_| Vec::with_capacity(4096)).collect(),
        }
    }

    pub fn zusatz_setzen(&mut self, quellen: Vec<crate::zusatz::Leser>) {
        self.zusatz = quellen;
    }

    pub fn rate_setzen(&mut self, rate: u32) {
        self.rate = rate;
        self.mischer.rate_setzen(rate);
    }

    /// Mehrkanalige Aufnahme (verschachtelt, z. B. WASAPI) durchs Mischpult und senden
    pub fn verarbeiten_kanaele(&mut self, daten: &[f32], kanaele: usize) {
        let k = kanaele.max(1);
        let n = daten.len() / k;
        self.mischer.beginnen(n);
        for c in 0..k.min(MAX_KANAELE) {
            self.kanal.clear();
            self.kanal.extend(daten.chunks_exact(k).map(|f| f[c]));
            let kanal = std::mem::take(&mut self.kanal);
            self.mischer.kanal(c, &kanal, &self.geteilt);
            self.kanal = kanal;
        }
        self.mischer.beenden(&self.geteilt);
        self.senden_aus_mischer();
    }

    /// Ein einzelner Mono-Eingang (Testmodus)
    pub fn verarbeiten(&mut self, proben: &[f32]) {
        self.verarbeiten_kanaele(proben, 1);
    }

    /// Summe aus dem Mischpult (links/rechts) verschicken; vorher `mischer.beginnen/kanal/beenden`
    pub fn senden_aus_mischer(&mut self) {
        let g = self.geteilt.clone();
        // Zusatzquellen im Takt dieses Blocks abholen und als weitere Kanäle mischen
        let n = self.mischer.links.len();
        if !self.zusatz.is_empty() && n > 0 {
            let mut basis = g.anzeige.haupt_kanaele.load(Relaxed) as usize;
            for l in self.zusatz.iter_mut() {
                l.lesen(n, self.rate, &mut self.zusatz_puffer);
                for (j, buf) in self.zusatz_puffer.iter().take(l.kanaele).enumerate() {
                    self.mischer.kanal(basis + j, buf, &g);
                }
                basis += l.kanaele;
            }
        }
        let faktor = if g.regler.stumm.load(Relaxed) { 0.0 } else { db_zu_faktor(g.regler.eingang_db.get()) };
        let stereo = g.regler.stereo.load(Relaxed);
        let n = self.mischer.links.len();
        let mut spitze = 0.0f32;
        self.mono.clear();
        for i in 0..n {
            let l = self.mischer.links[i] * faktor;
            let r = self.mischer.rechts[i] * faktor;
            self.mischer.links[i] = l;
            self.mischer.rechts[i] = r;
            spitze = spitze.max(l.abs()).max(r.abs());
            self.mono.push(0.5 * (l + r));
        }
        g.anzeige.pegel_ein.max(spitze);
        if g.regler.schutz.load(Relaxed) {
            let a = &g.analyse;
            self.spektrum.zufuehren(&self.mono, self.rate, |e| {
                a.frequenz.set(e.frequenz);
                a.anteil.set(e.anteil);
                a.pegel_db.set(e.pegel_db);
            });
        }
        self.aufnahme_senden(n, stereo);
        let Some(ziel) = ziel_dekodieren(g.ziel.load(Relaxed)) else {
            self.vorher.clear();
            return;
        };
        if n == 0 {
            return;
        }
        let redundanz = g.regler.redundanz.load(Relaxed);
        let kanaele = if stereo { 2 } else { 1 };
        // Stereo mit Kopie muss unter die MTU passen -> kleinere Pakete
        let max_frames = if stereo { MAX_FRAMES_STEREO } else { MAX_FRAMES };
        let teile = n.div_ceil(max_frames);
        let groesse = n.div_ceil(teile);
        let q = |x: f32| (x.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        let mut start = 0;
        while start < n {
            let ende = (start + groesse).min(n);
            self.aktuell.clear();
            for i in start..ende {
                if stereo {
                    self.aktuell.push(q(self.mischer.links[i]));
                    self.aktuell.push(q(self.mischer.rechts[i]));
                } else {
                    self.aktuell.push(q(self.mono[i]));
                }
            }
            let frames = ende - start;
            let mit_kopie = redundanz && !self.vorher.is_empty() && self.vorher_kanaele == kanaele;
            self.klar.clear();
            self.klar.push(if stereo { CODEC_STEREO } else { CODEC_MONO });
            self.klar.push(mit_kopie as u8);
            self.klar.extend_from_slice(&(frames as u16).to_le_bytes());
            self.klar.extend_from_slice(&self.rate.to_le_bytes());
            self.klar.extend_from_slice(&self.seq.to_le_bytes());
            for &s in &self.aktuell {
                self.klar.extend_from_slice(&s.to_le_bytes());
            }
            if mit_kopie {
                self.klar.extend_from_slice(&((self.vorher.len() / kanaele) as u16).to_le_bytes());
                for &s in &self.vorher {
                    self.klar.extend_from_slice(&s.to_le_bytes());
                }
            }
            self.krypto.versiegeln(T_AUDIO, &self.klar, &mut self.paket);
            let _ = self.sock.send_to(&self.paket, ziel);
            std::mem::swap(&mut self.vorher, &mut self.aktuell);
            self.vorher_kanaele = kanaele;
            self.seq = self.seq.wrapping_add(1);
            start = ende;
        }
    }
}

impl Sender {
    /// Während einer Aufnahme den eigenen Ton zusätzlich an den Server schicken – mit der
    /// Serverzeit, zu der er gespielt wurde. Die Zeit läuft über die Probenzahl mit (gleichmäßig)
    /// und wird nur bei größerer Abweichung (Taktdrift, Aussetzer) neu an die Uhr angelegt.
    fn aufnahme_senden(&mut self, n: usize, stereo: bool) {
        let g = self.geteilt.clone();
        if !g.aufnahme_an.load(Relaxed) || n == 0 || !g.uhr_ok.load(Relaxed) {
            self.aufn_aktiv = false;
            return;
        }
        let Some(server) = ziel_dekodieren(g.server_ziel.load(Relaxed)) else { return };
        let rate = self.rate as f64;
        let gespielt = uhr_us() as f64 + g.uhr_versatz.load(Relaxed) as f64 - g.anzeige.ein_latenz_ms.get() as f64 * 1000.0 - n as f64 * 1e6 / rate;
        if !self.aufn_aktiv || (gespielt - self.aufn_t).abs() > 20_000.0 {
            self.aufn_t = gespielt;
            self.aufn_aktiv = true;
        }
        let q = |x: f32| (x.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        let kanaele = if stereo { 2u8 } else { 1 };
        let mut start = 0;
        while start < n {
            let ende = (start + AUFNAHME_MAX_FRAMES).min(n);
            self.aufn_proben.clear();
            for i in start..ende {
                if stereo {
                    self.aufn_proben.push(q(self.mischer.links[i]));
                    self.aufn_proben.push(q(self.mischer.rechts[i]));
                } else {
                    self.aufn_proben.push(q(self.mono[i]));
                }
            }
            aufnahme_ton_schreiben(&mut self.aufn_paket, self.krypto.sitzung, self.aufn_seq, self.aufn_t as u64, self.rate, kanaele, &self.aufn_proben);
            let _ = self.sock.send_to(&self.aufn_paket, server);
            self.aufn_seq = self.aufn_seq.wrapping_add(1);
            self.aufn_t += (ende - start) as f64 * 1e6 / rate;
            start = ende;
        }
    }
}

/// Schutz gegen wiederholt eingespielte Pakete (gleitendes 64er-Fenster)
struct Wiederholschutz {
    hoechster: u32,
    maske: u64,
    leer: bool,
}

impl Wiederholschutz {
    fn neu() -> Self {
        Wiederholschutz { hoechster: 0, maske: 0, leer: true }
    }
    fn pruefen(&mut self, z: u32) -> bool {
        if self.leer {
            self.leer = false;
            self.hoechster = z;
            self.maske = 1;
            return true;
        }
        if z > self.hoechster {
            let d = z - self.hoechster;
            self.maske = if d >= 64 { 1 } else { (self.maske << d) | 1 };
            self.hoechster = z;
            true
        } else {
            let d = self.hoechster - z;
            if d >= 64 || self.maske & (1 << d) != 0 {
                return false;
            }
            self.maske |= 1 << d;
            true
        }
    }
}

pub struct NetzDaten {
    pub sock: Arc<UdpSocket>,
    pub krypto: Krypto,
    pub geteilt: Arc<Geteilt>,
    pub server: String,
    pub raum: [u8; 32],
    pub raum_name: String,
    pub name: String,
}

struct Qos(windows::Win32::Foundation::HANDLE);

impl Qos {
    fn neu() -> Option<Qos> {
        use windows::Win32::NetworkManagement::QoS::*;
        let v = QOS_VERSION { MajorVersion: 1, MinorVersion: 0 };
        let mut h = windows::Win32::Foundation::HANDLE::default();
        unsafe { QOSCreateHandle(&v, &mut h).as_bool().then_some(Qos(h)) }
    }
    /// Pakete als Sprachverkehr markieren (DSCP 46/56) – hilft in WLANs mit WMM
    fn markieren(&self, sock: &UdpSocket, an: SocketAddr) {
        use std::os::windows::io::AsRawSocket;
        use windows::Win32::NetworkManagement::QoS::*;
        use windows::Win32::Networking::WinSock::*;
        let SocketAddr::V4(v4) = an else { return };
        let sa = SOCKADDR_IN {
            sin_family: AF_INET,
            sin_port: v4.port().to_be(),
            sin_addr: IN_ADDR { S_un: IN_ADDR_0 { S_addr: u32::from_ne_bytes(v4.ip().octets()) } },
            sin_zero: [0; 8],
        };
        let mut flow = 0u32;
        unsafe {
            let _ = QOSAddSocketToFlow(
                self.0,
                SOCKET(sock.as_raw_socket() as usize),
                Some(&sa as *const _ as *const SOCKADDR),
                QOSTrafficTypeVoice,
                QOS_NON_ADAPTIVE_FLOW,
                &mut flow,
            );
        }
    }
}

impl Drop for Qos {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::NetworkManagement::QoS::QOSCloseHandle(self.0);
        }
    }
}

struct Netz {
    d: NetzDaten,
    empfang: Empfang,
    start: Instant,
    kandidaten: Vec<SocketAddr>,
    dns_naechster: Instant,
    server: Option<SocketAddr>,
    server_zuletzt: Instant,
    server_seit: Instant,
    keks: u64,
    lokal: Vec<SocketAddr>,
    raum_voll: bool,
    partner: Option<Teilnehmer>,
    akt_sitzung: Option<u64>,
    wiederholschutz: Wiederholschutz,
    direkt: Option<SocketAddr>,
    direkt_zuletzt: Option<Instant>,
    lochstanzen_seit: Instant,
    extra_kandidaten: Vec<SocketAddr>,
    rtt_direkt: f32,
    rtt_relay: f32,
    weg: u8,
    t_hallo: Instant,
    t_ping_relay: Instant,
    t_ping_direkt: Instant,
    t_status: Instant,
    t_statistik: Instant,
    alt_empfangen: u64,
    alt_verloren: u64,
    krypto_fehler: u32,
    krypto_ok: Option<Instant>,
    audio_zuletzt: Option<Instant>,
    spektrum: Spektrum,
    waechter: Waechter,
    proben: Vec<f32>,
    proben_vorher: Vec<f32>,
    mono: Vec<f32>,
    raus: Vec<u8>,
    klar: Vec<u8>,
    qos: Option<Qos>,
    qos_markiert: Vec<SocketAddr>,
    /// (RTT, Versatz) der letzten Uhrabgleiche
    uhr_proben: Vec<(u64, i64)>,
    /// Metronom-Wunsch, bis der Server ihn bestätigt: (an, bpm, takt), letzter Versand, Versuche
    metronom_offen: Option<((bool, f32, u8), Option<Instant>, u32)>,
    aufnahme_offen: Option<((bool, [u8; 32], String, Option<u8>), Option<Instant>, u32)>,
    /// Spur-Wunsch, Stand des Servers beim Wunsch, letzter Versand, Versuche
    spur_offen: Option<((bool, [u8; 32], String, u32), u32, Option<Instant>, u32)>,
}

fn ms_seit(t: Option<Instant>) -> f32 {
    t.map(|t| t.elapsed().as_secs_f32() * 1000.0).unwrap_or(f32::MAX)
}

impl Netz {
    fn senden(&mut self, an: SocketAddr) {
        let _ = self.d.sock.send_to(&self.raus, an);
    }

    fn server_aufloesen(&mut self) {
        let host = self.d.server.trim().to_string();
        let mut liste = Vec::new();
        // Eintrag „host:port“ = nur dieser Port, sonst alle Standardports
        let ports: Vec<u16> = match host.rsplit_once(':').and_then(|(_, p)| p.parse::<u16>().ok()) {
            Some(p) => vec![p],
            None => STANDARD_PORTS.to_vec(),
        };
        let name = match host.rsplit_once(':') {
            Some((h, p)) if p.parse::<u16>().is_ok() => h.to_string(),
            _ => host.clone(),
        };
        for p in ports {
            if let Ok(it) = (name.as_str(), p).to_socket_addrs() {
                if let Some(a) = it.into_iter().find(|a| a.is_ipv4()) {
                    liste.push(a);
                }
            }
        }
        if liste.is_empty() {
            self.d.geteilt.fehler(format!("Server „{name}“ nicht gefunden (Internet/DNS prüfen)"));
            self.dns_naechster = Instant::now() + Duration::from_secs(5);
            return;
        }
        *self.d.geteilt.anzeige.fehler.lock().unwrap() = None;
        // eigene lokale Adresse (für zwei Räume im selben lokalen Netz)
        if let Ok(s) = UdpSocket::bind("0.0.0.0:0") {
            if s.connect(liste[0]).is_ok() {
                if let (Ok(a), Ok(eigen)) = (s.local_addr(), self.d.sock.local_addr()) {
                    self.lokal = vec![SocketAddr::new(a.ip(), eigen.port())];
                }
            }
        }
        self.kandidaten = liste;
    }

    fn hallo_senden(&mut self) {
        let h = Hallo { raum: self.d.raum, keks: self.keks, name: self.d.name.clone(), lokal: self.lokal.clone(), zeit: uhr_us() };
        h.schreiben(&mut self.raus, self.d.krypto.sitzung);
        match self.server {
            Some(s) => self.senden(s),
            None => {
                for a in self.kandidaten.clone() {
                    self.senden(a);
                }
            }
        }
        self.t_hallo = Instant::now();
    }

    fn statistik_inhalt(&self, art: u8, weg: u8, t_us: u64) -> Vec<u8> {
        let a = &self.d.geteilt.anzeige;
        let mut k = Vec::with_capacity(32);
        k.push(art);
        k.push(weg);
        k.extend_from_slice(&[0, 0]);
        k.extend_from_slice(&t_us.to_le_bytes());
        for v in [a.puffer_ist_ms.get(), a.aus_latenz_ms.get(), a.ein_latenz_ms.get(), a.verlust_prozent.get()] {
            k.extend_from_slice(&v.to_le_bytes());
        }
        k.push(self.d.geteilt.regler.stumm.load(Relaxed) as u8);
        k
    }

    fn ping_senden(&mut self, an: SocketAddr, direkt: bool) {
        let t = self.start.elapsed().as_micros() as u64;
        self.klar = self.statistik_inhalt(0, direkt as u8, t);
        self.d.krypto.versiegeln(T_PING, &self.klar, &mut self.raus);
        self.senden(an);
    }

    fn partner_wechsel(&mut self, sitzung: u64) {
        self.akt_sitzung = Some(sitzung);
        self.wiederholschutz = Wiederholschutz::neu();
        self.empfang.zuruecksetzen();
        self.direkt = None;
        self.direkt_zuletzt = None;
        self.extra_kandidaten.clear();
        self.rtt_direkt = -1.0;
        self.rtt_relay = -1.0;
        self.krypto_fehler = 0;
        self.krypto_ok = None;
        self.audio_zuletzt = None;
        self.lochstanzen_seit = Instant::now();
    }

    fn stand(&mut self, st: Stand) {
        let g = self.d.geteilt.clone();
        if st.zeit_server != 0 {
            if st.zeit_app != 0 {
                // Uhrabgleich wie NTP: die Messung mit der kleinsten Laufzeit ist die genaueste
                let jetzt = uhr_us();
                let rtt = jetzt.saturating_sub(st.zeit_app);
                if rtt < 2_000_000 {
                    let versatz = st.zeit_server as i64 - (st.zeit_app + rtt / 2) as i64;
                    self.uhr_proben.push((rtt, versatz));
                    if self.uhr_proben.len() > 8 {
                        self.uhr_proben.remove(0);
                    }
                    if let Some(&(_, v)) = self.uhr_proben.iter().min_by_key(|p| p.0) {
                        g.uhr_versatz.store(v, Relaxed);
                        g.uhr_ok.store(true, Relaxed);
                    }
                }
            }
            let m = st.metronom;
            let z = &g.metronom;
            z.bpm.set(m.bpm.max(1.0));
            z.takt.store(m.takt.max(1) as u32, Relaxed);
            z.start_us.store(m.start_us, Relaxed);
            z.an.store(m.an, Relaxed);
            z.stand.store(m.stand, Relaxed);
            if let Some(((an, bpm, takt), _, _)) = self.metronom_offen {
                if m.an == an && (!an || ((m.bpm - bpm).abs() < 0.01 && m.takt == takt)) {
                    self.metronom_offen = None;
                }
            }
            let a = st.aufnahme;
            g.aufnahme_an.store(a.an, Relaxed);
            g.aufnahme_start.store(a.start_us, Relaxed);
            *g.aufnahme_name.lock().unwrap() = a.name;
            if self.aufnahme_offen.as_ref().is_some_and(|w| w.0 .0 == a.an) {
                self.aufnahme_offen = None;
            }
            let sp = st.spur;
            *g.spur_datei.lock().unwrap() = (sp.datei.clone(), sp.kennung);
            g.spur_start.store(sp.start_us, Relaxed);
            g.spur_stand.store(sp.stand, Relaxed);
            g.spur_an.store(sp.an, Relaxed);
            // bestätigt, wenn der Server umgestellt hat (Stand gezählt) bzw. schon aus ist
            if self.spur_offen.as_ref().is_some_and(|w| (sp.an == w.0 .0 && sp.stand != w.1) || (!w.0 .0 && !sp.an)) {
                self.spur_offen = None;
            }
        }
        let a = &self.d.geteilt.anzeige;
        match st.status {
            STATUS_VOLL => {
                self.raum_voll = true;
                self.partner = None;
                return;
            }
            STATUS_VERALTET => {
                self.d.geteilt.fehler("Diese Version ist zu alt – bitte die neue Jamcenter holen");
                return;
            }
            _ => self.raum_voll = false,
        }
        match st.andere.into_iter().next() {
            Some(n) => {
                *a.partner.lock().unwrap() = n.name.clone();
                a.partner_da.store(true, Relaxed);
                let neu = self.akt_sitzung != Some(n.sitzung);
                self.partner = Some(n.clone());
                if neu {
                    self.partner_wechsel(n.sitzung);
                }
            }
            None => {
                // Gegenstelle hat den Raum verlassen (direkter Weg darf noch kurz weiterlaufen)
                if self.partner.is_some() && ms_seit(self.direkt_zuletzt) > 1500.0 {
                    self.akt_sitzung = None;
                    self.direkt = None;
                    a.partner_da.store(false, Relaxed);
                }
                self.partner = None;
            }
        }
    }

    fn empfangen(&mut self, buf: &mut [u8], von: SocketAddr) {
        let Some((typ, sitzung)) = kopf_lesen(buf) else { return };
        let vom_server = self.server == Some(von) || (self.server.is_none() && self.kandidaten.contains(&von));
        match typ {
            T_PRUEFUNG if vom_server => {
                if let Some(k) = pruefung_lesen(buf) {
                    self.keks = k;
                    if self.server.is_none() {
                        self.server = Some(von);
                        self.server_seit = Instant::now();
                    }
                    self.server_zuletzt = Instant::now();
                    self.hallo_senden();
                }
            }
            T_STAND if vom_server => {
                if self.server.is_none() {
                    self.server = Some(von);
                    self.server_seit = Instant::now();
                }
                self.server_zuletzt = Instant::now();
                if let Some(st) = Stand::lesen(buf) {
                    self.stand(st);
                }
            }
            T_AUDIO | T_PING => {
                if sitzung == self.d.krypto.sitzung {
                    return;
                }
                let krypto = self.d.krypto.clone();
                let Some((s, z, inhalt)) = krypto.oeffnen(buf) else {
                    if self.partner.as_ref().map(|p| p.sitzung) == Some(sitzung) {
                        self.krypto_fehler += 1;
                    }
                    return;
                };
                if self.akt_sitzung != Some(s) {
                    // gültiger Schlüssel = gehört zum Raum (z. B. Neustart der Gegenstelle)
                    self.partner_wechsel(s);
                    self.d.geteilt.anzeige.partner_da.store(true, Relaxed);
                }
                if !self.wiederholschutz.pruefen(z) {
                    return;
                }
                self.krypto_fehler = 0;
                self.krypto_ok = Some(Instant::now());
                let direkt = !vom_server;
                if typ == T_AUDIO {
                    self.audio(inhalt);
                } else {
                    self.ping(inhalt, von, direkt);
                }
            }
            _ => {}
        }
    }

    fn audio(&mut self, k: &[u8]) {
        let mut l = Leser::neu(k, 0);
        let (Some(codec), Some(flags), Some(frames), Some(rate), Some(seq)) = (l.u8(), l.u8(), l.u16(), l.u32(), l.u32()) else {
            return;
        };
        let kanaele = match codec {
            CODEC_MONO => 1,
            CODEC_STEREO => 2,
            _ => return,
        };
        if frames as usize > 2048 {
            return;
        }
        // immer als Stereo-Paare in den Puffer (Mono auf beide Seiten)
        let lesen = |daten: &[u8], ziel: &mut Vec<f32>| {
            for b in daten.chunks_exact(2 * kanaele) {
                let l = i16::from_le_bytes([b[0], b[1]]) as f32 * (1.0 / 32768.0);
                let r = if kanaele == 2 { i16::from_le_bytes([b[2], b[3]]) as f32 * (1.0 / 32768.0) } else { l };
                ziel.push(l);
                ziel.push(r);
            }
        };
        let Some(daten) = l.bytes(frames as usize * 2 * kanaele) else { return };
        self.proben.clear();
        lesen(daten, &mut self.proben);
        self.proben_vorher.clear();
        let mut hat_vorher = false;
        if flags & 1 != 0 {
            if let Some(n) = l.u16() {
                if let Some(v) = l.bytes(n as usize * 2 * kanaele) {
                    lesen(v, &mut self.proben_vorher);
                    hat_vorher = true;
                }
            }
        }
        self.audio_zuletzt = Some(Instant::now());
        let vorher = if hat_vorher { Some(&self.proben_vorher[..]) } else { None };
        self.empfang.paket(seq, rate, &self.proben, vorher);
        // Rückkopplungsschutz auf der Mono-Summe
        self.mono.clear();
        self.mono.extend(self.proben.chunks_exact(2).map(|p| 0.5 * (p[0] + p[1])));

        let g = &self.d.geteilt;
        if g.regler.schutz.load(Relaxed) {
            let mikro = Ergebnis { frequenz: g.analyse.frequenz.get(), anteil: g.analyse.anteil.get(), pegel_db: g.analyse.pegel_db.get() };
            let block_ms = crate::analyse::N as f32 * 1000.0 / rate as f32;
            let waechter = &mut self.waechter;
            self.spektrum.zufuehren(&self.mono, rate, |e| {
                if waechter.pruefen(e, mikro, block_ms) {
                    let l = g.regler.lautstaerke_db.get();
                    g.regler.lautstaerke_db.set((l - 6.0).max(-40.0));
                    g.anzeige.rueckkopplungen.fetch_add(1, Relaxed);
                }
            });
        }
    }

    fn ping(&mut self, k: &[u8], von: SocketAddr, direkt: bool) {
        let mut l = Leser::neu(k, 0);
        let (Some(art), Some(_weg), Some(_), Some(t)) = (l.u8(), l.u8(), l.u16(), l.u64()) else { return };
        let (Some(puffer), Some(aus), Some(ein), Some(verlust)) = (l.f32(), l.f32(), l.f32(), l.f32()) else { return };
        let a = &self.d.geteilt.anzeige;
        a.partner_puffer_ms.set(puffer);
        a.partner_aus_ms.set(aus);
        a.partner_ein_ms.set(ein);
        a.partner_verlust.set(verlust);
        if art == 0 {
            // Ping beantworten – auf demselben Weg zurück
            self.klar = self.statistik_inhalt(1, direkt as u8, t);
            self.d.krypto.versiegeln(T_PING, &self.klar, &mut self.raus);
            let ziel = if direkt { Some(von) } else { self.server };
            if let Some(z) = ziel {
                self.senden(z);
            }
            if direkt && !self.extra_kandidaten.contains(&von) && self.extra_kandidaten.len() < 4 {
                self.extra_kandidaten.push(von);
            }
        } else {
            let jetzt = self.start.elapsed().as_micros() as u64;
            let rtt = jetzt.saturating_sub(t) as f32 / 1000.0;
            if rtt > 5000.0 {
                return;
            }
            let glatt = |alt: f32| if alt < 0.0 { rtt } else { alt + (rtt - alt) * 0.2 };
            if direkt {
                self.rtt_direkt = glatt(self.rtt_direkt);
                self.direkt = Some(von);
                self.direkt_zuletzt = Some(Instant::now());
                a.rtt_direkt_ms.set(self.rtt_direkt);
            } else {
                self.rtt_relay = glatt(self.rtt_relay);
                a.rtt_relay_ms.set(self.rtt_relay);
            }
        }
    }

    fn takt(&mut self) {
        let jetzt = Instant::now();
        let g = self.d.geteilt.clone();
        if self.kandidaten.is_empty() {
            if jetzt >= self.dns_naechster {
                self.server_aufloesen();
            }
            if self.kandidaten.is_empty() {
                return;
            }
        }
        let hallo_takt = if self.server.is_some() { 1000 } else { 300 };
        if jetzt.duration_since(self.t_hallo) >= Duration::from_millis(hallo_takt) {
            self.hallo_senden();
        }
        if self.server.is_some() && jetzt.duration_since(self.server_zuletzt) > Duration::from_secs(4) {
            self.server = None;
            self.keks = 0;
            self.server_seit = jetzt;
        }

        // Metronom-Wunsch der Oberfläche an den Server (wiederholen, bis er bestätigt ist)
        if let Some(w) = g.metronom_wunsch.lock().unwrap().take() {
            self.metronom_offen = Some((w, None, 0));
        }
        if let (Some((w, zuletzt, versuche)), Some(srv)) = (self.metronom_offen, self.server) {
            if zuletzt.map_or(true, |t| jetzt.duration_since(t) > Duration::from_millis(400)) {
                if versuche >= 5 {
                    self.metronom_offen = None;
                } else {
                    metronom_wunsch_schreiben(&mut self.raus, self.d.krypto.sitzung, w.0, w.1, w.2);
                    self.senden(srv);
                    self.metronom_offen = Some((w, Some(jetzt), versuche + 1));
                }
            }
        }

        // Aufnahme starten/stoppen (wiederholen, bis der Server es bestätigt)
        if let Some(w) = g.aufnahme_wunsch.lock().unwrap().take() {
            self.aufnahme_offen = Some((w, None, 0));
        }
        if let (Some((w, zuletzt, versuche)), Some(srv)) = (self.aufnahme_offen.clone(), self.server) {
            if zuletzt.map_or(true, |t| jetzt.duration_since(t) > Duration::from_millis(400)) {
                if versuche >= 8 {
                    self.aufnahme_offen = None;
                } else {
                    aufnahme_wunsch_schreiben(&mut self.raus, self.d.krypto.sitzung, w.0, &w.1, &w.2, w.3);
                    self.senden(srv);
                    self.aufnahme_offen = Some((w, Some(jetzt), versuche + 1));
                }
            }
        }
        // Spur für den Raum starten/stoppen (wiederholen, bis der Server es bestätigt)
        if let Some(w) = g.spur_wunsch.lock().unwrap().take() {
            self.spur_offen = Some((w, g.spur_stand.load(Relaxed), None, 0));
        }
        if let (Some((w, stand, zuletzt, versuche)), Some(srv)) = (self.spur_offen.clone(), self.server) {
            if zuletzt.map_or(true, |t| jetzt.duration_since(t) > Duration::from_millis(400)) {
                if versuche >= 8 {
                    self.spur_offen = None;
                } else {
                    spur_wunsch_schreiben(&mut self.raus, self.d.krypto.sitzung, w.0, &w.1, &w.2, w.3);
                    self.senden(srv);
                    self.spur_offen = Some((w, stand, Some(jetzt), versuche + 1));
                }
            }
        }
        g.server_ziel.store(ziel_kodieren(self.server), Relaxed);

        // Pings: über den Server und direkt (Lochstanzen)
        let direkt_ok = ms_seit(self.direkt_zuletzt) < 1200.0;
        if self.akt_sitzung.is_some() || self.partner.is_some() {
            if let Some(s) = self.server {
                if jetzt.duration_since(self.t_ping_relay) >= Duration::from_millis(200) {
                    self.ping_senden(s, false);
                    self.t_ping_relay = jetzt;
                }
            }
            let takt = if direkt_ok {
                200
            } else if jetzt.duration_since(self.lochstanzen_seit) < Duration::from_secs(15) {
                250
            } else {
                2000
            };
            if jetzt.duration_since(self.t_ping_direkt) >= Duration::from_millis(takt) {
                self.t_ping_direkt = jetzt;
                if direkt_ok {
                    if let Some(d) = self.direkt {
                        self.ping_senden(d, true);
                    }
                } else {
                    let mut ziele: Vec<SocketAddr> = Vec::new();
                    if let Some(p) = &self.partner {
                        ziele.push(p.oeffentlich);
                        ziele.extend(p.lokal.iter().copied());
                    }
                    ziele.extend(self.extra_kandidaten.iter().copied());
                    ziele.retain(|a| a.is_ipv4() && Some(*a) != self.server);
                    ziele.dedup();
                    for z in ziele {
                        self.ping_senden(z, true);
                    }
                }
            }
        }

        // Weg wählen (mit Hysterese)
        let relay_ok = self.server.is_some() && self.akt_sitzung.is_some();
        let direkt_besser = |schwelle: f32| self.rtt_relay < 0.0 || self.rtt_direkt <= self.rtt_relay + schwelle;
        let weg = if direkt_ok && (!relay_ok || direkt_besser(if self.weg == 2 { 3.0 } else { 1.0 })) {
            2
        } else if relay_ok {
            1
        } else {
            0
        };
        self.weg = weg;
        let ziel = match weg {
            2 => self.direkt,
            1 => self.server,
            _ => None,
        };
        g.ziel.store(ziel_kodieren(ziel), Relaxed);
        g.anzeige.weg.store(weg, Relaxed);
        if let (Some(q), Some(z)) = (&self.qos, ziel) {
            if !self.qos_markiert.contains(&z) {
                q.markieren(&self.d.sock, z);
                self.qos_markiert.push(z);
            }
        }

        if jetzt.duration_since(self.t_statistik) >= Duration::from_secs(1) {
            self.t_statistik = jetzt;
            let e = self.empfang.empfangen - self.alt_empfangen;
            let v = self.empfang.verloren - self.alt_verloren;
            self.alt_empfangen = self.empfang.empfangen;
            self.alt_verloren = self.empfang.verloren;
            let p = if e + v > 0 { v as f32 * 100.0 / (e + v) as f32 } else { 0.0 };
            let alt = g.anzeige.verlust_prozent.get();
            g.anzeige.verlust_prozent.set(alt + (p - alt) * 0.3);
        }

        if jetzt.duration_since(self.t_status) >= Duration::from_millis(250) {
            self.t_status = jetzt;
            let partner = g.anzeige.partner.lock().unwrap().clone();
            let text = if self.raum_voll {
                format!("Raum „{}“ ist schon mit zwei Teilnehmern belegt.", self.d.raum_name)
            } else if self.server.is_none() && weg == 0 {
                if jetzt.duration_since(self.server_seit) > Duration::from_secs(3) {
                    "Server nicht erreichbar – UDP wird vermutlich von der Firewall gesperrt (Ports 3478, 443, 47800).".into()
                } else {
                    "Verbinde mit dem Server …".into()
                }
            } else if self.akt_sitzung.is_none() {
                format!("Im Raum „{}“ – warte auf die Gegenstelle …", self.d.raum_name)
            } else if self.krypto_fehler > 20 && ms_seit(self.krypto_ok) > 2000.0 {
                format!("„{partner}“ ist da, aber das Passwort stimmt nicht überein.")
            } else {
                let w = match weg {
                    2 => "direkte Verbindung",
                    1 => "über den Server",
                    _ => "Verbindung wird aufgebaut",
                };
                let ton = if ms_seit(self.audio_zuletzt) > 1000.0 { " – noch kein Ton von dort" } else { "" };
                format!("Verbunden mit „{partner}“ ({w}){ton}")
            };
            g.status(text);
        }
    }
}

pub fn laufen(d: NetzDaten, empfang: Empfang) {
    let _mm = crate::audio::Mmcss::neu();
    let _ = d.sock.set_read_timeout(Some(Duration::from_millis(5)));
    let jetzt = Instant::now();
    let fern = jetzt - Duration::from_secs(10);
    let mut n = Netz {
        d,
        empfang,
        start: jetzt,
        kandidaten: Vec::new(),
        dns_naechster: jetzt,
        server: None,
        server_zuletzt: jetzt,
        server_seit: jetzt,
        keks: 0,
        lokal: Vec::new(),
        raum_voll: false,
        partner: None,
        akt_sitzung: None,
        wiederholschutz: Wiederholschutz::neu(),
        direkt: None,
        direkt_zuletzt: None,
        lochstanzen_seit: jetzt,
        extra_kandidaten: Vec::new(),
        rtt_direkt: -1.0,
        rtt_relay: -1.0,
        weg: 0,
        t_hallo: fern,
        t_ping_relay: fern,
        t_ping_direkt: fern,
        t_status: fern,
        t_statistik: jetzt,
        alt_empfangen: 0,
        alt_verloren: 0,
        krypto_fehler: 0,
        krypto_ok: None,
        audio_zuletzt: None,
        spektrum: Spektrum::neu(),
        waechter: Waechter::neu(),
        proben: Vec::with_capacity(4096),
        proben_vorher: Vec::with_capacity(4096),
        mono: Vec::with_capacity(4096),
        raus: Vec::with_capacity(2048),
        klar: Vec::with_capacity(64),
        qos: Qos::neu(),
        qos_markiert: Vec::new(),
        uhr_proben: Vec::new(),
        metronom_offen: None,
        aufnahme_offen: None,
        spur_offen: None,
    };
    let mut buf = [0u8; 2048];
    while !n.d.geteilt.stop.load(Relaxed) {
        match n.d.sock.recv_from(&mut buf) {
            Ok((len, von)) => n.empfangen(&mut buf[..len], von),
            // Zeitüberschreitung, oder ICMP „Port nicht erreichbar“ (Windows meldet das als Fehler)
            Err(_) => {}
        }
        n.takt();
    }
    if let Some(s) = n.server {
        kopf_schreiben(&mut n.raus, T_TSCHUESS, n.d.krypto.sitzung);
        let _ = n.d.sock.send_to(&n.raus, s);
    }
    n.d.geteilt.ziel.store(0, Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Kosten der Verschlüsselung je Paket (128 Proben + Kopie = 528 Byte Nutzdaten)
    #[test]
    fn kosten_verschluesselung() {
        let k = Krypto::neu(&[7u8; 32], 42);
        let inhalt = vec![1u8; 528];
        let mut paket = Vec::new();
        let n = 20_000;
        let start = std::time::Instant::now();
        for _ in 0..n {
            k.versiegeln(T_AUDIO, &inhalt, &mut paket);
            let mut kopie = paket.clone();
            assert!(k.oeffnen(&mut kopie).is_some());
        }
        let us = start.elapsed().as_secs_f64() * 1e6 / n as f64;
        println!("Verschlüsseln + Entschlüsseln je Paket: {us:.2} µs = {:.4} ms", us / 1000.0);
    }
}
