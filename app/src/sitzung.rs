//! Startet und beendet Netz-, Aufnahme- und Wiedergabe-Thread einer Verbindung.

use crate::asio;
use crate::audio::{self, Com};
use crate::gemeinsam::{Einstellungen, Geteilt};
use crate::netz::{self, Krypto, NetzDaten, Sender};
use crate::puffer::{Empfang, Wiedergabe, RING};
use std::net::UdpSocket;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

pub struct Sitzung {
    threads: Vec<JoinHandle<()>>,
    geteilt: Arc<Geteilt>,
}

pub fn regler_setzen(e: &Einstellungen, g: &Geteilt) {
    let r = &g.regler;
    r.eingang_db.set(e.eingang_db);
    r.lautstaerke_db.set(e.lautstaerke_db);
    r.redundanz.store(e.redundanz, Relaxed);
    r.schutz.store(e.schutz, Relaxed);
    r.puffer_auto.store(e.puffer_auto, Relaxed);
    r.puffer_ms.set(e.puffer_ms);
    r.metronom_db.set(e.metronom_db);
    r.spur_db.set(e.spur_db);
    r.stereo.store(e.stereo, Relaxed);
    r.metronom_klang.store(e.metronom_klang, Relaxed);
}

fn zufall_u64() -> u64 {
    let mut b = [0u8; 8];
    let _ = getrandom::getrandom(&mut b);
    u64::from_le_bytes(b) | 1
}

pub fn starten(e: &Einstellungen, g: Arc<Geteilt>) -> Result<Sitzung, String> {
    g.anzeige_zuruecksetzen();
    regler_setzen(e, &g);
    e.kanaele_anwenden(&g);
    let sock = Arc::new(UdpSocket::bind("0.0.0.0:0").map_err(|x| format!("Netzwerk: {x}"))?);
    let sitzung = zufall_u64();
    let krypto = Krypto::neu(&netz::schluessel(&e.raum, &e.passwort), sitzung);
    let (prod, cons) = rtrb::RingBuffer::new(RING);
    let mut threads = Vec::new();

    // weitere Tonquellen (Windows-Ton, Loopback, zweite Soundkarte)
    let quellen: Vec<crate::zusatz::Quelle> = crate::zusatz::aus_text(&e.zusatz).iter().filter_map(|s| crate::zusatz::beschreiben(s)).collect();
    *g.zusatz_namen.lock().unwrap() = crate::zusatz::kanal_namen(&quellen);
    let mut leser = Vec::new();
    for q in &quellen {
        match crate::zusatz::starten(q, g.clone(), e.exklusiv) {
            Ok((t, l)) => {
                threads.push(t);
                leser.push(l);
            }
            Err(x) => g.fehler(format!("Zusatzquelle {}: {x}", q.name)),
        }
    }

    let d = NetzDaten {
        sock: sock.clone(),
        krypto: krypto.clone(),
        geteilt: g.clone(),
        server: e.server.clone(),
        raum: netz::raum_kennung(&e.raum),
        raum_name: e.raum.trim().to_string(),
        name: e.name.trim().to_string(),
    };
    let gn = g.clone();
    threads.push(
        thread::Builder::new()
            .name("netz".into())
            .spawn(move || netz::laufen(d, Empfang::neu(prod, gn)))
            .map_err(|x| x.to_string())?,
    );

    if !e.treiber.is_empty() {
        // ASIO: ein Treiber, ein Callback für Ein- und Ausgang
        let info = asio::treiber_liste()
            .into_iter()
            .find(|t| t.name == e.treiber)
            .ok_or_else(|| format!("ASIO-Treiber „{}“ ist nicht (mehr) installiert", e.treiber))?;
        let auftrag = asio::Auftrag { treiber: info, eingang: e.asio_eingang, ausgang: e.asio_ausgang, block: e.asio_block };
        let mut sender = Sender::neu(sock, krypto, g.clone(), 48_000);
        sender.zusatz_setzen(leser);
        let wiedergabe = Wiedergabe::neu(cons, 48_000, g.clone());
        let ga = g.clone();
        threads.push(
            thread::Builder::new()
                .name("asio".into())
                .spawn(move || asio::laufen(auftrag, ga, sender, wiedergabe))
                .map_err(|x| x.to_string())?,
        );
        return Ok(Sitzung { threads, geteilt: g });
    }

    let (ausgang, exklusiv) = (e.ausgang.clone(), e.exklusiv);
    let gw = g.clone();
    threads.push(
        thread::Builder::new()
            .name("wiedergabe".into())
            .spawn(move || {
                let _com = Com::init();
                match audio::oeffnen(&ausgang, false, exklusiv) {
                    Ok(s) => {
                        gw.anzeige.aus_latenz_ms.set(s.latenz_ms);
                        *gw.anzeige.aus_info.lock().unwrap() = format!("{}: {}", s.name, s.beschreibung);
                        let mut w = Wiedergabe::neu(cons, s.format.rate, gw.clone());
                        if let Err(x) = s.wiedergabe_laufen(&gw.stop, |b| w.fuellen(b)) {
                            gw.fehler(format!("Lautsprecher: {x}"));
                        }
                    }
                    Err(x) => gw.fehler(format!("Lautsprecher: {x}")),
                }
            })
            .map_err(|x| x.to_string())?,
    );

    let eingang = e.eingang.clone();
    let ga = g.clone();
    threads.push(
        thread::Builder::new()
            .name("aufnahme".into())
            .spawn(move || {
                let _com = Com::init();
                match audio::oeffnen(&eingang, true, exklusiv) {
                    Ok(s) => {
                        ga.anzeige.ein_latenz_ms.set(s.latenz_ms);
                        *ga.anzeige.ein_info.lock().unwrap() = format!("{}: {}", s.name, s.beschreibung);
                        let k = s.format.kanaele.max(1);
                        let namen: Vec<String> = match k {
                            1 => vec!["Mono".into()],
                            2 => vec!["Links".into(), "Rechts".into()],
                            _ => (1..=k).map(|i| format!("Kanal {i}")).collect(),
                        };
                        ga.kanaele_geoeffnet(namen, None);
                        let mut sender = Sender::neu(sock, krypto, ga.clone(), s.format.rate);
                        sender.zusatz_setzen(leser);
                        if let Err(x) = s.aufnahme_laufen(&ga.stop, |b, k| sender.verarbeiten_kanaele(b, k)) {
                            ga.fehler(format!("Mikrofon: {x}"));
                        }
                    }
                    Err(x) => ga.fehler(format!("Mikrofon: {x}")),
                }
            })
            .map_err(|x| x.to_string())?,
    );
    Ok(Sitzung { threads, geteilt: g })
}

impl Sitzung {
    pub fn stoppen(self) {
        self.geteilt.stop.store(true, Relaxed);
        for t in self.threads {
            let _ = t.join();
        }
        self.geteilt.ziel.store(0, Relaxed);
    }
}
