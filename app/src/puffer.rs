//! Empfangsseite: Jitter-Puffer, Taktausgleich und Verdeckung von Paketverlusten.
//!
//! Netz-Thread (`Empfang`): ordnet Pakete nach Sequenznummer, nutzt die mitgeschickte
//! Kopie des Vorgängerpakets bei einzelnen Verlusten, verdeckt größere Lücken
//! (gespiegelte Fortsetzung mit Ausblendung) und schiebt Stereo-Paare (L, R verschachtelt;
//! Mono kommt auf beiden Seiten an) in einen lock-freien Ringpuffer.
//!
//! Wiedergabe-Thread (`Wiedergabe`): liest mit einem kubischen Resampler, dessen
//! Verhältnis ein PI-Regler minimal nachführt (Taktdrift zwischen den Soundkarten
//! beider Räume, Ratenunterschiede 44,1/48 kHz). Ziel des Puffers = eigener Block +
//! Paketraster + gemessener Jitter + Reserve (+ Zuschlag nach Aussetzern).

use crate::gemeinsam::{db_zu_faktor, uhr_us, Geteilt};
use protokoll::klang::{bassdrum, Stimme};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;

/// Kanäle im Ring und im Wiedergabeblock (immer Stereo-Paare)
pub const CH: usize = 2;
pub const RING: usize = 48_000 * 2 * CH;
const EINBLENDEN: usize = 48;

pub struct Empfang {
    prod: rtrb::Producer<f32>,
    erwartet: Option<u32>,
    letztes: Vec<f32>,
    verdeckt: bool,
    rate: u32,
    tmp: Vec<f32>,
    pub empfangen: u64,
    pub verloren: u64,
    pub gerettet: u64,
    geteilt: Arc<Geteilt>,
    start: std::time::Instant,
    soll_ms: f64,
    basis: f64,
    t_letzt: f64,
    j_akt: f64,
    j_alt: f64,
    j_start: f64,
}

/// Wie schnell die Bezugslinie der Jittermessung nach oben nachgeben darf (Taktdrift)
const LECK: f64 = 0.0002;

impl Empfang {
    pub fn neu(prod: rtrb::Producer<f32>, geteilt: Arc<Geteilt>) -> Self {
        Empfang {
            prod,
            erwartet: None,
            letztes: Vec::new(),
            verdeckt: false,
            rate: 0,
            tmp: Vec::with_capacity(8192),
            empfangen: 0,
            verloren: 0,
            gerettet: 0,
            geteilt,
            start: std::time::Instant::now(),
            soll_ms: 0.0,
            basis: f64::NAN,
            t_letzt: 0.0,
            j_akt: 0.0,
            j_alt: 0.0,
            j_start: 0.0,
        }
    }

    /// Verspätung jedes Pakets gegenüber dem Paketraster messen (früheste Ankunft =
    /// Bezugslinie). Der größte Wert der letzten 3-6 s ist der Jitter, den der
    /// Puffer abdecken muss. Bündel (z. B. 2 Pakete je 10-ms-Block) zählen mit.
    fn jitter_messen(&mut self, t_ms: f64, frames: usize) {
        self.soll_ms += frames as f64 * 1000.0 / self.rate as f64;
        let x = t_ms - self.soll_ms;
        if self.basis.is_nan() {
            self.basis = x;
            self.j_start = t_ms;
        } else {
            self.basis = (self.basis + (t_ms - self.t_letzt).max(0.0) * LECK).min(x);
        }
        self.t_letzt = t_ms;
        self.j_akt = self.j_akt.max(x - self.basis);
        if t_ms - self.j_start >= 3000.0 {
            self.j_alt = self.j_akt;
            self.j_akt = 0.0;
            self.j_start = t_ms;
        }
        self.geteilt.jitter_ms.set(self.j_akt.max(self.j_alt).min(200.0) as f32);
    }

    fn jitter_neu(&mut self) {
        self.soll_ms = 0.0;
        self.basis = f64::NAN;
    }

    /// Neuer Partner / neue Rate: Puffer leeren (das erledigt der Wiedergabe-Thread)
    pub fn zuruecksetzen(&mut self) {
        self.erwartet = None;
        self.letztes.clear();
        self.verdeckt = false;
        self.jitter_neu();
        self.j_akt = 0.0;
        self.j_alt = 0.0;
        self.geteilt.reset.store(true, Relaxed);
    }

    /// `proben` = Stereo-Paare (L, R verschachtelt)
    pub fn paket(&mut self, seq: u32, rate: u32, proben: &[f32], vorher: Option<&[f32]>) {
        let t = self.start.elapsed().as_secs_f64() * 1000.0;
        self.paket_zeit(seq, rate, proben, vorher, t);
    }

