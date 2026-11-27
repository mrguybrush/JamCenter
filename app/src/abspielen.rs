//! Aufnahmen direkt vom Server anhören (ohne Download): Die Datei wird per TCP gestreamt, in
//! einem eigenen Thread dekodiert (MP3/WAV, symphonia) und über einen Ring in die Wiedergabe
//! gemischt – während einer Verbindung in Jamcenters eigenen Ausgang (geht auch exklusiv und
//! nur lokal, der andere Raum hört nichts), sonst über einen eigenen geteilten WASAPI-Ausgang.

use crate::audio::{self, Com};
use crate::gemeinsam::{db_zu_faktor, Geteilt};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSource, MediaSourceStream, ReadOnlySource};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Stereo-Paare im Ring: 4 s bei 48 kHz
const RING: usize = 48_000 * 2 * 4;

/// Stand des Abspielens, von GUI und Threads gemeinsam genutzt
#[derive(Default)]
pub struct Stand {
    pub stop: AtomicBool,
    /// abgespielte Frames (Dateirate)
    pub gespielt: AtomicU64,
    pub rate: AtomicU32,
    /// Datei vollständig dekodiert
    pub dekodiert: AtomicBool,
    /// alles abgespielt
    pub fertig: AtomicBool,
    /// höchster ausgegebener Pegel seit dem letzten Abholen (f32-Bits, zum Prüfen)
    pub spitze: AtomicU32,
    pub fehler: Mutex<Option<String>>,
}

/// Leseseite für die Wiedergabe (liegt in `Geteilt::vorhoeren`)
pub struct Vorhoeren {
    cons: rtrb::Consumer<f32>,
    stand: Arc<Stand>,
    frac: f64,
    a: [f32; 2],
    b: [f32; 2],
    /// erst loslegen, wenn 0,3 s Vorrat da sind (sonst stottert der Anfang)
    laeuft: bool,
}

impl Vorhoeren {
    /// In einen Block Stereo-Paare (Geräterate `rate`) hineinmischen
    pub fn mischen(&mut self, aus: &mut [f32], rate: f64, g: &Geteilt) {
        let quelle = self.stand.rate.load(Relaxed) as f64;
        if quelle < 8000.0 || rate < 8000.0 {
            return;
        }
        if !self.laeuft {
            if (self.cons.slots() as f64) < quelle * 0.6 && !self.stand.dekodiert.load(Relaxed) {
                return;
            }
            self.laeuft = true;
        }
        let laut = db_zu_faktor(g.regler.lautstaerke_db.get());
        let schritt = quelle / rate;
        let mut weiter = 0u64;
        for p in aus.chunks_exact_mut(2) {
            let t = self.frac as f32;
            p[0] += (self.a[0] + (self.b[0] - self.a[0]) * t) * laut;
            p[1] += (self.a[1] + (self.b[1] - self.a[1]) * t) * laut;
            self.frac += schritt;
            while self.frac >= 1.0 {
                self.frac -= 1.0;
                self.a = self.b;
                if self.cons.slots() >= 2 {
                    if let Ok(c) = self.cons.read_chunk(2) {
                        let (x, _) = c.as_slices();
                        self.b = [x[0], x[1]];
                        c.commit_all();
                    }
                    weiter += 1;
                } else {
                    self.b = [0.0; 2];
                    if self.stand.dekodiert.load(Relaxed) {
                        self.stand.fertig.store(true, Relaxed);
                    }
                }
            }
        }
        self.stand.gespielt.fetch_add(weiter, Relaxed);
        let s = aus.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
        if s > f32::from_bits(self.stand.spitze.load(Relaxed)) {
            self.stand.spitze.store(s.to_bits(), Relaxed);
        }
    }
}

pub struct Spieler {
    pub stand: Arc<Stand>,
    threads: Vec<JoinHandle<()>>,
    geteilt: Arc<Geteilt>,
}

impl Spieler {
    /// Abspielen starten. `eigener_ausgang`: Some(Geräte-ID), wenn keine Verbindung läuft
    /// (dann öffnet der Spieler den Ausgang selbst, geteilt), sonst mischt die laufende Wiedergabe.
    pub fn starten(host: String, kennung: [u8; 32], datei: String, geteilt: Arc<Geteilt>, eigener_ausgang: Option<String>) -> Spieler {
        let stand = Arc::new(Stand::default());
        let mut threads = Vec::new();
        let (st, g) = (stand.clone(), geteilt.clone());
        threads.push(thread::spawn(move || {
            if let Err(e) = dekodieren(&host, &kennung, &datei, &st, &g) {
                *st.fehler.lock().unwrap() = Some(e);
            }
            st.dekodiert.store(true, Relaxed);
        }));
        if let Some(id) = eigener_ausgang {
            let (st, g) = (stand.clone(), geteilt.clone());
            threads.push(thread::spawn(move || {
                let _com = Com::init();
                match audio::oeffnen(&id, false, false) {
                    Ok(s) => {
                        let rate = s.format.rate as f64;
                        let r = s.wiedergabe_laufen(&st.stop, |b| {
                            b.fill(0.0);
                            if let Ok(mut v) = g.vorhoeren.try_lock() {
                                if let Some(v) = v.as_mut() {
                                    v.mischen(b, rate, &g);
                                }
                            }
                        });
                        if let Err(e) = r {
                            *st.fehler.lock().unwrap() = Some(format!("Lautsprecher: {e}"));
                        }
                    }
                    Err(e) => *st.fehler.lock().unwrap() = Some(format!("Lautsprecher: {e}")),
                }
            }));
        }
        Spieler { stand, threads, geteilt }
    }

