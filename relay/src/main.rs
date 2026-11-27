//! Jamcenter-Relay: Treffpunkt und Weiterleitung für die Audio-App.
//!
//! - Apps melden sich mit einer Raumkennung an (Hash aus Raumname + Passwort).
//! - Der Server nennt jeder App die öffentliche und lokale Adresse des Gegenübers,
//!   damit beide eine direkte Verbindung (UDP-Lochstanzen) versuchen können.
//! - Klappt das nicht, leitet er die verschlüsselten Audiopakete unverändert weiter.
//!
//! Absichtlich ohne Async-Laufzeit: je Port ein Thread mit blockierendem recv_from,
//! das ist die kürzeste mögliche Weiterleitungszeit (wenige Mikrosekunden).

mod aufnahme;

use protokoll::*;
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::hash_map::RandomState;
use std::collections::HashMap;
use std::hash::BuildHasher;
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const ZEITLIMIT: Duration = Duration::from_secs(6);
const MAX_RAEUME: usize = 500;

struct Mitglied {
    sitzung: u64,
    adresse: SocketAddr,
    sock: usize,
    name: String,
    lokal: Vec<SocketAddr>,
    zuletzt: Instant,
}

#[derive(Default)]
struct Zustand {
    raeume: HashMap<[u8; 32], Vec<Mitglied>>,
    sitzungen: HashMap<u64, [u8; 32]>,
    metronome: HashMap<[u8; 32], Metronom>,
    aufnahmen: HashMap<[u8; 32], Aufnahme>,
    /// Spur je Raum und der Startpunkt (ms) des letzten Wunsches (doppelte Wünsche erkennen)
    spuren: HashMap<[u8; 32], (Spur, u32, Instant)>,
}

struct Server {
    socks: Vec<Arc<UdpSocket>>,
    zustand: Mutex<Zustand>,
    geheim: RandomState,
    max: usize,
    /// Bezugspunkt der Serveruhr (µs seit Start)
    beginn: Instant,
    aufnehmer: SyncSender<aufnahme::Auftrag>,
}

fn kurz(raum: &[u8; 32]) -> String {
    raum[..4].iter().map(|b| format!("{b:02x}")).collect()
}

fn sock_oeffnen(port: u16) -> std::io::Result<UdpSocket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    // DSCP EF (Expedited Forwarding) – in WLANs mit WMM landet das in der Sprach-Warteschlange
    let _ = s.set_tos(0xB8);
    let _ = s.set_recv_buffer_size(1 << 20);
    let _ = s.set_send_buffer_size(1 << 20);
    s.bind(&SocketAddr::from(([0, 0, 0, 0], port)).into())?;
    Ok(s.into())
}

impl Server {
    fn senden(&self, sock: usize, buf: &[u8], an: SocketAddr) {
        let _ = self.socks[sock].send_to(buf, an);
    }

    fn uhr(&self) -> u64 {
        self.beginn.elapsed().as_micros() as u64
    }

    /// Stand an ein Mitglied (andere = übrige Mitglieder des Raums)
    #[allow(clippy::too_many_arguments)]
    fn stand_fuer(&self, mitglieder: &[Mitglied], fuer: &Mitglied, metronom: Metronom, aufnahme: &Aufnahme, spur: &Spur, zeit_app: u64, antwort: &mut Vec<u8>) {
        let andere = mitglieder
            .iter()
            .filter(|m| m.sitzung != fuer.sitzung)
            .map(|m| Teilnehmer { sitzung: m.sitzung, name: m.name.clone(), oeffentlich: m.adresse, lokal: m.lokal.clone() })
            .collect();
        Stand { status: STATUS_OK, du: fuer.adresse, andere, zeit_app, zeit_server: self.uhr(), metronom, aufnahme: aufnahme.clone(), spur: spur.clone() }.schreiben(antwort);
    }

    /// Kopie der Mitglieder eines Raums (zum Senden ohne gehaltene Sperre)
    fn mitglieder_kopie(z: &Zustand, raum: &[u8; 32]) -> Vec<Mitglied> {
        z.raeume
            .get(raum)
            .map(|m| m.iter().map(|x| Mitglied { sitzung: x.sitzung, adresse: x.adresse, sock: x.sock, name: x.name.clone(), lokal: x.lokal.clone(), zuletzt: x.zuletzt }).collect())
            .unwrap_or_default()
    }