    pub fn paket_zeit(&mut self, seq: u32, rate: u32, proben: &[f32], vorher: Option<&[f32]>, t_ms: f64) {
        let frames = proben.len() / CH;
        if frames == 0 || rate < 8000 {
            return;
        }
        if rate != self.rate {
            self.rate = rate;
            self.geteilt.quell_rate.store(rate, Relaxed);
            self.zuruecksetzen();
        }
        self.geteilt.paket_frames.store(frames as u32, Relaxed);
        let Some(e) = self.erwartet else {
            self.empfangen += 1;
            self.jitter_neu();
            self.jitter_messen(t_ms, frames);
            self.schieben(proben, true);
            self.erwartet = Some(seq.wrapping_add(1));
            return;
        };
        let d = seq.wrapping_sub(e) as i32;
        if d < 0 {
            // zu spät oder doppelt (z. B. beim Wechsel Server/direkt)
            return;
        }
        self.empfangen += 1;
        let mut fehlend = d as usize;
        if fehlend > 0 {
            // Lücke länger als 20 ms: nicht verdecken, sondern neu einsetzen
            if fehlend * frames > rate as usize / 50 {
                self.verloren += fehlend as u64;
                self.jitter_neu();
                self.jitter_messen(t_ms, frames);
                self.schieben(proben, true);
                self.erwartet = Some(seq.wrapping_add(1));
                return;
            }
            let rettung = vorher.filter(|v| v.len() >= CH);
            if rettung.is_some() {
                fehlend -= 1;
            }
            for _ in 0..fehlend {
                self.verdecken(frames);
            }
            self.verloren += fehlend as u64;
            if let Some(v) = rettung {
                self.gerettet += 1;
                let ein = self.verdeckt;
                self.schieben(v, ein);
            }
        }
        self.jitter_messen(t_ms, (d as usize + 1) * frames);
        let ein = self.verdeckt;
        self.schieben(proben, ein);
        self.erwartet = Some(seq.wrapping_add(1));
    }

    fn schieben(&mut self, p: &[f32], einblenden: bool) {
        self.tmp.clear();
        self.tmp.extend_from_slice(p);
        if einblenden {
            let n = EINBLENDEN.min(self.tmp.len() / CH);
            for (i, paar) in self.tmp.chunks_exact_mut(CH).take(n).enumerate() {
                let g = i as f32 / n as f32;
                for x in paar {
                    *x *= g;
                }
            }
        }
        self.ring_schreiben();
        self.letztes.clear();
        self.letztes.extend_from_slice(p);
        self.verdeckt = false;
    }

    /// Lücke füllen: gespiegelte Fortsetzung des letzten Pakets (keine Sprungstelle),
    /// innerhalb von 5 ms auf Null ausgeblendet.
    fn verdecken(&mut self, frames: usize) {
        self.tmp.clear();
        let l = self.letztes.len() / CH;
        if self.verdeckt || l == 0 {
            self.tmp.resize(frames * CH, 0.0);
        } else {
            let blende = (self.rate as usize / 200).clamp(1, frames.max(1));
            for i in 0..frames {
                let k = i % (2 * l);
                let idx = if k < l { l - 1 - k } else { k - l };
                let g = if i < blende { 1.0 - i as f32 / blende as f32 } else { 0.0 };
                for c in 0..CH {
                    self.tmp.push(self.letztes[idx * CH + c] * g);
                }
            }
        }
        self.ring_schreiben();
        self.verdeckt = true;
    }

    fn ring_schreiben(&mut self) {
        let n = self.tmp.len();
        if self.prod.slots() < n {
            return;
        }
        if let Ok(mut chunk) = self.prod.write_chunk(n) {
            let (a, b) = chunk.as_mut_slices();
            a.copy_from_slice(&self.tmp[..a.len()]);
            b.copy_from_slice(&self.tmp[a.len()..]);
            chunk.commit_all();
        }
    }
}

#[inline]
fn hermite(x0: f32, x1: f32, x2: f32, x3: f32, t: f32) -> f32 {
    let c1 = 0.5 * (x2 - x0);
    let c2 = x0 - 2.5 * x1 + 2.0 * x2 - 0.5 * x3;
    let c3 = 0.5 * (x3 - x0) + 1.5 * (x1 - x2);
    ((c3 * t + c2) * t + c1) * t + x1
}

/// Stereo-Begrenzer ohne Vorausschau (keine zusätzliche Latenz) + weiche Sättigung.
/// Beide Seiten werden gemeinsam geregelt, damit das Stereobild nicht wandert.
pub struct Begrenzer {
    huelle: f32,
    rueck: f32,
}

#[inline]
fn weich(y: f32) -> f32 {
    let s = y.abs();
    if s <= 0.9 {
        y
    } else {
        y.signum() * (0.9 + 0.1 * ((s - 0.9) / 0.1).tanh())
    }
}

impl Begrenzer {
    pub fn neu(rate: f32) -> Self {
        Begrenzer { huelle: 0.0, rueck: (-1.0 / (0.15 * rate)).exp() }
    }
    #[inline]
    pub fn paar(&mut self, l: f32, r: f32) -> (f32, f32) {
        let a = l.abs().max(r.abs());
        self.huelle = if a > self.huelle { a } else { self.huelle * self.rueck };
        let g = if self.huelle > 0.89 { 0.89 / self.huelle } else { 1.0 };
        (weich(l * g), weich(r * g))
    }
}

pub struct Wiedergabe {
    cons: rtrb::Consumer<f32>,
    aus_rate: f64,
    spielt: bool,
    h: [[f32; 4]; CH],
    frac: f64,
    integral: f64,
    fuell_ema: f64,
    ziel_ms: f64,
    zuschlag_ms: f64,
    fenster_t: f64,
    fenster_unterlauf: bool,
    verstaerkung: f32,
    verst_ziel: f32,
    springen: bool,
    begrenzer: Begrenzer,
    klick: KlickGeber,
    spur: SpurGeber,
    geteilt: Arc<Geteilt>,
}

const KP: f64 = 0.0004; // je ms Abweichung
const KI: f64 = 0.0001; // je ms·s
const MAX_KORR: f64 = 0.004;
const MAX_ZIEL_MS: f64 = 150.0;
const RESERVE_MS: f64 = 0.5;

