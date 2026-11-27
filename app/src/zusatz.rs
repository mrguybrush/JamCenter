//! Weitere Tonquellen neben dem Haupteingang, gleichzeitig von anderen Soundkarten:
//!
//! - „system“: Windows-Ton ohne Jamcenter (Prozess-Loopback, ab Windows 10 2004). Alles, was
//!   Sequencer, Browser, Player … über Windows abspielen – aber nicht der Ton der Gegenstelle und
//!   nicht das Metronom, sonst hörte der andere Raum sich selbst als Echo.
//! - „aus:<Geräte-ID>“: alles, was auf einem bestimmten Ausgang läuft (Loopback des Geräts).
//! - „ein:<Geräte-ID>“: ein weiteres Aufnahmegerät (zweite Soundkarte, Webcam …).
//!
//! Jede Quelle läuft in einem eigenen Thread mit eigener Uhr. Der Sender holt bei jedem Block
//! passend viele Proben ab und gleicht Taktunterschiede mit einem geregelten Resampler aus
//! (wie der Empfangspuffer). Die Quellen erscheinen als zusätzliche Kanäle im Mischpult.

use crate::audio::{self, Com};
use crate::gemeinsam::Geteilt;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

/// höchstens so viele Kanäle je Quelle
const MAX_KAN: usize = 32;

#[derive(Clone, Debug)]
pub struct Quelle {
    pub schluessel: String,
    /// Anzeigename im Mischpult (kurz)
    pub name: String,
    pub kanaele: usize,
}

/// Gespeicherte Liste „system;aus:…;ein:…“
pub fn aus_text(s: &str) -> Vec<String> {
    s.split(';').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
}

pub fn zu_text(v: &[String]) -> String {
    v.join(";")
}

fn kurz(name: &str) -> String {
    // „Lautsprecher (Realtek(R) Audio)“ -> „Lautsprecher“
    let n = name.split(" (").next().unwrap_or(name).trim();
    let n: String = n.chars().take(14).collect();
    if n.is_empty() { "Quelle".into() } else { n }
}

/// Alle wählbaren Quellen: (Schlüssel, Text für die Auswahl)
pub fn auswahl(haupt_eingang: &str) -> Vec<(String, String)> {
    let mut v = vec![("system".to_string(), "Windows-Ton (alles außer Jamcenter) – empfohlen für Sequencer und Player".to_string())];
    for g in audio::geraete(false) {
        v.push((format!("aus:{}", g.id), format!("Ausgang mithören: {}", g.name)));
    }
    for g in audio::geraete(true) {
        if g.id != haupt_eingang {
            v.push((format!("ein:{}", g.id), format!("Weiterer Eingang: {}", g.name)));
        }
    }
    v
}

/// Quelle aus dem Schlüssel (Name und Kanalzahl für das Mischpult)
pub fn beschreiben(schluessel: &str) -> Option<Quelle> {
    let (name, kanaele) = if schluessel == "system" {
        ("Windows".to_string(), 2)
    } else if let Some(id) = schluessel.strip_prefix("aus:") {
        let g = audio::geraete(false).into_iter().find(|g| g.id == id)?;
        (kurz(&g.name), 2)
    } else if let Some(id) = schluessel.strip_prefix("ein:") {
        let g = audio::geraete(true).into_iter().find(|g| g.id == id)?;
        (kurz(&g.name), audio::kanalzahl(id).clamp(1, MAX_KAN))
    } else {
        return None;
    };
    Some(Quelle { schluessel: schluessel.to_string(), name, kanaele })
}

/// Kanalnamen und Vorgabe-Panorama der Quellen (Stereo = links/rechts)
pub fn kanal_namen(quellen: &[Quelle]) -> Vec<(String, f32)> {
    let mut v = Vec::new();
    for q in quellen {
        match q.kanaele {
            1 => v.push((q.name.clone(), 0.0)),
            2 => {
                v.push((format!("{} L", q.name), -1.0));
                v.push((format!("{} R", q.name), 1.0));
            }
            k => {
                for i in 0..k {
                    v.push((format!("{} {}", q.name, i + 1), 0.0));
                }
            }
        }
    }
    v
}

/// Gemeinsamer Zustand Quelle -> Leser
struct Takt {
    rate: AtomicU32,
    periode: AtomicU32,
}

/// Läuft im Sender: holt Proben einer Quelle im Takt des Haupteingangs
pub struct Leser {
    pub kanaele: usize,
    cons: rtrb::Consumer<f32>,
    takt: Arc<Takt>,
    spielt: bool,
    frac: f64,
    integral: f64,
    /// letzte 4 Frames je Kanal (Hermite)
    h: [[f32; 4]; MAX_KAN],
}

fn hermite(h: &[f32; 4], t: f32) -> f32 {
    let (y0, y1, y2, y3) = (h[0], h[1], h[2], h[3]);
    let c1 = 0.5 * (y2 - y0);
    let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
    let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
    ((c3 * t + c2) * t + c1) * t + y1
}