    /// Allen im Raum sofort den neuen Stand schicken
    fn stand_an_alle(&self, raum: &[u8; 32], antwort: &mut Vec<u8>) {
        let z = self.zustand.lock().unwrap();
        let liste = Self::mitglieder_kopie(&z, raum);
        let metronom = z.metronome.get(raum).copied().unwrap_or_default();
        let aufnahme = z.aufnahmen.get(raum).cloned().unwrap_or_default();
        let spur = z.spuren.get(raum).map(|s| s.0.clone()).unwrap_or_default();
        drop(z);
        for m in &liste {
            self.stand_fuer(&liste, m, metronom, &aufnahme, &spur, 0, antwort);
            self.senden(m.sock, antwort, m.adresse);
        }
    }

    /// Raum der Sitzung, wenn das Paket wirklich von ihr kommt
    fn raum_von(&self, z: &Zustand, von: SocketAddr, sitzung: u64) -> Option<[u8; 32]> {
        let raum = z.sitzungen.get(&sitzung).copied()?;
        z.raeume.get(&raum)?.iter().any(|m| m.sitzung == sitzung && m.adresse == von).then_some(raum)
    }

    /// Aufnahme starten oder stoppen (gilt für den ganzen Raum)
    fn aufnahme(&self, buf: &[u8], von: SocketAddr, sitzung: u64, antwort: &mut Vec<u8>) {
        let Some((an, kennung, name, klang)) = aufnahme_wunsch_lesen(buf) else { return };
        let jetzt = self.uhr();
        let mut z = self.zustand.lock().unwrap();
        let Some(raum) = self.raum_von(&z, von, sitzung) else { return };
        let laeuft = z.aufnahmen.get(&raum).is_some_and(|a| a.an);
        if an && !laeuft {
            let name: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').take(40).collect();
            let name = if name.is_empty() { "Aufnahme".to_string() } else { name };
            z.aufnahmen.insert(raum, Aufnahme { an: true, start_us: jetzt, name: name.clone() });
            let metronom = klang.map(|k| (k, z.metronome.get(&raum).copied().unwrap_or_default()));
            let spur = z.spuren.get(&raum).map(|s| s.0.clone()).filter(|s| s.an);
            let _ = self.aufnehmer.try_send(aufnahme::Auftrag::Start { raum, kennung, name, start_us: jetzt, metronom, spur });
            println!("Raum {}: Aufnahme gestartet", kurz(&raum));
        } else if !an && laeuft {
            z.aufnahmen.remove(&raum);
            let _ = self.aufnehmer.send(aufnahme::Auftrag::Stop { raum });
            println!("Raum {}: Aufnahme gestoppt", kurz(&raum));
        }
        drop(z);
        self.stand_an_alle(&raum, antwort);
    }

    /// Spur für den ganzen Raum starten/stoppen. Start mit 2 s Vorlauf, damit alle Apps die Datei
    /// holen können (wer später fertig ist, steigt an der richtigen Stelle ein).
    fn spur(&self, buf: &[u8], von: SocketAddr, sitzung: u64, antwort: &mut Vec<u8>) {
        let Some((an, kennung, datei, ab_ms)) = spur_wunsch_lesen(buf) else { return };
        if an && (!aufnahme::datei_ok(&datei) || !aufnahme::basis().join(aufnahme::hex(&kennung)).join(&datei).is_file()) {
            return;
        }
        let jetzt = self.uhr();
        let mut z = self.zustand.lock().unwrap();
        let Some(raum) = self.raum_von(&z, von, sitzung) else { return };
        let (alt, alt_ab, alt_t) = z.spuren.get(&raum).cloned().unwrap_or_else(|| (Spur::default(), 0, Instant::now()));
        // derselbe Wunsch noch einmal (Wiederholung bis zur Bestätigung): nichts ändern.
        // Später derselbe Wunsch = neu starten (z. B. Spur noch einmal von vorn).
        let gleich = alt.an == an && (!an || (alt.datei == datei && alt.kennung == kennung && alt_ab == ab_ms && alt_t.elapsed() < Duration::from_millis(3500)));
        if !gleich {
            let neu = Spur {
                an,
                kennung: if an { kennung } else { alt.kennung },
                datei: if an { datei } else { alt.datei.clone() },
                start_us: if an { (jetzt + 2_000_000).saturating_sub(ab_ms as u64 * 1000) } else { alt.start_us },
                stand: alt.stand.wrapping_add(1),
            };
            println!("Raum {}: Spur {} {}", kurz(&raum), if an { "startet" } else { "gestoppt" }, neu.datei);
            if z.aufnahmen.get(&raum).is_some_and(|a| a.an) {
                let _ = self.aufnehmer.try_send(aufnahme::Auftrag::Spur { raum, spur: neu.clone(), jetzt });
            }
            z.spuren.insert(raum, (neu, ab_ms, Instant::now()));
        }
        drop(z);
        self.stand_an_alle(&raum, antwort);
    }