impl Wiedergabe {
    pub fn neu(cons: rtrb::Consumer<f32>, aus_rate: u32, geteilt: Arc<Geteilt>) -> Self {
        Wiedergabe {
            cons,
            aus_rate: aus_rate as f64,
            spielt: false,
            h: [[0.0; 4]; CH],
            frac: 0.0,
            integral: 0.0,
            fuell_ema: 0.0,
            ziel_ms: 0.0,
            zuschlag_ms: 0.0,
            fenster_t: 0.0,
            fenster_unterlauf: false,
            verstaerkung: 0.0,
            verst_ziel: 1.0,
            springen: false,
            begrenzer: Begrenzer::neu(aus_rate as f32),
            klick: KlickGeber::neu(),
            spur: SpurGeber::default(),
            geteilt,
        }
    }

    pub fn rate_setzen(&mut self, aus_rate: u32) {
        if self.aus_rate != aus_rate as f64 {
            self.aus_rate = aus_rate as f64;
            self.begrenzer = Begrenzer::neu(aus_rate as f32);
            self.spielt = false;
        }
    }

    #[cfg(test)]
    pub fn ziel_ms(&self) -> f64 {
        self.ziel_ms
    }

    fn leeren(&mut self) {
        let n = self.cons.slots();
        if let Ok(c) = self.cons.read_chunk(n) {
            c.commit_all();
        }
    }

    /// Ein Stereo-Paar aus dem Ring holen
    #[inline]
    fn paar_holen(&mut self) -> Option<[f32; CH]> {
        if self.cons.slots() < CH {
            return None;
        }
        let mut p = [0.0; CH];
        for x in p.iter_mut() {
            *x = self.cons.pop().ok()?;
        }
        Some(p)
    }

    /// Wiedergabeblock füllen (Stereo-Paare): Ton der Gegenstelle + Metronom
    pub fn fuellen(&mut self, aus: &mut [f32]) {
        self.ton_fuellen(aus);
        let g = self.geteilt.clone();
        self.klick.mischen(aus, &g, self.aus_rate);
        self.spur.mischen(aus, &g, self.aus_rate);
        // Aufnahme anhören (nur lokal)
        if let Ok(mut v) = g.vorhoeren.try_lock() {
            if let Some(v) = v.as_mut() {
                v.mischen(aus, self.aus_rate, &g);
            }
        };
    }