impl Leser {
    fn frame_holen(&mut self) -> bool {
        let k = self.kanaele;
        if self.cons.slots() < k {
            return false;
        }
        for c in 0..k {
            let x = self.cons.pop().unwrap_or(0.0);
            let h = &mut self.h[c];
            h.copy_within(1.., 0);
            h[3] = x;
        }
        true
    }

    /// Füllstand des Rings in ms (= zusätzliche Verzögerung dieser Quelle)
    pub fn fuell_ms(&self) -> f64 {
        let rs = self.takt.rate.load(Relaxed).max(1) as f64;
        (self.cons.slots() / self.kanaele) as f64 * 1000.0 / rs
    }

    /// `n` Proben je Kanal bei `rate` (Rate des Haupteingangs) in `ziel[c]` legen
    pub fn lesen(&mut self, n: usize, rate: u32, ziel: &mut [Vec<f32>]) {
        let k = self.kanaele;
        for z in ziel.iter_mut().take(k) {
            z.clear();
            z.resize(n, 0.0);
        }
        let rs = self.takt.rate.load(Relaxed) as f64;
        if rs < 8000.0 || n == 0 {
            return;
        }
        let fuell = (self.cons.slots() / k) as f64;
        // Zielfüllung so knapp wie möglich: eine Periode der Quelle (kommt am Stück) + ein Block des
        // Haupteingangs (wird am Stück entnommen) + 2 ms Reserve für Planungsschwankungen
        let ziel_fr = self.takt.periode.load(Relaxed) as f64 + n as f64 * rs / rate as f64 + rs * 0.002;
        if !self.spielt {
            if fuell < ziel_fr {
                return;
            }
            self.spielt = true;
            self.integral = 0.0;
            self.frac = 0.0;
        }
        // viel zu voll (Quelle lief allein weiter): auf das Ziel kürzen
        if fuell > ziel_fr + rs * 0.12 {
            let weg = (fuell - ziel_fr) as usize * k;
            if let Ok(c) = self.cons.read_chunk(weg.min(self.cons.slots())) {
                c.commit_all();
            }
        }
        let fuell = (self.cons.slots() / k) as f64;
        let fehler_ms = (fuell - ziel_fr) / rs * 1000.0;
        let dt = n as f64 / rate as f64;
        self.integral = (self.integral + fehler_ms * dt).clamp(-50.0, 50.0);
        let korr = (0.0005 * fehler_ms + 0.0001 * self.integral).clamp(-0.005, 0.005);
        let schritt = rs / rate as f64 * (1.0 + korr);
        for i in 0..n {
            for c in 0..k {
                ziel[c][i] = hermite(&self.h[c], self.frac as f32);
            }
            self.frac += schritt;
            while self.frac >= 1.0 {
                if !self.frame_holen() {
                    // leer gelaufen: Stille, bis wieder genug da ist (Loopback liefert nichts, solange nichts spielt)
                    self.spielt = false;
                    self.h = [[0.0; 4]; MAX_KAN];
                    return;
                }
                self.frac -= 1.0;
            }
        }
    }
}

/// Quelle starten: Thread nimmt auf und schreibt in den Ring, der Leser geht an den Sender
/// `exklusiv`: weitere Eingänge wie der Haupteingang exklusiv mit kleinen Blöcken öffnen
/// (fällt selbst auf IAudioClient3/geteilt zurück)
pub fn starten(q: &Quelle, g: Arc<Geteilt>, exklusiv: bool) -> Result<(JoinHandle<()>, Leser), String> {
    let k = q.kanaele.clamp(1, MAX_KAN);
    let (mut prod, cons) = rtrb::RingBuffer::<f32>::new(48_000 * k);
    let takt = Arc::new(Takt { rate: AtomicU32::new(0), periode: AtomicU32::new(0) });
    let t = takt.clone();
    let schluessel = q.schluessel.clone();
    let name = q.name.clone();
    let h = thread::Builder::new()
        .name("zusatz".into())
        .spawn(move || {
            let _com = Com::init();
            let strom = if schluessel == "system" {
                audio::oeffnen_system_ohne_mich()
            } else if let Some(id) = schluessel.strip_prefix("aus:") {
                audio::oeffnen_loopback(id)
            } else if let Some(id) = schluessel.strip_prefix("ein:") {
                audio::oeffnen(id, true, exklusiv)
            } else {
                return;
            };
            let s = match strom {
                Ok(s) => s,
                Err(e) => {
                    g.fehler(format!("Zusatzquelle {name}: {e}"));
                    return;
                }
            };
            t.rate.store(s.format.rate, Relaxed);
            t.periode.store(s.periode, Relaxed);
            {
                let mut info = g.anzeige.ein_info.lock().unwrap();
                info.push_str(&format!(" · + {name}: {}", s.beschreibung));
            }
            let r = s.aufnahme_laufen(&g.stop, |daten, kd| {
                let kd = kd.max(1);
                // nur so viele Kanäle wie angemeldet (Loopback 5.1/7.1 -> vorn links/rechts)
                if prod.slots() < daten.len() / kd * k {
                    return; // Leser hängt (z. B. Haupteingang steht): verwerfen statt blockieren
                }
                for f in daten.chunks_exact(kd) {
                    for c in 0..k {
                        let _ = prod.push(f.get(c).copied().unwrap_or(if kd == 1 { f[0] } else { 0.0 }));
                    }
                }
            });
            if let Err(e) = r {
                g.fehler(format!("Zusatzquelle {name}: {e}"));
            }
        })
        .map_err(|e| e.to_string())?;
    Ok((h, Leser { kanaele: k, cons, takt, spielt: false, frac: 0.0, integral: 0.0, h: [[0.0; 4]; MAX_KAN] }))
}