    /// Ton einer App für die laufende Aufnahme in die Warteschlange (blockiert nie)
    fn aufnahme_ton(&self, buf: &[u8], von: SocketAddr, sitzung: u64) {
        let Some(t) = aufnahme_ton_lesen(buf) else { return };
        let raum = {
            let z = self.zustand.lock().unwrap();
            let Some(raum) = self.raum_von(&z, von, sitzung) else { return };
            if !z.aufnahmen.get(&raum).is_some_and(|a| a.an) {
                return;
            }
            raum
        };
        let _ = self.aufnehmer.try_send(aufnahme::Auftrag::Ton {
            raum,
            sitzung,
            seq: t.seq,
            zeit_us: t.zeit_us,
            rate: t.rate,
            kanaele: t.kanaele,
            daten: t.daten.to_vec(),
        });
    }

    /// Jemand im Raum stellt das Metronom um: Tempo-/Taktwechsel am nächsten Taktanfang,
    /// Start mit 0,3 s Vorlauf. Danach bekommen alle im Raum sofort einen neuen Stand.
    fn metronom(&self, buf: &[u8], von: SocketAddr, sitzung: u64, antwort: &mut Vec<u8>) {
        let Some((an, bpm, takt)) = metronom_wunsch_lesen(buf) else { return };
        if !bpm.is_finite() {
            return;
        }
        let jetzt = self.uhr();
        let mut z = self.zustand.lock().unwrap();
        let Some(raum) = z.sitzungen.get(&sitzung).copied() else { return };
        let Some(mitglieder) = z.raeume.get(&raum) else { return };
        if !mitglieder.iter().any(|m| m.sitzung == sitzung && m.adresse == von) {
            return;
        }
        let alt = z.metronome.get(&raum).copied().unwrap_or_default();
        let bpm = bpm.clamp(20.0, 300.0);
        let takt = takt.clamp(1, 12);
        // Gleicher Wunsch noch einmal (z. B. wiederholt, weil die Bestätigung unterwegs war):
        // nichts ändern – sonst begänne das Metronom neu und Schläge fielen aus
        if alt.an == an && (!an || (alt.bpm == bpm && alt.takt == takt)) {
            drop(z);
            self.stand_an_alle(&raum, antwort);
            return;
        }
        let start_us = if !an {
            alt.start_us
        } else if alt.an && alt.bpm > 0.0 {
            let takt_us = 60e6 / alt.bpm as f64 * alt.takt.max(1) as f64;
            let seit = (jetzt + 150_000).saturating_sub(alt.start_us) as f64;
            alt.start_us + ((seit / takt_us).ceil() * takt_us) as u64
        } else {
            jetzt + 300_000
        };
        let neu = Metronom { an, bpm, takt, start_us, stand: alt.stand.wrapping_add(1) };
        z.metronome.insert(raum, neu);
        if z.aufnahmen.get(&raum).is_some_and(|a| a.an) {
            let _ = self.aufnehmer.try_send(aufnahme::Auftrag::Metronom { raum, metronom: neu, jetzt });
        }
        drop(z);
        println!("Raum {}: Metronom {} {} bpm, {}er-Takt", kurz(&raum), if an { "an" } else { "aus" }, bpm, takt);
        self.stand_an_alle(&raum, antwort);
    }

    fn keks(&self, von: SocketAddr, sitzung: u64) -> u64 {
        self.geheim.hash_one((von, sitzung)) | 1
    }