    fn ton_fuellen(&mut self, aus: &mut [f32]) {
        let g = self.geteilt.clone();
        if g.reset.swap(false, Relaxed) {
            self.leeren();
            self.spielt = false;
        }
        let in_rate = g.quell_rate.load(Relaxed) as f64;
        let n = aus.len() / CH;
        if in_rate < 8000.0 || n == 0 {
            aus.fill(0.0);
            g.anzeige.puffer_ist_ms.set(0.0);
            return;
        }
        let frames_im_ring = |s: &Self| s.cons.slots() / CH;
        let dt = n as f64 / self.aus_rate;
        let auto = g.regler.puffer_auto.load(Relaxed);
        let paket_ms = g.paket_frames.load(Relaxed) as f64 * 1000.0 / in_rate;
        let block_ms = n as f64 * 1000.0 / self.aus_rate;
        let jitter = g.jitter_ms.get() as f64;
        self.ziel_ms = (block_ms + paket_ms + jitter + RESERVE_MS + self.zuschlag_ms).min(MAX_ZIEL_MS);
        // von Hand: Reserve über dem Minimum (Block + Paket), 0 = ohne Jitter-Reserve
        let ziel = if auto {
            self.ziel_ms
        } else {
            (block_ms + paket_ms + RESERVE_MS + (g.regler.puffer_ms.get() as f64).clamp(0.0, 100.0)).min(MAX_ZIEL_MS)
        };
        let ziel_frames = (ziel * in_rate / 1000.0) as usize;
        let verhaeltnis_nenn = in_rate / self.aus_rate;
        let benoetigt = (n as f64 * verhaeltnis_nenn * (1.0 + MAX_KORR)).ceil() as usize + 2;

        if !self.spielt {
            if frames_im_ring(self) >= ziel_frames.max(benoetigt) + 3 {
                self.h = [[0.0; 4]; CH];
                for i in 1..4 {
                    let p = self.paar_holen().unwrap_or([0.0; CH]);
                    for c in 0..CH {
                        self.h[c][i] = p[c];
                    }
                }
                self.frac = 0.0;
                self.integral = 0.0;
                self.fuell_ema = frames_im_ring(self) as f64 * 1000.0 / in_rate;
                self.verstaerkung = 0.0;
                self.verst_ziel = 1.0;
                self.spielt = true;
            } else {
                aus.fill(0.0);
                g.anzeige.puffer_ist_ms.set((frames_im_ring(self) as f64 * 1000.0 / in_rate) as f32);
                g.anzeige.puffer_ziel_ms.set(ziel as f32);
                return;
            }
        }

        // Regler
        let fuell_ms = frames_im_ring(self) as f64 * 1000.0 / in_rate;
        let a = 1.0 - (-dt / 0.3).exp();
        self.fuell_ema += (fuell_ms - self.fuell_ema) * a;
        let fehler = self.fuell_ema - ziel;
        if fehler > 15f64.max(ziel) && !self.springen {
            // viel zu voll (z. B. nach Wegwechsel): ausblenden, Überschuss verwerfen, einblenden
            self.springen = true;
            self.verst_ziel = 0.0;
        }
        self.integral = (self.integral + fehler * dt).clamp(-MAX_KORR / KI * 0.5, MAX_KORR / KI * 0.5);
        let korr = (KP * fehler + KI * self.integral).clamp(-MAX_KORR, MAX_KORR);
        let schritt = verhaeltnis_nenn * (1.0 + korr);

        let lautst = db_zu_faktor(g.regler.lautstaerke_db.get());
        let rampe = 1.0 / 48.0;
        let mut spitze = 0.0f32;
        let mut unterlauf_bei = None;
        for i in 0..n {
            let t = self.frac as f32;
            let l = hermite(self.h[0][0], self.h[0][1], self.h[0][2], self.h[0][3], t);
            let r = hermite(self.h[1][0], self.h[1][1], self.h[1][2], self.h[1][3], t);
            if self.verstaerkung < self.verst_ziel {
                self.verstaerkung = (self.verstaerkung + rampe).min(self.verst_ziel);
            } else if self.verstaerkung > self.verst_ziel {
                self.verstaerkung = (self.verstaerkung - rampe).max(self.verst_ziel);
            }
            let v = self.verstaerkung * lautst;
            let (zl, zr) = self.begrenzer.paar(l * v, r * v);
            spitze = spitze.max(zl.abs()).max(zr.abs());
            aus[i * CH] = zl;
            aus[i * CH + 1] = zr;
            self.frac += schritt;
            while self.frac >= 1.0 {
                self.frac -= 1.0;
                match self.paar_holen() {
                    Some(p) => {
                        for c in 0..CH {
                            self.h[c] = [self.h[c][1], self.h[c][2], self.h[c][3], p[c]];
                        }
                    }
                    None => {
                        unterlauf_bei = Some(i + 1);
                        break;
                    }
                }
            }
            if unterlauf_bei.is_some() {
                break;
            }
        }
        if let Some(ab) = unterlauf_bei {
            // Aussetzer: kurz ausblenden, dann warten, bis der Puffer wieder voll ist
            for c in 0..CH {
                let mut letzter = if ab > 0 { aus[(ab - 1) * CH + c] } else { 0.0 };
                for i in ab..n {
                    letzter *= 0.9;
                    aus[i * CH + c] = letzter;
                }
            }
            self.spielt = false;
            self.fenster_unterlauf = true;
            g.anzeige.unterlaeufe.fetch_add(1, Relaxed);
            if auto {
                self.zuschlag_ms = (self.zuschlag_ms + 1.0).min(50.0);
            }
        }
        if self.springen && self.verstaerkung <= 0.0 {
            let ueber = frames_im_ring(self).saturating_sub(ziel_frames) * CH;
            if let Ok(c) = self.cons.read_chunk(ueber) {
                c.commit_all();
            }
            self.fuell_ema = ziel;
            self.integral = 0.0;
            self.springen = false;
            self.verst_ziel = 1.0;
        }

        // Zuschlag langsam abbauen, solange keine Aussetzer kommen
        self.fenster_t += dt;
        if self.fenster_t >= 2.0 {
            if !self.fenster_unterlauf {
                self.zuschlag_ms = (self.zuschlag_ms - 0.1).max(0.0);
            }
            self.fenster_t = 0.0;
            self.fenster_unterlauf = false;
        }

        g.anzeige.pegel_aus.max(spitze);
        g.anzeige.puffer_ist_ms.set(self.fuell_ema as f32);
        g.anzeige.puffer_ziel_ms.set(ziel as f32);
    }
}

/// Klangformen des Metronoms (Reihenfolge = Auswahlliste der Oberfläche)
pub const KLANGFORMEN: [&str; 7] = ["Klick", "Glocke", "Holzblock", "Rimshot", "Kuhglocke", "Bassdrum / Snare", "Hi-Hat"];

#[derive(Clone, Copy, PartialEq)]
struct Raster {
    start: f64,
    schlag_us: f64,
    takt: i64,
}

/// Metronom, sample-genau an der Serveruhr ausgerichtet. Der Klick ERKLINGT zur Serverzeit des
/// Schlags – in beiden Räumen gleich (Ausgabelatenz des Geräts ist eingerechnet).
///
/// Damit kein Schlag verloren geht:
/// - Die Blockzeit läuft über die Probenzahl mit und wird nur langsam an die Uhr angeglichen
///   (im geteilten Modus schwanken die Aufrufzeiten um mehrere Millisekunden).
/// - Sprünge des Uhrabgleichs werden sanft nachgeführt (höchstens 0,2 ms je Block).
/// - Gemerkt wird die Serverzeit des letzten Schlags (nicht seine Nummer); ein knapp verpasster
///   Schlag (bis 40 ms) wird sofort nachgeholt statt verschluckt.
/// - Bei einer Änderung läuft das alte Raster weiter, bis das neue am nächsten Taktanfang beginnt.
/// Spur des Raums: spielt die geladene Datei so ab, dass Position 0 zur Serverzeit `spur_start`
/// aus dem Lautsprecher kommt – in allen Räumen gleichzeitig (wie das Metronom). Kein Puffer im
/// Echtzeitweg: nur Mischen in den Ausgabeblock, Gleichlauf per winziger Tempokorrektur.
#[derive(Default)]
pub struct SpurGeber {
    ton: Option<Arc<Vec<i16>>>,
    schluessel: String,
    rate: f64,
    pos: f64,
    gueltig: bool,
    stand: u32,
    t_block: f64,
    uhr_gueltig: bool,
    versatz: f64,
    versatz_ok: bool,
}