/// Kommandozeile: Quelle öffnen und je Sekunde Spitzenpegel und Rate ausgeben
pub fn probe(schluessel: &str, sek: u64) {
    let _com = Com::init();
    if schluessel == "liste" {
        for (k, t) in auswahl("") {
            println!("{k}	{t}");
        }
        return;
    }
    let Some(q) = beschreiben(schluessel) else {
        println!("unbekannte Quelle {schluessel}");
        return;
    };
    let g = Arc::new(Geteilt::neu());
    g.anzeige_zuruecksetzen();
    let (t, mut l) = match starten(&q, g.clone(), true) {
        Ok(x) => x,
        Err(e) => {
            println!("Fehler: {e}");
            return;
        }
    };
    let mut ziel: Vec<Vec<f32>> = (0..MAX_KAN).map(|_| Vec::new()).collect();
    let mut naechste = std::time::Instant::now();
    for s in 0..sek {
        let mut spitze = 0.0f32;
        let (mut fmin, mut fmax, mut fsum) = (f64::MAX, 0.0f64, 0.0);
        for _ in 0..375 {
            // genau im 128er-Takt wie ein Haupteingang (sleep allein schläft zu lange)
            naechste += std::time::Duration::from_micros(2667);
            let jetzt = std::time::Instant::now();
            if naechste > jetzt {
                std::thread::sleep(naechste - jetzt);
            }
            l.lesen(128, 48_000, &mut ziel);
            let f = l.fuell_ms();
            fmin = fmin.min(f);
            fmax = fmax.max(f);
            fsum += f;
            for z in ziel.iter().take(l.kanaele) {
                spitze = z.iter().fold(spitze, |a, &v| a.max(v.abs()));
            }
        }
        println!("[{s}s] {} | Rate {} | Spitze {:.4} | Vorrat Ø {:.1} ms (min {:.1}, max {:.1}) | {}", q.name, l.takt.rate.load(Relaxed), spitze, fsum / 375.0, fmin, fmax, g.anzeige.fehler.lock().unwrap().clone().unwrap_or_default());
    }
    println!("{}", g.anzeige.ein_info.lock().unwrap());
    g.stop.store(true, Relaxed);
    let _ = t.join();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Quelle 44,1 kHz in unregelmäßigen 10-ms-Paketen, Haupteingang 48 kHz in 128er-Blöcken:
    /// Füllstand bleibt stabil, kein Leerlaufen, Sinus kommt sauber an
    #[test]
    fn taktausgleich() {
        let (mut prod, cons) = rtrb::RingBuffer::<f32>::new(48_000 * 2);
        let takt = Arc::new(Takt { rate: AtomicU32::new(44_100), periode: AtomicU32::new(441) });
        let mut l = Leser { kanaele: 2, cons, takt, spielt: false, frac: 0.0, integral: 0.0, h: [[0.0; 4]; MAX_KAN] };
        let mut ziel = vec![Vec::new(), Vec::new()];
        let mut phase = 0.0f64;
        let (mut t_quelle, mut t_haupt) = (0.0f64, 0.0f64);
        let mut leer = 0;
        let mut spitze_fehler = 0.0f32;
        let mut ausgang = Vec::new();
        // Quelle läuft 0,02 % schneller (eigene Uhr)
        while t_haupt < 20.0 {
            while t_quelle <= t_haupt {
                for _ in 0..441 {
                    phase += 1000.0 / 44_100.0 * 1.0002;
                    let x = (phase * std::f64::consts::TAU).sin() as f32 * 0.5;
                    let _ = prod.push(x);
                    let _ = prod.push(-x);
                }
                t_quelle += 0.01;
            }
            l.lesen(128, 48_000, &mut ziel);
            if t_haupt > 1.0 {
                if !l.spielt {
                    leer += 1;
                }
                for i in 0..128 {
                    spitze_fehler = spitze_fehler.max((ziel[0][i] + ziel[1][i]).abs());
                    ausgang.push(ziel[0][i]);
                }
            }
            t_haupt += 128.0 / 48_000.0;
        }
        assert_eq!(leer, 0, "leer gelaufen");
        assert!(spitze_fehler < 1e-4, "Kanäle vertauscht/verschoben");
        // Pegel des Sinus bleibt erhalten
        let spitze = ausgang.iter().fold(0.0f32, |a, &v| a.max(v.abs()));
        assert!((spitze - 0.5).abs() < 0.02, "Spitze {spitze}");
    }
}