    fn hallo(&self, idx: usize, buf: &[u8], von: SocketAddr, sitzung: u64, antwort: &mut Vec<u8>) {
        let Some(h) = Hallo::lesen(buf) else { return };
        let keks = self.keks(von, sitzung);
        if h.keks != keks {
            // Adressnachweis: Antwort ist kleiner als die Anfrage -> keine Verstärkung
            pruefung_schreiben(antwort, keks);
            self.senden(idx, antwort, von);
            return;
        }
        let jetzt = Instant::now();
        let mut z = self.zustand.lock().unwrap();
        // Raumwechsel derselben Sitzung
        if let Some(alt) = z.sitzungen.get(&sitzung).copied() {
            if alt != h.raum {
                if let Some(m) = z.raeume.get_mut(&alt) {
                    m.retain(|m| m.sitzung != sitzung);
                }
                z.sitzungen.remove(&sitzung);
            }
        }
        if !z.raeume.contains_key(&h.raum) && z.raeume.len() >= MAX_RAEUME {
            return;
        }
        let max = self.max;
        let raum = z.raeume.entry(h.raum).or_default();
        raum.retain(|m| jetzt.duration_since(m.zuletzt) < ZEITLIMIT || m.sitzung == sitzung);
        let status;
        let mut neu = false;
        if let Some(m) = raum.iter_mut().find(|m| m.sitzung == sitzung) {
            m.adresse = von;
            m.sock = idx;
            m.name = h.name.clone();
            m.lokal = h.lokal.clone();
            m.zuletzt = jetzt;
            status = STATUS_OK;
        } else if raum.len() >= max {
            status = STATUS_VOLL;
        } else {
            raum.push(Mitglied {
                sitzung,
                adresse: von,
                sock: idx,
                name: h.name.clone(),
                lokal: h.lokal.clone(),
                zuletzt: jetzt,
            });
            status = STATUS_OK;
            neu = true;
        }
        let andere: Vec<Teilnehmer> = if status == STATUS_OK {
            raum.iter()
                .filter(|m| m.sitzung != sitzung)
                .map(|m| Teilnehmer {
                    sitzung: m.sitzung,
                    name: m.name.clone(),
                    oeffentlich: m.adresse,
                    lokal: m.lokal.clone(),
                })
                .collect()
        } else {
            Vec::new()
        };
        let anzahl = raum.len();
        if status == STATUS_OK {
            z.sitzungen.insert(sitzung, h.raum);
        }
        let metronom = z.metronome.get(&h.raum).copied().unwrap_or_default();
        let aufnahme = z.aufnahmen.get(&h.raum).cloned().unwrap_or_default();
        let spur = z.spuren.get(&h.raum).map(|s| s.0.clone()).unwrap_or_default();
        drop(z);
        if neu {
            println!("Raum {}: „{}“ beigetreten von {} (Port {}), jetzt {}", kurz(&h.raum), h.name, von, self.socks[idx].local_addr().map(|a| a.port()).unwrap_or(0), anzahl);
        } else if status == STATUS_VOLL {
            println!("Raum {}: „{}“ abgewiesen (Raum voll)", kurz(&h.raum), h.name);
        }
        Stand { status, du: von, andere, zeit_app: h.zeit, zeit_server: self.uhr(), metronom, aufnahme, spur }.schreiben(antwort);
        self.senden(idx, antwort, von);
    }

    fn tschuess(&self, von: SocketAddr, sitzung: u64) {
        let mut z = self.zustand.lock().unwrap();
        let Some(raum) = z.sitzungen.get(&sitzung).copied() else { return };
        let mut weg = None;
        if let Some(m) = z.raeume.get_mut(&raum) {
            if let Some(pos) = m.iter().position(|m| m.sitzung == sitzung && m.adresse == von) {
                weg = Some(m.remove(pos).name);
            }
            if m.is_empty() {
                z.raeume.remove(&raum);
                z.metronome.remove(&raum);
                if z.aufnahmen.remove(&raum).is_some() {
                    let _ = self.aufnehmer.send(aufnahme::Auftrag::Stop { raum });
                }
            }
        }
        if let Some(name) = weg {
            z.sitzungen.remove(&sitzung);
            println!("Raum {}: „{}“ abgemeldet", kurz(&raum), name);
        }
    }