impl SpurGeber {
    pub fn mischen(&mut self, aus: &mut [f32], g: &Geteilt, rate: f64) {
        let n = aus.len() / CH;
        if n == 0 || rate < 8000.0 {
            return;
        }
        // Lokale Uhr des Blocks glätten (wie KlickGeber)
        let jetzt = uhr_us() as f64;
        let block_us = n as f64 * 1e6 / rate;
        if !self.uhr_gueltig || (jetzt - self.t_block).abs() > 50_000.0 {
            self.t_block = jetzt;
            self.uhr_gueltig = true;
        } else {
            self.t_block += (jetzt - self.t_block) * 0.01;
        }
        let t0_lokal = self.t_block;
        self.t_block += block_us;
        let v = g.uhr_versatz.load(Relaxed) as f64;
        if !self.versatz_ok || (v - self.versatz).abs() > 50_000.0 {
            self.versatz = v;
            self.versatz_ok = true;
        } else {
            self.versatz += (v - self.versatz).clamp(-200.0, 200.0);
        }
        if !g.spur_an.load(Relaxed) || !g.uhr_ok.load(Relaxed) {
            self.gueltig = false;
            return;
        }
        // geladene Datei übernehmen (nur wenn sie wechselt)
        if let Ok(t) = g.spur_ton.try_lock() {
            match t.as_ref() {
                Some(t) if t.schluessel != self.schluessel => {
                    self.ton = Some(t.daten.clone());
                    self.schluessel = t.schluessel.clone();
                    self.rate = t.rate as f64;
                    self.gueltig = false;
                }
                None if self.ton.is_some() => {
                    self.ton = None;
                    self.schluessel.clear();
                }
                _ => {}
            }
        }
        let Some(ton) = self.ton.clone() else { return };
        if self.rate < 8000.0 {
            return;
        }
        let stand = g.spur_stand.load(Relaxed);
        if stand != self.stand {
            self.stand = stand;
            self.gueltig = false;
        }
        // Serverzeit, zu der Sample 0 dieses Blocks aus dem Lautsprecher kommt
        let t0 = t0_lokal + self.versatz + g.anzeige.aus_latenz_ms.get() as f64 * 1000.0;
        let soll = (t0 - g.spur_start.load(Relaxed) as f64) / 1e6 * self.rate;
        let frames = (ton.len() / 2) as f64;
        if soll >= frames + 1.0 {
            self.gueltig = false;
            return;
        }
        if !self.gueltig || (soll - self.pos).abs() > self.rate * 0.03 {
            self.pos = soll;
            self.gueltig = true;
        }
        // Gleichlauf: 1 ms Abweichung -> 0,2 % schneller/langsamer (unhörbar)
        let fehler_s = (soll - self.pos) / self.rate;
        let schritt = self.rate / rate * (1.0 + (fehler_s * 2.0).clamp(-0.002, 0.002));
        let db = g.regler.spur_db.get();
        let laut = if db <= -39.5 { 0.0 } else { db_zu_faktor(db) / 32768.0 };
        for p in aus.chunks_exact_mut(CH) {
            let i = self.pos.floor();
            if i >= 0.0 && i + 1.0 < frames && laut > 0.0 {
                let k = i as usize * 2;
                let t = (self.pos - i) as f32;
                let l = ton[k] as f32 + (ton[k + 2] as f32 - ton[k] as f32) * t;
                let r = ton[k + 1] as f32 + (ton[k + 3] as f32 - ton[k + 1] as f32) * t;
                p[0] += l * laut;
                p[1] += r * laut;
            }
            self.pos += schritt;
        }
    }
}

pub struct KlickGeber {
    t_block: f64,
    gueltig: bool,
    versatz: f64,
    versatz_ok: bool,
    stimmen: [Stimme; 3],
    naechste_stimme: usize,
    letzte_zeit: f64,
    raster: Option<Raster>,
    vorher: Option<Raster>,
    stand: u32,
    /// Zahl der gespielten Schläge (für Tests)
    pub gespielt: u64,
}

impl KlickGeber {
    pub fn neu() -> Self {
        KlickGeber {
            t_block: 0.0,
            gueltig: false,
            versatz: 0.0,
            versatz_ok: false,
            stimmen: [Stimme::still(); 3],
            naechste_stimme: 0,
            letzte_zeit: f64::MIN,
            raster: None,
            vorher: None,
            stand: u32::MAX,
            gespielt: 0,
        }
    }