    /// Position und (geschätzte) Länge in Sekunden
    pub fn position(&self) -> f64 {
        let r = self.stand.rate.load(Relaxed).max(1) as f64;
        self.stand.gespielt.load(Relaxed) as f64 / r
    }

    pub fn stoppen(self) {
        self.stand.stop.store(true, Relaxed);
        *self.geteilt.vorhoeren.lock().unwrap() = None;
        for t in self.threads {
            let _ = t.join();
        }
        *self.geteilt.vorhoeren.lock().unwrap() = None;
    }
}

/// Prüfwerkzeug: `--anhoeren <server> <raum> <datei> [passwort]` spielt eine Aufnahme über den
/// Standardausgang und meldet je Sekunde Position und Pegel
pub fn probe(args: &[String]) {
    let server = args.first().cloned().unwrap_or_default();
    let raum = args.get(1).cloned().unwrap_or_default();
    let datei = args.get(2).cloned().unwrap_or_default();
    let passwort = args.get(3).cloned().unwrap_or_default();
    let g = Arc::new(Geteilt::neu());
    g.anzeige_zuruecksetzen();
    let kennung = crate::netz::aufnahme_kennung(&raum, &passwort);
    let sp = Spieler::starten(crate::aufnahmen::host(&server), kennung, datei, g.clone(), Some(String::new()));
    for s in 0..600 {
        thread::sleep(Duration::from_secs(1));
        let spitze = f32::from_bits(sp.stand.spitze.swap(0, Relaxed));
        println!("[{s}s] Position {:.2} s, Spitze {spitze:.4}, Rate {}", sp.position(), sp.stand.rate.load(Relaxed));
        if let Some(f) = sp.stand.fehler.lock().unwrap().clone() {
            println!("Fehler: {f}");
            break;
        }
        if sp.stand.fertig.load(Relaxed) {
            println!("fertig");
            break;
        }
    }
    sp.stoppen();
}

/// Datei dekodieren und die Stereo-Paare (f32) blockweise an `ziel(rate, paare)` geben;
/// `ziel` liefert false = abbrechen
fn dekodieren_mit(quelle: Box<dyn MediaSource>, endung: &str, mut ziel: impl FnMut(u32, &[f32]) -> bool) -> Result<(), String> {
    let mss = MediaSourceStream::new(quelle, Default::default());
    let mut hint = Hint::new();
    hint.with_extension(endung);
    let probe = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| format!("Format nicht lesbar: {e}"))?;
    let mut format = probe.format;
    let spur = format.default_track().ok_or("keine Tonspur")?;
    let spur_id = spur.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&spur.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("Codec: {e}"))?;
    let mut puffer: Option<SampleBuffer<f32>> = None;
    let mut paare: Vec<f32> = Vec::new();
    loop {
        let paket = match format.next_packet() {
            Ok(p) => p,
            // Ende des Datenstroms
            Err(symphonia::core::errors::Error::IoError(_)) => return Ok(()),
            Err(e) => return Err(format!("Lesen: {e}")),
        };
        if paket.track_id() != spur_id {
            continue;
        }
        let ton = match decoder.decode(&paket) {
            Ok(t) => t,
            // einzelnes kaputtes Paket überspringen
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
            Err(e) => return Err(format!("Dekodieren: {e}")),
        };
        let spec = *ton.spec();
        let kanaele = spec.channels.count().max(1);
        if puffer.as_ref().is_none_or(|p| p.capacity() < ton.capacity() * kanaele) {
            puffer = Some(SampleBuffer::new(ton.capacity() as u64, spec));
        }
        let p = puffer.as_mut().unwrap();
        p.copy_interleaved_ref(ton);
        paare.clear();
        for f in p.samples().chunks_exact(kanaele) {
            let l = f[0];
            paare.push(l);
            paare.push(if kanaele > 1 { f[1] } else { l });
        }
        if !ziel(spec.rate, &paare) {
            return Ok(());
        }
    }
}

fn endung(datei: &str) -> String {
    datei.rsplit('.').next().unwrap_or("").to_ascii_lowercase()
}

