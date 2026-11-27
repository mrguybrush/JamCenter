//! Kopfloser Testlauf ohne Soundkarte (Netz, Verschlüsselung, Puffer, Wegwahl):
//! `jamcenter --test <raum> <name> [server] [sekunden] [passwort]`
//! Erzeugt einen Sinuston in 128er-Blöcken und „spielt“ das Empfangene im selben Takt ab.

use crate::gemeinsam::Geteilt;
use crate::netz::{self, Krypto, NetzDaten, Sender};
use crate::puffer::{Empfang, Wiedergabe, RING};
use std::net::UdpSocket;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

pub fn laufen(args: &[String]) {
    let raum = args.first().cloned().unwrap_or_else(|| "test".into());
    let name = args.get(1).cloned().unwrap_or_else(|| "Test".into());
    let server = args.get(2).cloned().unwrap_or_else(|| protokoll::STANDARD_SERVER.into());
    if server.trim().is_empty() {
        println!("Kein Server angegeben: --test <raum> <name> <server[:port]> [sek] [passwort]");
        return;
    }
    let dauer: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(20);
    let passwort = args.get(4).cloned().unwrap_or_default();
    let g = Arc::new(Geteilt::neu());
    g.anzeige_zuruecksetzen();
    g.anzeige.ein_latenz_ms.set(2.7);
    g.anzeige.aus_latenz_ms.set(5.3);
    g.kanaele_geoeffnet(vec!["Testton".into()], None);
    if std::env::var("KB_STEREO").is_ok() {
        g.regler.stereo.store(true, Relaxed);
        g.regler.kanal_pan[0].set(-0.5);
    }
    // KB_NUR_KLICK: eigenen Ton stumm, damit im Ausgang nur das Metronom liegt
    if std::env::var("KB_NUR_KLICK").is_ok() {
        g.regler.stumm.store(true, Relaxed);
        g.regler.metronom_db.set(0.0);
    }
    let abweichungen = Arc::new(std::sync::Mutex::new(Vec::<f64>::new()));
    let sock = Arc::new(UdpSocket::bind("0.0.0.0:0").expect("bind"));
    let sitzung = {
        let mut b = [0u8; 8];
        getrandom::getrandom(&mut b).unwrap();
        u64::from_le_bytes(b) | 1
    };
    let krypto = Krypto::neu(&netz::schluessel(&raum, &passwort), sitzung);
    let (prod, cons) = rtrb::RingBuffer::new(RING);
    let g_server = server.clone();
    let d = NetzDaten {
        sock: sock.clone(),
        krypto: krypto.clone(),
        geteilt: g.clone(),
        server,
        raum: netz::raum_kennung(&raum),
        raum_name: raum.clone(),
        name: name.clone(),
    };
    let gn = g.clone();
    let t_netz = thread::spawn(move || netz::laufen(d, Empfang::neu(prod, gn)));

    const RATE: u32 = 48_000;
    const BLOCK: usize = 128;
    let periode = Duration::from_secs_f64(BLOCK as f64 / RATE as f64);
    let freq = 300.0 + (name.len() as f64 * 97.0) % 500.0;
    let ga = g.clone();
    let t_auf = thread::spawn(move || {
        let _mm = crate::audio::Mmcss::neu();
        // KB_EINGANG=<Geräte-ID> (+ KB_EXKLUSIV=1): echtes Mikrofon statt Testton
        if let Ok(id) = std::env::var("KB_EINGANG") {
            let _com = crate::audio::Com::init();
            match crate::audio::oeffnen(&id, true, std::env::var("KB_EXKLUSIV").is_ok()) {
                Ok(st) => {
                    println!("Eingang: {} {}", st.name, st.beschreibung);
                    ga.anzeige.ein_latenz_ms.set(st.latenz_ms);
                    let mut s = Sender::neu(sock, krypto, ga.clone(), st.format.rate);
                    let _ = st.aufnahme_laufen(&ga.stop, |b, k| s.verarbeiten_kanaele(b, k));
                }
                Err(e) => println!("Eingang: {e}"),
            }
            return;
        }
        let mut s = Sender::neu(sock, krypto, ga.clone(), RATE);
        let mut phase = 0.0f64;
        let mut block = [0.0f32; BLOCK];
        let mut naechste = Instant::now();
        while !ga.stop.load(Relaxed) {
            for x in block.iter_mut() {
                phase += freq / RATE as f64;
                *x = ((phase * std::f64::consts::TAU).sin() * 0.3) as f32;
            }
            s.verarbeiten(&block);
            naechste += periode;
            let jetzt = Instant::now();
            if naechste > jetzt {
                thread::sleep(naechste - jetzt);
            }
        }
    });
    let gw = g.clone();
    let ab = abweichungen.clone();
    let pegel_lr = Arc::new(std::sync::Mutex::new([0.0f32; 2]));
    let pegel_lr_anzeige = pegel_lr.clone();
    let pegel_lr = pegel_lr.clone();
    let t_wie = thread::spawn(move || {
        let _mm = crate::audio::Mmcss::neu();
        let mut w = Wiedergabe::neu(cons, RATE, gw.clone());
        let mut block = [0.0f32; BLOCK * 2];
        let mut naechste = Instant::now();
        let mut zuvor_still = true;
        let mut seiten = [0.0f32; 2];
        while !gw.stop.load(Relaxed) {
            let t_block = crate::gemeinsam::uhr_us() as f64;
            w.fuellen(&mut block);
            // Klickanfänge gegen das Serverraster prüfen (Hörzeit = Samplezeit + Ausgabelatenz)
            let m = &gw.metronom;
            if m.an.load(Relaxed) {
                for (i, &x) in block.iter().step_by(2).enumerate() {
                    if x.abs() > 0.001 && zuvor_still {
                        let t = t_block + i as f64 * 1e6 / RATE as f64 + gw.uhr_versatz.load(Relaxed) as f64 + gw.anzeige.aus_latenz_ms.get() as f64 * 1000.0;
                        let schlag = 60e6 / m.bpm.get() as f64;
                        let mut d = (t - m.start_us.load(Relaxed) as f64) % schlag;
                        if d > schlag / 2.0 {
                            d -= schlag;
                        }
                        ab.lock().unwrap().push(d / 1000.0);
                    }
                    zuvor_still = x.abs() <= 1e-7;
                }
            }
            for p in block.chunks_exact(2) {
                seiten[0] = seiten[0].max(p[0].abs());
                seiten[1] = seiten[1].max(p[1].abs());
            }
            *pegel_lr.lock().unwrap() = seiten;
            naechste += periode;
            let jetzt = Instant::now();
            if naechste > jetzt {
                thread::sleep(naechste - jetzt);
            }
        }
    });

    let start = Instant::now();
    let metronom_wunsch = std::env::var("KB_METRONOM").ok();
    while start.elapsed() < Duration::from_secs(dauer) {
        thread::sleep(Duration::from_secs(1));
        if start.elapsed().as_secs() == 3 {
            // KB_METRONOM="bpm,takt" (oder "aus"): Metronom für den ganzen Raum umstellen
            if let Some(w) = &metronom_wunsch {
                let mut t = w.split(',');
                let bpm = t.next().and_then(|x| x.parse().ok());
                let takt = t.next().and_then(|x| x.parse().ok()).unwrap_or(4);
                *g.metronom_wunsch.lock().unwrap() = Some((bpm.is_some(), bpm.unwrap_or(100.0), takt));
            }
        }
        // KB_AUFNAHME=1: Aufnahme nach 3 s starten, 2 s vor Schluss stoppen, danach Liste zeigen
        if std::env::var("KB_AUFNAHME").is_ok() {
            let s = start.elapsed().as_secs();
            let kennung = netz::aufnahme_kennung(&raum, &passwort);
            if s == 3 {
                *g.aufnahme_wunsch.lock().unwrap() = Some((true, kennung, "2026_12_01_1538".into(), Some(0)));
            } else if s + 2 == dauer {
                *g.aufnahme_wunsch.lock().unwrap() = Some((false, kennung, String::new(), None));
            }
            println!("        Aufnahme {} {}", if g.aufnahme_an.load(Relaxed) { "läuft" } else { "aus" }, g.aufnahme_name.lock().unwrap());
        }
        let m = &g.metronom;
        let ab = abweichungen.lock().unwrap();
        let (n, mittel, max) = (ab.len(), ab.iter().sum::<f64>() / ab.len().max(1) as f64, ab.iter().fold(0.0f64, |a, &b| a.max(b.abs())));
        println!(
            "        Uhr-Versatz {} µs ({}) | Metronom {} {} bpm {}er Stand {} | Klicks {} Abweichung Ø {:.3} ms, max {:.3} ms",
            g.uhr_versatz.load(Relaxed),
            if g.uhr_ok.load(Relaxed) { "ok" } else { "–" },
            if m.an.load(Relaxed) { "an" } else { "aus" },
            m.bpm.get(),
            m.takt.load(Relaxed),
            m.stand.load(Relaxed),
            n,
            mittel,
            max
        );
        drop(ab);
        let lr = *pegel_lr_anzeige.lock().unwrap();
        println!("        Empfangspegel links {:.3}, rechts {:.3}", lr[0], lr[1]);
        let a = &g.anzeige;
        let weg = ["–", "Server", "direkt"][a.weg.load(Relaxed) as usize];
        println!(
            "[{:>3}s] {} | Weg {} | RTT Server {:.1} ms, direkt {:.1} ms | Puffer {:.1}/{:.1} ms | Aussetzer {} | Verlust {:.2} % | Gegenstelle Puffer {:.1} ms{}",
            start.elapsed().as_secs(),
            a.status.lock().unwrap(),
            weg,
            a.rtt_relay_ms.get(),
            a.rtt_direkt_ms.get(),
            a.puffer_ist_ms.get(),
            a.puffer_ziel_ms.get(),
            a.unterlaeufe.load(Relaxed),
            a.verlust_prozent.get(),
            a.partner_puffer_ms.get(),
            a.fehler.lock().unwrap().as_ref().map(|f| format!(" | FEHLER {f}")).unwrap_or_default(),
        );
    }
    if std::env::var("KB_AUFNAHME").is_ok() {
        let host = crate::aufnahmen::host(&g_server);
        for _ in 0..10 {
            thread::sleep(Duration::from_secs(1));
            match crate::aufnahmen::liste(&host, &netz::aufnahme_kennung(&raum, &passwort)) {
                Ok(l) => {
                    for e in &l {
                        println!("Aufnahme auf dem Server: {} {} Bytes {}", e.datei, e.groesse, if e.fertig { "fertig" } else { "wird umgewandelt" });
                    }
                    if !l.is_empty() && l.iter().all(|e| e.fertig) {
                        let ziel = std::env::temp_dir().join(&l[0].datei);
                        println!("Download: {:?} -> {}", crate::aufnahmen::holen(&host, &netz::aufnahme_kennung(&raum, &passwort), &l[0].datei, &ziel), ziel.display());
                        break;
                    }
                }
                Err(e) => println!("Liste: {e}"),
            }
        }
    }
    g.stop.store(true, Relaxed);
    let _ = t_auf.join();
    let _ = t_wie.join();
    let _ = t_netz.join();
}