    /// `aus` = Stereo-Paare
    pub fn mischen(&mut self, aus: &mut [f32], g: &Geteilt, rate: f64) {
        let n = aus.len() / CH;
        if n == 0 || rate < 8000.0 {
            return;
        }
        let jetzt = uhr_us() as f64;
        let block_us = n as f64 * 1e6 / rate;
        if !self.gueltig || (jetzt - self.t_block).abs() > 50_000.0 {
            self.t_block = jetzt;
            self.gueltig = true;
        } else {
            self.t_block += (jetzt - self.t_block) * 0.01;
        }
        let t0_lokal = self.t_block;
        self.t_block += block_us;
        let v = g.uhr_versatz.load(Relaxed) as f64;
        if !self.versatz_ok || (v - self.versatz).abs() > 50_000.0 {
            self.versatz = v;
            self.versatz_ok = true;
        } else {
            self.versatz += (v - self.versatz).clamp(-200.0, 200.0);
        }

        let m = &g.metronom;
        let db = g.regler.metronom_db.get();
        let an = m.an.load(Relaxed) && g.uhr_ok.load(Relaxed) && db > -39.5;
        if !an {
            self.stimmen = [Stimme::still(); 3];
            self.raster = None;
            self.vorher = None;
            self.stand = u32::MAX;
            return;
        }
        let neu = Raster {
            start: m.start_us.load(Relaxed) as f64,
            schlag_us: 60e6 / (m.bpm.get() as f64).max(1.0),
            takt: m.takt.load(Relaxed).max(1) as i64,
        };
        let stand = m.stand.load(Relaxed);
        if stand != self.stand {
            self.stand = stand;
            // bisheriges Raster weiterspielen, bis das neue beginnt
            if self.raster.is_some_and(|r| r != neu) {
                self.vorher = self.raster;
            }
        }
        self.raster = Some(neu);
        let lautst = db_zu_faktor(db) * 0.5;
        let form = g.regler.metronom_klang.load(Relaxed);
        // Serverzeit, zu der Sample 0 dieses Blocks aus dem Lautsprecher kommt
        let t0 = t0_lokal + self.versatz + g.anzeige.aus_latenz_ms.get() as f64 * 1000.0;
        let t1 = t0 + block_us;
        if self.vorher.is_some() && t0 >= neu.start {
            self.vorher = None;
        }

        // (Probe, betont, Stelle im Takt)
        let mut starts: [(usize, bool, i64); 8] = [(usize::MAX, false, 0); 8];
        let mut anzahl = 0;
        let ab = (t0 - 40_000.0).max(self.letzte_zeit + 1000.0);
        for (r, bis) in [(self.vorher, neu.start), (Some(neu), f64::MAX)] {
            let Some(r) = r else { continue };
            let mut k = (((ab - r.start) / r.schlag_us).ceil() as i64).max(0);
            while anzahl < starts.len() {
                let tb = r.start + k as f64 * r.schlag_us;
                if tb >= t1 || tb >= bis {
                    break;
                }
                if tb > self.letzte_zeit + 1000.0 {
                    let idx = (((tb - t0).max(0.0) * rate / 1e6) as usize).min(n - 1);
                    starts[anzahl] = (idx, r.takt > 1 && k % r.takt == 0, if bassdrum(k, r.takt) { 0 } else { 1 });
                    anzahl += 1;
                    self.letzte_zeit = tb;
                }
                k += 1;
            }
        }
        starts[..anzahl].sort_by_key(|s| s.0);

        let rate32 = rate as f32;
        let mut naechster = 0;
        for i in 0..n {
            while naechster < anzahl && starts[naechster].0 == i {
                let (_, betont, im_takt) = starts[naechster];
                self.stimmen[self.naechste_stimme] = Stimme::start(form, betont, im_takt, rate);
                self.naechste_stimme = (self.naechste_stimme + 1) % self.stimmen.len();
                self.gespielt += 1;
                naechster += 1;
            }
            let mut y = 0.0;
            for s in self.stimmen.iter_mut() {
                y += s.probe(rate32);
            }
            if y != 0.0 {
                let v = y * lautst;
                for c in 0..CH {
                    aus[i * CH + c] = (aus[i * CH + c] + v).clamp(-1.0, 1.0);
                }
            }
        }
    }
}

#[cfg(test)]
mod klick_tests {
    use super::*;

    /// Alle Klangformen erzeugen hörbaren, begrenzten Ton und klingen aus
    #[test]
    fn klangformen() {
        for form in 0..KLANGFORMEN.len() as u32 {
            for schlag in 0..2 {
                let mut s = Stimme::start(form, schlag == 0, schlag, 48_000.0);
                let mut spitze = 0.0f32;
                let mut proben = 0;
                loop {
                    if !s.klingt() {
                        break;
                    }
                    let y = s.probe(48_000.0);
                    assert!(y.is_finite());
                    spitze = spitze.max(y.abs());
                    proben += 1;
                }
                assert!(spitze > 0.05 && spitze < 3.0, "Form {form}: Spitze {spitze}");
                assert!(proben > 1000, "Form {form} zu kurz");
            }
        }
    }

    /// Schläge werden gesetzt (Klickgeber arbeitet mit der echten Uhr)
    #[test]
    fn klick_raster() {
        let g = Geteilt::neu();
        g.anzeige_zuruecksetzen();
        g.uhr_ok.store(true, Relaxed);
        g.regler.metronom_db.set(0.0);
        let m = &g.metronom;
        m.bpm.set(120.0);
        m.takt.store(4, Relaxed);
        m.start_us.store(uhr_us() + 50_000, Relaxed);
        m.stand.store(1, Relaxed);
        m.an.store(true, Relaxed);
        let mut k = KlickGeber::neu();
        let mut block = vec![0.0f32; 128 * CH];
        let beginn = std::time::Instant::now();
        while beginn.elapsed() < std::time::Duration::from_millis(1200) {
            block.fill(0.0);
            k.mischen(&mut block, &g, 48_000.0);
            std::thread::sleep(std::time::Duration::from_micros(2667));
        }
        assert!(k.gespielt >= 2, "keine Schläge: {}", k.gespielt);
    }
}

#[cfg(test)]
mod klick_diagnose {
    use super::*;