    /// Verschlüsseltes App-zu-App-Paket an die übrigen Raummitglieder weitergeben.
    fn weiterleiten(&self, buf: &[u8], von: SocketAddr, sitzung: u64) {
        let mut ziele: [(SocketAddr, usize); MAX_TEILNEHMER] = [(von, 0); MAX_TEILNEHMER];
        let mut n = 0;
        {
            let mut z = self.zustand.lock().unwrap();
            let Some(raum) = z.sitzungen.get(&sitzung).copied() else { return };
            let Some(mitglieder) = z.raeume.get_mut(&raum) else { return };
            let mut gueltig = false;
            for m in mitglieder.iter_mut() {
                if m.sitzung == sitzung {
                    if m.adresse != von {
                        return;
                    }
                    m.zuletzt = Instant::now();
                    gueltig = true;
                } else if n < ziele.len() {
                    ziele[n] = (m.adresse, m.sock);
                    n += 1;
                }
            }
            if !gueltig {
                return;
            }
        }
        for &(an, sock) in &ziele[..n] {
            self.senden(sock, buf, an);
        }
    }

    fn lauschen(self: Arc<Self>, idx: usize) {
        let sock = self.socks[idx].clone();
        let mut buf = [0u8; 2048];
        let mut antwort = Vec::with_capacity(512);
        loop {
            let (n, von) = match sock.recv_from(&mut buf) {
                Ok(x) => x,
                Err(_) => continue,
            };
            let paket = &buf[..n];
            let Some((typ, sitzung)) = kopf_lesen(paket) else { continue };
            match typ {
                T_HALLO => self.hallo(idx, paket, von, sitzung, &mut antwort),
                T_TSCHUESS => self.tschuess(von, sitzung),
                T_METRONOM => self.metronom(paket, von, sitzung, &mut antwort),
                T_AUFNAHME => self.aufnahme(paket, von, sitzung, &mut antwort),
                T_AUFNAHME_TON => self.aufnahme_ton(paket, von, sitzung),
                T_SPUR => self.spur(paket, von, sitzung, &mut antwort),
                t if t >= 0x10 => self.weiterleiten(paket, von, sitzung),
                _ => {}
            }
        }
    }

    fn aufraeumen(&self) {
        let jetzt = Instant::now();
        let mut z = self.zustand.lock().unwrap();
        let mut weg = Vec::new();
        z.raeume.retain(|raum, m| {
            m.retain(|m| {
                let ok = jetzt.duration_since(m.zuletzt) < ZEITLIMIT;
                if !ok {
                    weg.push((m.sitzung, *raum, m.name.clone()));
                }
                ok
            });
            !m.is_empty()
        });
        let Zustand { raeume, metronome, aufnahmen, spuren, .. } = &mut *z;
        metronome.retain(|r, _| raeume.contains_key(r));
        spuren.retain(|r, _| raeume.contains_key(r));
        aufnahmen.retain(|r, _| {
            let bleibt = raeume.contains_key(r);
            if !bleibt {
                let _ = self.aufnehmer.send(aufnahme::Auftrag::Stop { raum: *r });
            }
            bleibt
        });
        for (s, raum, name) in weg {
            z.sitzungen.remove(&s);
            println!("Raum {}: „{}“ getrennt (Zeitüberschreitung)", kurz(&raum), name);
        }
    }
}

fn main() {
    let mut ports: Vec<u16> = STANDARD_PORTS.to_vec();
    let mut max = MAX_TEILNEHMER;
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--ports" if i + 1 < args.len() => {
                ports = args[i + 1].split(',').filter_map(|p| p.trim().parse().ok()).collect();
                i += 1;
            }
            "--max" if i + 1 < args.len() => {
                max = args[i + 1].parse().unwrap_or(MAX_TEILNEHMER).clamp(2, MAX_TEILNEHMER);
                i += 1;
            }
            _ => {
                eprintln!("Aufruf: jamcenter-relay [--ports 3478,443,47800] [--max 2]");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let mut socks = Vec::new();
    for &p in &ports {
        match sock_oeffnen(p) {
            Ok(s) => {
                println!("Lausche auf UDP {p}");
                socks.push(Arc::new(s));
            }
            Err(e) => eprintln!("UDP {p} nicht verfügbar: {e}"),
        }
    }
    if socks.is_empty() {
        eprintln!("Kein Port verfügbar");
        std::process::exit(1);
    }
    let aufnehmer = aufnahme::starten();
    let server = Arc::new(Server { socks, zustand: Mutex::new(Zustand::default()), geheim: RandomState::new(), max, beginn: Instant::now(), aufnehmer });
    for idx in 0..server.socks.len() {
        let s = server.clone();
        thread::Builder::new()
            .name(format!("udp{idx}"))
            .spawn(move || s.lauschen(idx))
            .expect("Thread");
    }
    loop {
        thread::sleep(Duration::from_secs(1));
        server.aufraeumen();
    }
}