/// Prüfwerkzeug: `--hochladen <server> <raum> <datei> [passwort]`
pub fn hochladen_probe(args: &[String]) {
    let server = args.first().cloned().unwrap_or_default();
    let raum = args.get(1).cloned().unwrap_or_default();
    let pfad = std::path::PathBuf::from(args.get(2).cloned().unwrap_or_default());
    let passwort = args.get(3).cloned().unwrap_or_default();
    let kennung = crate::netz::aufnahme_kennung(&raum, &passwort);
    match hochladen_vorbereiten(&pfad).and_then(|(datei, daten)| crate::aufnahmen::hochladen(&crate::aufnahmen::host(&server), &kennung, &datei, &daten).map(|_| (datei, daten.len()))) {
        Ok((datei, n)) => println!("hochgeladen: {datei} ({n} Bytes)"),
        Err(e) => println!("Fehler: {e}"),
    }
}

fn dekodieren(host: &str, kennung: &[u8; 32], datei: &str, st: &Arc<Stand>, g: &Geteilt) -> Result<(), String> {
    let quelle = crate::aufnahmen::strom(host, kennung, datei)?;
    let (mut prod, cons) = rtrb::RingBuffer::<f32>::new(RING);
    let mut cons = Some(cons);
    dekodieren_mit(Box::new(ReadOnlySource::new(quelle)), &endung(datei), |rate, paare| {
        if let Some(c) = cons.take() {
            st.rate.store(rate, Relaxed);
            *g.vorhoeren.lock().unwrap() = Some(Vorhoeren { cons: c, stand: st.clone(), frac: 0.0, a: [0.0; 2], b: [0.0; 2], laeuft: false });
        }
        let mut rest = paare;
        while !rest.is_empty() {
            if st.stop.load(Relaxed) {
                return false;
            }
            let frei = prod.slots() & !1;
            if frei == 0 {
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            let n = frei.min(rest.len());
            if let Ok(mut c) = prod.write_chunk(n) {
                let (a, b) = c.as_mut_slices();
                a.copy_from_slice(&rest[..a.len()]);
                b.copy_from_slice(&rest[a.len()..n]);
                c.commit_all();
            }
            rest = &rest[n..];
        }
        !st.stop.load(Relaxed)
    })
}

fn pcm16(x: f32) -> i16 {
    (x.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

/// Spur des Raums vom Server holen und vollständig dekodieren (Stereo-Paare PCM16)
pub fn spur_laden(host: &str, kennung: &[u8; 32], datei: &str) -> Result<(u32, Vec<i16>), String> {
    let quelle = crate::aufnahmen::strom(host, kennung, datei)?;
    let mut rate = 0;
    let mut daten: Vec<i16> = Vec::new();
    dekodieren_mit(Box::new(ReadOnlySource::new(quelle)), &endung(datei), |r, paare| {
        rate = r;
        daten.extend(paare.iter().map(|&x| pcm16(x)));
        // höchstens 30 Minuten
        daten.len() < 48_000 * 2 * 1800
    })?;
    if rate == 0 || daten.is_empty() {
        return Err("Datei enthält keinen Ton".into());
    }
    Ok((rate, daten))
}

/// Datei für das Hochladen vorbereiten: MP3 bleibt, alles andere (WAV, FLAC, OGG, M4A …) wird zu
/// einer 16-Bit-WAV (so kann der Server sie in Aufnahmen mischen). Liefert (Dateiname, Inhalt).
pub fn hochladen_vorbereiten(pfad: &std::path::Path) -> Result<(String, Vec<u8>), String> {
    let stamm = pfad.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut name: String = stamm
        .chars()
        .map(|c| match c {
            'ä' => 'a',
            'ö' => 'o',
            'ü' => 'u',
            'Ä' => 'A',
            'Ö' => 'O',
            'Ü' => 'U',
            'ß' => 's',
            ' ' => '_',
            c => c,
        })
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(32)
        .collect();
    while name.starts_with(['-', '_']) {
        name.remove(0);
    }
    if name.is_empty() {
        name = "Spur".into();
    }
    let e = endung(&pfad.to_string_lossy());
    if e == "mp3" {
        let daten = std::fs::read(pfad).map_err(|x| x.to_string())?;
        return Ok((format!("{name}.mp3"), daten));
    }
    let datei = std::fs::File::open(pfad).map_err(|x| x.to_string())?;
    let mut rate = 0u32;
    let mut proben: Vec<u8> = Vec::new();
    dekodieren_mit(Box::new(datei), &e, |r, paare| {
        rate = r;
        for &x in paare {
            proben.extend_from_slice(&pcm16(x).to_le_bytes());
        }
        proben.len() < 290_000_000
    })?;
    if rate == 0 || proben.is_empty() {
        return Err("Datei enthält keinen Ton".into());
    }
    let mut w = Vec::with_capacity(proben.len() + 44);
    let n = proben.len() as u32;
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + n).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&(rate * 4).to_le_bytes());
    w.extend_from_slice(&4u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&n.to_le_bytes());
    w.extend_from_slice(&proben);
    Ok((format!("{name}.wav"), w))
}