    /// Wie der Testmodus: 128er-Blöcke im 2,67-ms-Takt; Klickanfänge gegen das Raster messen
    #[test]
    #[ignore]
    fn klick_genauigkeit() {
        let g = Geteilt::neu();
        g.anzeige_zuruecksetzen();
        g.uhr_ok.store(true, Relaxed);
        g.regler.metronom_db.set(0.0);
        g.uhr_versatz.store(942_108, Relaxed);
        let m = &g.metronom;
        m.bpm.set(120.0);
        m.takt.store(4, Relaxed);
        m.start_us.store((uhr_us() as i64 + 942_108 + 300_000) as u64, Relaxed);
        m.stand.store(1, Relaxed);
        m.an.store(true, Relaxed);
        let mut k = KlickGeber::neu();
        let mut block = [0.0f32; 256];
        let periode = std::time::Duration::from_secs_f64(128.0 / 48_000.0);
        let mut naechste = std::time::Instant::now();
        let beginn = naechste;
        let mut still = true;
        let mut ab = Vec::new();
        while beginn.elapsed() < std::time::Duration::from_secs(3) {
            let t_block = uhr_us() as f64;
            block.fill(0.0);
            k.mischen(&mut block, &g, 48_000.0);
            for (i, &x) in block.iter().step_by(2).enumerate() {
                if x.abs() > 0.001 && still {
                    let t = t_block + i as f64 * 1e6 / 48_000.0 + 942_108.0;
                    let mut d = (t - m.start_us.load(Relaxed) as f64) % 500_000.0;
                    if d > 250_000.0 {
                        d -= 500_000.0;
                    }
                    ab.push(d / 1000.0);
                }
                still = x.abs() <= 1e-7;
            }
            naechste += periode;
            let jetzt = std::time::Instant::now();
            if naechste > jetzt {
                std::thread::sleep(naechste - jetzt);
            }
        }
        println!("Anfänge {} gespielt {}: {:?}", ab.len(), k.gespielt, ab.iter().map(|x| (x * 10.0).round() / 10.0).collect::<Vec<_>>());
    }

    /// Wie geteilter WASAPI-Modus (Aufrufe ~10 ms, schwankende Blockgröße) mit springendem
    /// Uhrabgleich und einer Tempoänderung: kein Schlag darf fehlen oder doppelt kommen.
    #[test]
    fn schlaege_lueckenlos() {
        let g = Geteilt::neu();
        g.anzeige_zuruecksetzen();
        g.uhr_ok.store(true, Relaxed);
        g.regler.metronom_db.set(0.0);
        let m = &g.metronom;
        m.bpm.set(240.0);
        m.takt.store(4, Relaxed);
        let start = uhr_us() + 50_000;
        m.start_us.store(start, Relaxed);
        m.stand.store(1, Relaxed);
        m.an.store(true, Relaxed);
        let mut k = KlickGeber::neu();
        let mut block = vec![0.0f32; 4096 * CH];
        let beginn = std::time::Instant::now();
        let mut gespielt = 0.0f64;
        let mut runde = 0u32;
        while beginn.elapsed() < std::time::Duration::from_millis(4000) {
            runde += 1;
            // Uhrabgleich springt gelegentlich um ±3 ms
            if runde % 37 == 0 {
                g.uhr_versatz.store(if runde % 74 == 0 { 3000 } else { -3000 }, Relaxed);
            }
            let jetzt = beginn.elapsed().as_secs_f64();
            let frames = (((jetzt + 0.020) - gespielt) * 48_000.0).max(0.0) as usize;
            if frames > 0 {
                gespielt += frames as f64 / 48_000.0;
                let b = &mut block[..frames.min(4096) * CH];
                b.fill(0.0);
                k.mischen(b, &g, 48_000.0);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // erwartete Schläge: von start bis zum Ende der gespielten Zeit (+20 ms Vorlauf)
        let ende = beginn.elapsed().as_secs_f64();
        let erwartet = ((ende - 0.05) * 4.0).floor() as i64;
        let ist = k.gespielt as i64;
        assert!((ist - erwartet).abs() <= 1, "Schläge {ist}, erwartet {erwartet}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stereo(m: &[f32]) -> Vec<f32> {
        m.iter().flat_map(|&x| [x, x]).collect()
    }

    /// Simulation: Sender 48 kHz mit 128er-Paketen und Netzjitter, Empfänger mit eigener
    /// Rate, Blockgröße und Taktabweichung. Liefert (Ziel ms, Ist ms, Aussetzer nach 60 s, gesamt).
    fn simulieren(aus_rate: u32, aus_block: usize, jitter_s: f64, drift: f64, dauer_s: f64) -> (f64, f32, u32, u32) {
        let g = Arc::new(Geteilt::neu());
        g.anzeige_zuruecksetzen();
        let (prod, cons) = rtrb::RingBuffer::new(RING);
        let mut e = Empfang::neu(prod, g.clone());
        let mut w = Wiedergabe::neu(cons, aus_rate, g.clone());
        let mut t_send = 0.0f64;
        let mut t_play = 0.0f64;
        let mut seq = 0u32;
        let mut phase = 0.0f64;
        let mut rng = 12345u64;
        let mut zufall = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            (rng % 1000) as f64 / 1000.0
        };
        let mut aus = vec![0.0f32; aus_block * CH];
        let mut warteschlange: Vec<(f64, u32, Vec<f32>)> = Vec::new();
        let mut spaet = 0;
        while t_play < dauer_s {
            while t_send <= t_play + 0.05 {
                let p: Vec<f32> = (0..128)
                    .map(|_| {
                        phase += 440.0 / 48_000.0;
                        (phase * std::f64::consts::TAU).sin() as f32 * 0.5
                    })
                    .collect();
                warteschlange.push((t_send + 0.010 + zufall() * jitter_s, seq, stereo(&p)));
                seq += 1;
                t_send += 128.0 / 48_000.0;
            }
            warteschlange.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
            while !warteschlange.is_empty() && warteschlange[0].0 <= t_play {
                let (ankunft, s, p) = warteschlange.remove(0);
                e.paket_zeit(s, 48_000, &p, None, ankunft * 1000.0);
            }
            let vorher = g.anzeige.unterlaeufe.load(Relaxed);
            w.fuellen(&mut aus);
            if t_play > 60.0 && g.anzeige.unterlaeufe.load(Relaxed) > vorher {
                spaet += 1;
            }
            t_play += aus_block as f64 / aus_rate as f64 * (1.0 + drift);
        }
        (w.ziel_ms(), g.anzeige.puffer_ist_ms.get(), spaet, g.anzeige.unterlaeufe.load(Relaxed))
    }

    #[test]
    fn regelung_geteilt_441() {
        // geteilter Modus: 10-ms-Blöcke, 44,1 kHz, 2 ms Jitter, Uhr 0,02 % schneller
        let (ziel, ist, spaet, ges) = simulieren(44_100, 441, 0.002, 0.0002, 300.0);
        println!("geteilt: Ziel {ziel:.2} ms, Ist {ist:.2} ms, späte Aussetzer {spaet}, gesamt {ges}");
        assert!(spaet <= 2, "zu viele Aussetzer: {spaet}");
        assert!(ziel < 17.0, "Puffer zu groß: {ziel}");
        assert!((ist as f64 - ziel).abs() < 4.0, "Regelung schwingt: ist {ist} ziel {ziel}");
    }

    #[test]
    fn regelung_exklusiv_128() {
        // exklusiver Modus: 128er-Blöcke bei 48 kHz, 1 ms Jitter, Uhr 0,01 % langsamer
        let (ziel, ist, spaet, ges) = simulieren(48_000, 128, 0.001, -0.0001, 300.0);
        println!("exklusiv: Ziel {ziel:.2} ms, Ist {ist:.2} ms, späte Aussetzer {spaet}, gesamt {ges}");
        assert!(spaet <= 2, "zu viele Aussetzer: {spaet}");
        assert!(ziel < 8.0, "Puffer zu groß: {ziel}");
        assert!((ist as f64 - ziel).abs() < 2.0, "Regelung schwingt: ist {ist} ziel {ziel}");
    }

    #[test]
    fn verlust_mit_kopie() {
        let g = Arc::new(Geteilt::neu());
        let (prod, mut cons) = rtrb::RingBuffer::new(RING);
        let mut e = Empfang::neu(prod, g.clone());
        let a = stereo(&[0.1f32; 100]);
        let b = stereo(&[0.2f32; 100]);
        let c = stereo(&[0.3f32; 100]);
        e.paket(0, 48_000, &a, None);
        // Paket 1 fehlt, Paket 2 trägt die Kopie von 1
        e.paket(2, 48_000, &c, Some(&b));
        assert_eq!(cons.slots(), 300 * CH);
        assert_eq!(e.gerettet, 1);
        assert_eq!(e.verloren, 0);
        for _ in 0..100 * CH {
            cons.pop().unwrap();
        }
        // Kopie wurde ohne Einblendung eingesetzt
        assert!((cons.pop().unwrap() - 0.2).abs() < 1e-6);
    }

    /// Spur: Probe k erklingt zur Serverzeit spur_start + k/rate (hier ohne Versatz/Latenz)
    #[test]
    fn spur_zeitgenau() {
        let g = Geteilt::neu();
        g.anzeige_zuruecksetzen();
        g.uhr_ok.store(true, Relaxed);
        g.regler.spur_db.set(0.0);
        let daten: Vec<i16> = (0..20_000).flat_map(|k: i32| [k as i16, -(k as i16)]).collect();
        *g.spur_ton.lock().unwrap() = Some(crate::gemeinsam::SpurTon { schluessel: "x".into(), rate: 48_000, daten: Arc::new(daten) });
        g.spur_an.store(true, Relaxed);
        // Position 1000 soll jetzt erklingen
        g.uhr_versatz.store(5_000_000, Relaxed);
        g.spur_start.store(uhr_us() + 5_000_000 - (1000.0 / 48_000.0 * 1e6) as u64, Relaxed);
        let mut s = SpurGeber::default();
        let mut aus = vec![0.0f32; 256];
        s.mischen(&mut aus, &g, 48_000.0);
        let erste = aus[0] * 32768.0;
        assert!((erste - 1000.0).abs() < 8.0, "erste Probe {erste}");
        assert!((aus[1] * 32768.0 + erste).abs() < 0.5, "rechts");
        // Folgeblöcke laufen lückenlos weiter
        let mut aus2 = vec![0.0f32; 256];
        s.mischen(&mut aus2, &g, 48_000.0);
        let weiter = aus2[0] * 32768.0;
        assert!((weiter - erste - 128.0).abs() < 2.0, "weiter {weiter}");
        // aus: nichts mischen
        g.spur_an.store(false, Relaxed);
        let mut aus3 = vec![0.0f32; 256];
        s.mischen(&mut aus3, &g, 48_000.0);
        assert!(aus3.iter().all(|&x| x == 0.0));
    }

}
