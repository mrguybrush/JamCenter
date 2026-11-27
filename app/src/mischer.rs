//! Mischpult vor dem Verschicken – Effektkette je Eingang wie im Songcenter (audiosport.info):
//!   Trittschallfilter -> 5-Band-EQ -> Kompressor (+Make-up) -> Hall parallel (Mix) -> Fader -> Panorama
//!
//! Alles ohne zusätzliche Latenz:
//! - EQ: Biquad-Filter nach den Formeln der Web-Audio-Spezifikation (Lowshelf/Glocke/Highshelf),
//!   Probe für Probe, keine Verzögerung.
//! - Kompressor: gleiche statische Kennlinie (Threshold, Ratio, Knee quadratisch, Make-up) wie im
//!   Songcenter, aber OHNE Vorausschau (der DynamicsCompressor des Browsers verzögert um ~6 ms).
//! - Hall: statt Faltung (2 s Impuls = blockweise und teuer) ein Feedback-Delay-Netz mit 8 Leitungen;
//!   die Nachhallzeit entspricht der Songcenter-Hüllkurve (1 − t/Größe)^Abklingen. Das trockene Signal
//!   läuft sofort durch, nur die Hallfahne kommt dazu.

use crate::gemeinsam::{db_zu_faktor, Geteilt, KanalFx, EQ_BAENDER, EQ_VORGABE, MAX_KANAELE, SPEKTRUM_N};
use std::sync::atomic::Ordering::Relaxed;

#[derive(Clone, Copy, Default)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    fn durchlass(&mut self) {
        self.b0 = 1.0;
        self.b1 = 0.0;
        self.b2 = 0.0;
        self.a1 = 0.0;
        self.a2 = 0.0;
    }

    fn setzen(&mut self, b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) {
        self.b0 = (b0 / a0) as f32;
        self.b1 = (b1 / a0) as f32;
        self.b2 = (b2 / a0) as f32;
        self.a1 = (a1 / a0) as f32;
        self.a2 = (a2 / a0) as f32;
    }

    /// Typ 0 = Lowshelf, 1 = Glocke (Peaking), 2 = Highshelf – wie Web Audio (Shelf ohne Q)
    pub fn band(&mut self, typ: u8, f: f64, db: f64, q: f64, rate: f64) {
        if db.abs() < 0.01 {
            self.durchlass();
            return;
        }
        let f = f.clamp(10.0, rate * 0.49);
        let a = 10f64.powf(db / 40.0);
        let w = std::f64::consts::TAU * f / rate;
        let (c, s) = (w.cos(), w.sin());
        match typ {
            1 => {
                let alpha = s / (2.0 * q.max(0.05));
                self.setzen(1.0 + alpha * a, -2.0 * c, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * c, 1.0 - alpha / a);
            }
            _ => {
                // Web Audio: Shelf mit S = 1 -> alpha = sin(w)/2 · √2
                let alpha = s / 2.0 * std::f64::consts::SQRT_2;
                let k = 2.0 * a.sqrt() * alpha;
                if typ == 2 {
                    self.setzen(
                        a * ((a + 1.0) + (a - 1.0) * c + k),
                        -2.0 * a * ((a - 1.0) + (a + 1.0) * c),
                        a * ((a + 1.0) + (a - 1.0) * c - k),
                        (a + 1.0) - (a - 1.0) * c + k,
                        2.0 * ((a - 1.0) - (a + 1.0) * c),
                        (a + 1.0) - (a - 1.0) * c - k,
                    );
                } else {
                    self.setzen(
                        a * ((a + 1.0) - (a - 1.0) * c + k),
                        2.0 * a * ((a - 1.0) - (a + 1.0) * c),
                        a * ((a + 1.0) - (a - 1.0) * c - k),
                        (a + 1.0) + (a - 1.0) * c + k,
                        -2.0 * ((a - 1.0) + (a + 1.0) * c),
                        (a + 1.0) + (a - 1.0) * c - k,
                    );
                }
            }
        }
    }

    fn hochpass(&mut self, f: f64, rate: f64) {
        let w = std::f64::consts::TAU * f / rate;
        let alpha = w.sin() / (2.0 * std::f64::consts::FRAC_1_SQRT_2);
        let c = w.cos();
        self.setzen((1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0, 1.0 + alpha, -2.0 * c, 1.0 - alpha);
    }

    #[inline]
    fn probe(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// Betragsfrequenzgang eines Bandes in dB bei f (für die EQ-Kurve der Oberfläche)
pub fn band_antwort_db(typ: u8, f0: f32, db: f32, q: f32, f: f32) -> f32 {
    let rate = 48_000.0;
    let mut b = Biquad::default();
    b.band(typ, f0 as f64, db as f64, q as f64, rate);
    let w = std::f64::consts::TAU * f as f64 / rate;
    let (c1, s1, c2, s2) = (w.cos(), w.sin(), (2.0 * w).cos(), (2.0 * w).sin());
    let zr = b.b0 as f64 + b.b1 as f64 * c1 + b.b2 as f64 * c2;
    let zi = -(b.b1 as f64 * s1 + b.b2 as f64 * s2);
    let nr = 1.0 + b.a1 as f64 * c1 + b.a2 as f64 * c2;
    let ni = -(b.a1 as f64 * s1 + b.a2 as f64 * s2);
    let betrag = ((zr * zr + zi * zi) / (nr * nr + ni * ni)).sqrt();
    (20.0 * betrag.max(1e-9).log10()) as f32
}

/// Statische Kompressor-Kennlinie (ohne Make-up), wie compCurve() im Songcenter
#[inline]
pub fn komp_kennlinie(ein_db: f32, schwelle: f32, ratio: f32, knee: f32) -> f32 {
    let ratio = ratio.max(1.0);
    if knee > 0.0 && ein_db > schwelle - knee / 2.0 && ein_db < schwelle + knee / 2.0 {
        let x = ein_db - (schwelle - knee / 2.0);
        ein_db + ((1.0 / ratio - 1.0) * x * x) / (2.0 * knee)
    } else if ein_db >= schwelle {
        schwelle + (ein_db - schwelle) / ratio
    } else {
        ein_db
    }
}

/// Nachhallzeit (−60 dB) der Songcenter-Hüllkurve (1 − t/Größe)^Abklingen
pub fn hall_rt60(groesse: f32, abkling: f32) -> f32 {
    (groesse * (1.0 - 0.001f32.powf(1.0 / abkling.max(0.1)))).clamp(0.1, 10.0)
}

struct Allpass {
    puffer: Vec<f32>,
    pos: usize,
    g: f32,
}

impl Allpass {
    #[inline]
    fn probe(&mut self, x: f32) -> f32 {
        let b = self.puffer[self.pos];
        let v = x + b * self.g;
        self.puffer[self.pos] = v;
        self.pos = (self.pos + 1) % self.puffer.len();
        b - v * self.g
    }
}

const FDN: usize = 8;

/// Hall: Diffusion (4 Allpässe) + Feedback-Delay-Netz (8 Leitungen, Hadamard-Matrix,
/// gedämpfte Rückkopplung). Stereo-Ausgang aus unterschiedlichen Leitungen.
pub struct Hall {
    rate: f64,
    linien: Vec<Vec<f32>>,
    pos: [usize; FDN],
    tief: [f32; FDN],
    g: [f32; FDN],
    diff: Vec<Allpass>,
    rt_stand: f32,
}

impl Hall {
    fn neu(rate: f64) -> Self {
        let f = rate / 48_000.0;
        let laengen = [1031usize, 1327, 1523, 1801, 2053, 2381, 2713, 3019];
        let diff = [142usize, 107, 379, 277];
        Hall {
            rate,
            linien: laengen.iter().map(|&l| vec![0.0; ((l as f64 * f) as usize).max(16)]).collect(),
            pos: [0; FDN],
            tief: [0.0; FDN],
            g: [0.0; FDN],
            diff: diff.iter().map(|&l| Allpass { puffer: vec![0.0; ((l as f64 * f) as usize).max(4)], pos: 0, g: 0.6 }).collect(),
            rt_stand: -1.0,
        }
    }

    fn leeren(&mut self) {
        for l in self.linien.iter_mut() {
            l.fill(0.0);
        }
        for a in self.diff.iter_mut() {
            a.puffer.fill(0.0);
        }
        self.tief = [0.0; FDN];
    }

    #[inline]
    fn probe(&mut self, x: f32, rt60: f32) -> (f32, f32) {
        if (rt60 - self.rt_stand).abs() > 1e-3 {
            self.rt_stand = rt60;
            for (i, l) in self.linien.iter().enumerate() {
                // je Leitung so viel Abschwächung, dass nach rt60 −60 dB erreicht sind
                self.g[i] = 10f32.powf(-3.0 * l.len() as f32 / (rt60 * self.rate as f32));
            }
        }
        let mut d = x;
        for a in self.diff.iter_mut() {
            d = a.probe(d);
        }
        let mut o = [0.0f32; FDN];
        for i in 0..FDN {
            let y = self.linien[i][self.pos[i]];
            // leichte Höhendämpfung in der Rückkopplung (natürlicherer Raum)
            self.tief[i] = y * 0.75 + self.tief[i] * 0.25;
            o[i] = self.tief[i];
        }
        // schnelle Hadamard-Transformation (8×8), normiert
        let mut h = o;
        let mut schritt = 1;
        while schritt < FDN {
            for i in (0..FDN).step_by(schritt * 2) {
                for j in i..i + schritt {
                    let (a, b) = (h[j], h[j + schritt]);
                    h[j] = a + b;
                    h[j + schritt] = a - b;
                }
            }
            schritt *= 2;
        }
        let norm = 1.0 / (FDN as f32).sqrt();
        for i in 0..FDN {
            let ein = if i % 2 == 0 { d } else { -d };
            let v = h[i] * norm * self.g[i] + ein * 0.5;
            let l = &mut self.linien[i];
            l[self.pos[i]] = v;
            self.pos[i] = (self.pos[i] + 1) % l.len();
        }
        let links = (o[0] - o[2] + o[4] - o[6]) * 0.35;
        let rechts = (o[1] - o[3] + o[5] - o[7]) * 0.35;
        (links, rechts)
    }
}

/// Bearbeitung eines Eingangs
struct KanalDsp {
    dc_x: f32,
    dc_y: f32,
    tiefsperre: Biquad,
    eq: [Biquad; EQ_BAENDER],
    /// zuletzt eingestellte EQ-Werte (f, g, q je Band) und Tiefsperre – Koeffizienten nur bei Änderung neu
    eq_stand: [[f32; 3]; EQ_BAENDER],
    ts_stand: Option<bool>,
    gr: f32,
    hall: Hall,
    hall_lief: bool,
}

impl KanalDsp {
    fn neu(rate: f64) -> Self {
        KanalDsp {
            dc_x: 0.0,
            dc_y: 0.0,
            tiefsperre: Biquad::default(),
            eq: [Biquad::default(); EQ_BAENDER],
            eq_stand: [[f32::NAN; 3]; EQ_BAENDER],
            ts_stand: None,
            gr: 0.0,
            hall: Hall::neu(rate),
            hall_lief: false,
        }
    }

    fn einstellen(&mut self, fx: &KanalFx, rate: f64) {
        let ts = fx.tiefsperre.load(Relaxed);
        if self.ts_stand != Some(ts) {
            self.ts_stand = Some(ts);
            if ts {
                self.tiefsperre.hochpass(90.0, rate);
            } else {
                self.tiefsperre.durchlass();
            }
        }
        for b in 0..EQ_BAENDER {
            let w = [fx.eq_freq[b].get(), fx.eq_gain[b].get(), fx.eq_q[b].get()];
            if w != self.eq_stand[b] {
                self.eq_stand[b] = w;
                self.eq[b].band(EQ_VORGABE[b].0, w[0] as f64, w[1] as f64, w[2] as f64, rate);
            }
        }
    }
}

pub struct Mischer {
    rate: f64,
    kanaele: Vec<KanalDsp>,
    pub links: Vec<f32>,
    pub rechts: Vec<f32>,
    tmp: Vec<f32>,
    nass_l: Vec<f32>,
    nass_r: Vec<f32>,
}

/// Panorama mit gleichbleibender Mitte: Mitte = beide Seiten voll, ganz links = nur links
#[inline]
pub fn panorama(pan: f32) -> (f32, f32) {
    let p = pan.clamp(-1.0, 1.0);
    let l = (p.max(0.0) * std::f32::consts::FRAC_PI_2).cos();
    let r = ((-p).max(0.0) * std::f32::consts::FRAC_PI_2).cos();
    (l, r)
}

impl Mischer {
    pub fn neu(rate: u32) -> Self {
        Mischer {
            rate: rate as f64,
            // Hallspeicher für alle Eingänge vorab anlegen (keine Speicheranforderung im Audio-Thread)
            kanaele: (0..MAX_KANAELE).map(|_| KanalDsp::neu(rate as f64)).collect(),
            links: Vec::with_capacity(4096),
            rechts: Vec::with_capacity(4096),
            tmp: Vec::with_capacity(4096),
            nass_l: Vec::with_capacity(4096),
            nass_r: Vec::with_capacity(4096),
        }
    }

    pub fn rate_setzen(&mut self, rate: u32) {
        if rate as f64 != self.rate {
            *self = Mischer::neu(rate);
        }
    }

    pub fn beginnen(&mut self, n: usize) {
        for v in [&mut self.links, &mut self.rechts] {
            v.clear();
            v.resize(n, 0.0);
        }
    }

    /// Einen Eingang (Mono-Block) bearbeiten und in die Summe legen
    pub fn kanal(&mut self, c: usize, x: &[f32], g: &Geteilt) {
        if c >= MAX_KANAELE {
            return;
        }
        let r = &g.regler;
        let a = &g.anzeige;
        let n = x.len().min(self.links.len());
        if !r.kanal_an[c].load(Relaxed) {
            // nur Pegel anzeigen
            let spitze = x[..n].iter().fold(0.0f32, |a, &v| a.max(v.abs()));
            a.kanal_pegel[c].max(spitze);
            return;
        }
        let rate = self.rate;
        let fx = &r.fx[c];
        let k = &mut self.kanaele[c];
        k.einstellen(fx, rate);
        let eq = fx.eq_an.load(Relaxed);
        let komp = fx.komp_an.load(Relaxed);
        let (schwelle, ratio, knee) = (fx.schwelle.get(), fx.ratio.get().max(1.0), fx.knee.get().max(0.0));
        let anstieg = (-1.0 / (fx.attack_ms.get().max(0.01) * 0.001 * rate as f32)).exp();
        let abfall = (-1.0 / (fx.release_ms.get().max(1.0) * 0.001 * rate as f32)).exp();
        let makeup = db_zu_faktor(fx.makeup.get());
        let hall = fx.hall_an.load(Relaxed);
        if !hall && k.hall_lief {
            k.hall.leeren();
        }
        k.hall_lief = hall;
        let mix = if hall { fx.hall_mix.get().clamp(0.0, 1.0) } else { 0.0 };
        let rt60 = hall_rt60(fx.hall_groesse.get(), fx.hall_abkling.get());
        let gain = db_zu_faktor(r.kanal_db[c].get());
        let stereo = r.stereo.load(Relaxed);
        let (gl, gr) = if stereo { panorama(r.kanal_pan[c].get()) } else { (1.0, 1.0) };
        let dc_r = 1.0 - std::f32::consts::TAU * 10.0 / rate as f32;

        let (mut sp_ein, mut sp_aus, mut gr_max, mut spitze) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        // Frequenzanzeige im Effektfenster: Signal nach dem EQ mitschreiben
        let analyse = a.spektrum_kanal.load(Relaxed) == c as u32;
        let mut apos = a.spektrum_pos.load(Relaxed) as usize;
        if analyse {
            a.spektrum_rate.store(rate as u32, Relaxed);
        }
        self.tmp.clear();
        self.nass_l.clear();
        self.nass_r.clear();
        for &v in &x[..n] {
            // Gleichanteil-Sperre
            let d = v - k.dc_x + dc_r * k.dc_y;
            k.dc_x = v;
            k.dc_y = d;
            let mut y = k.tiefsperre.probe(d);
            if eq {
                for b in k.eq.iter_mut() {
                    y = b.probe(y);
                }
            }
            if analyse {
                a.spektrum[apos % SPEKTRUM_N].set(y);
                apos += 1;
            }
            if komp {
                sp_ein = sp_ein.max(y.abs());
                let pegel = 20.0 * y.abs().max(1e-6).log10();
                let ziel = (pegel - komp_kennlinie(pegel, schwelle, ratio, knee)).max(0.0);
                let koeff = if ziel > k.gr { anstieg } else { abfall };
                k.gr = koeff * k.gr + (1.0 - koeff) * ziel;
                gr_max = gr_max.max(k.gr);
                y *= db_zu_faktor(-k.gr) * makeup;
                sp_aus = sp_aus.max(y.abs());
            }
            let (mut wl, mut wr) = (0.0, 0.0);
            if hall {
                (wl, wr) = k.hall.probe(y, rt60);
                y *= 1.0 - mix;
            }
            y *= gain;
            spitze = spitze.max(y.abs());
            self.tmp.push(y);
            self.nass_l.push(wl * mix * gain);
            self.nass_r.push(wr * mix * gain);
        }
        a.kanal_pegel[c].max(spitze);
        if analyse {
            a.spektrum_pos.store((apos % SPEKTRUM_N) as u32, Relaxed);
        }
        if komp {
            a.komp_ein[c].max(sp_ein);
            a.komp_gr[c].max(gr_max);
            a.komp_aus[c].max(sp_aus);
        }
        for i in 0..n {
            let y = self.tmp[i];
            if stereo {
                self.links[i] += (y + self.nass_l[i]) * gl;
                self.rechts[i] += (y + self.nass_r[i]) * gr;
            } else {
                let m = y + 0.5 * (self.nass_l[i] + self.nass_r[i]);
                self.links[i] += m;
                self.rechts[i] += m;
            }
        }
    }

    /// Abschluss eines Blocks (Summe liegt in `links`/`rechts`)
    pub fn beenden(&mut self, _g: &Geteilt) {}
}

/// Kanal-Vorlage: alle Effekt-Parameter wie im Songcenter
pub struct Vorlage {
    pub name: &'static str,
    pub tiefsperre: bool,
    /// je Band (Frequenz, Gain dB, Q); Gain überall 0 = EQ aus
    pub eq: [(f32, f32, f32); EQ_BAENDER],
    /// (Threshold, Ratio, Attack ms, Release ms, Knee, Make-up) – None = Kompressor aus
    pub komp: Option<(f32, f32, f32, f32, f32, f32)>,
    /// (Anteil, Größe s, Abklingen) – None = Hall aus
    pub hall: Option<(f32, f32, f32)>,
}

const N: [(f32, f32, f32); EQ_BAENDER] = [(131.0, 0.0, 0.8), (320.0, 0.0, 1.0), (1000.0, 0.0, 1.0), (3600.0, 0.0, 1.0), (8620.0, 0.0, 0.8)];

/// Vorlagen für Stimmen und Instrumente (EQ, Kompressor, Hall)
pub const KANAL_VORLAGEN: [Vorlage; 21] = [
    Vorlage { name: "Neutral (alles aus)", tiefsperre: false, eq: N, komp: None, hall: None },
    Vorlage {
        name: "Stimme männlich",
        tiefsperre: true,
        eq: [(131.0, -2.0, 0.8), (320.0, -2.0, 1.2), (1000.0, 0.0, 1.0), (3600.0, 3.0, 1.0), (8620.0, 2.0, 0.8)],
        komp: Some((-20.0, 3.0, 8.0, 150.0, 10.0, 4.0)),
        hall: Some((0.15, 1.6, 3.0)),
    },
    Vorlage {
        name: "Stimme weiblich",
        tiefsperre: true,
        eq: [(150.0, -3.0, 0.8), (350.0, -1.5, 1.2), (1000.0, 0.0, 1.0), (4200.0, 2.5, 1.0), (10000.0, 3.0, 0.8)],
        komp: Some((-20.0, 3.0, 6.0, 150.0, 10.0, 4.0)),
        hall: Some((0.18, 1.8, 3.0)),
    },
    Vorlage {
        name: "Sprache / Ansage",
        tiefsperre: true,
        eq: [(150.0, -4.0, 0.8), (320.0, -2.0, 1.0), (1000.0, 0.0, 1.0), (2800.0, 3.0, 1.0), (8620.0, 1.0, 0.8)],
        komp: Some((-24.0, 4.0, 5.0, 120.0, 6.0, 6.0)),
        hall: None,
    },
    Vorlage {
        name: "Chor / Klasse singt",
        tiefsperre: true,
        eq: [(131.0, -2.0, 0.8), (320.0, -1.5, 1.0), (1000.0, 0.0, 1.0), (3600.0, 1.5, 1.0), (9500.0, 2.5, 0.8)],
        komp: Some((-24.0, 2.5, 20.0, 300.0, 20.0, 4.0)),
        hall: Some((0.25, 2.4, 2.5)),
    },
    Vorlage {
        name: "Klavier / Flügel",
        tiefsperre: false,
        eq: [(131.0, 1.0, 0.8), (320.0, -2.0, 1.0), (1000.0, 0.0, 1.0), (3600.0, 1.5, 1.0), (8620.0, 2.0, 0.8)],
        komp: Some((-18.0, 2.0, 20.0, 300.0, 20.0, 2.0)),
        hall: Some((0.20, 2.2, 2.5)),
    },
    Vorlage {
        name: "E-Piano / Keyboard",
        tiefsperre: false,
        eq: [(131.0, 0.0, 0.8), (400.0, -1.5, 1.0), (1000.0, 0.0, 1.0), (3000.0, 1.5, 1.0), (8620.0, 1.0, 0.8)],
        komp: Some((-16.0, 2.0, 15.0, 250.0, 15.0, 2.0)),
        hall: Some((0.12, 1.6, 3.0)),
    },
    Vorlage {
        name: "Akustikgitarre",
        tiefsperre: true,
        eq: [(131.0, -3.0, 0.8), (250.0, -2.5, 1.4), (1000.0, -1.0, 1.0), (4000.0, 2.5, 1.0), (10000.0, 2.0, 0.8)],
        komp: Some((-18.0, 3.0, 10.0, 200.0, 15.0, 3.0)),
        hall: Some((0.15, 1.8, 3.0)),
    },
    Vorlage {
        name: "E-Gitarre",
        tiefsperre: true,
        eq: [(131.0, -2.0, 0.8), (400.0, -1.0, 1.0), (1200.0, 2.0, 0.9), (3000.0, 1.5, 1.0), (8620.0, -2.0, 0.8)],
        komp: Some((-14.0, 2.5, 15.0, 200.0, 15.0, 2.0)),
        hall: Some((0.12, 1.4, 3.5)),
    },
    Vorlage {
        name: "E-Bass",
        tiefsperre: false,
        eq: [(80.0, 3.0, 0.8), (250.0, -2.0, 1.2), (800.0, 1.5, 1.0), (2500.0, 1.0, 1.0), (8620.0, -4.0, 0.8)],
        komp: Some((-22.0, 4.0, 15.0, 250.0, 10.0, 5.0)),
        hall: None,
    },
    Vorlage {
        name: "Schlagzeug (Overheads)",
        tiefsperre: true,
        eq: [(131.0, -2.0, 0.8), (400.0, -2.0, 1.2), (1000.0, 0.0, 1.0), (5000.0, 2.0, 1.0), (10000.0, 3.0, 0.8)],
        komp: Some((-18.0, 3.0, 15.0, 150.0, 10.0, 3.0)),
        hall: Some((0.12, 1.4, 3.5)),
    },
    Vorlage {
        name: "Bassdrum",
        tiefsperre: false,
        eq: [(60.0, 4.0, 0.8), (350.0, -5.0, 1.5), (1000.0, 0.0, 1.0), (3500.0, 3.0, 1.5), (8620.0, -3.0, 0.8)],
        komp: Some((-16.0, 4.0, 20.0, 120.0, 6.0, 3.0)),
        hall: None,
    },
    Vorlage {
        name: "Snare",
        tiefsperre: true,
        eq: [(131.0, -3.0, 0.8), (200.0, 2.0, 1.5), (900.0, -2.0, 1.2), (4500.0, 3.0, 1.0), (9000.0, 2.0, 0.8)],
        komp: Some((-18.0, 4.0, 5.0, 120.0, 6.0, 4.0)),
        hall: Some((0.15, 1.4, 3.5)),
    },
    Vorlage {
        name: "Cajón",
        tiefsperre: false,
        eq: [(80.0, 3.0, 0.8), (400.0, -3.0, 1.4), (1000.0, 0.0, 1.0), (4000.0, 3.0, 1.0), (8620.0, 1.0, 0.8)],
        komp: Some((-18.0, 3.0, 10.0, 150.0, 10.0, 3.0)),
        hall: Some((0.10, 1.2, 4.0)),
    },
    Vorlage {
        name: "Geige / Bratsche",
        tiefsperre: true,
        eq: [(131.0, -3.0, 0.8), (400.0, -1.0, 1.0), (1000.0, 0.0, 1.0), (2800.0, -1.0, 1.4), (10000.0, 2.0, 0.8)],
        komp: Some((-24.0, 2.0, 25.0, 350.0, 20.0, 2.0)),
        hall: Some((0.25, 2.6, 2.5)),
    },
    Vorlage {
        name: "Cello / Kontrabass",
        tiefsperre: false,
        eq: [(100.0, 1.5, 0.8), (300.0, -1.0, 1.0), (1000.0, 0.0, 1.0), (3000.0, 1.0, 1.0), (8620.0, -1.0, 0.8)],
        komp: Some((-24.0, 2.0, 30.0, 400.0, 20.0, 2.0)),
        hall: Some((0.22, 2.4, 2.5)),
    },
    Vorlage {
        name: "Querflöte / Blockflöte",
        tiefsperre: true,
        eq: [(150.0, -4.0, 0.8), (320.0, -1.0, 1.0), (1000.0, 1.0, 1.0), (3600.0, -1.0, 1.2), (8620.0, 1.5, 0.8)],
        komp: Some((-24.0, 2.0, 20.0, 300.0, 20.0, 2.0)),
        hall: Some((0.22, 2.2, 2.5)),
    },
    Vorlage {
        name: "Klarinette / Saxophon",
        tiefsperre: true,
        eq: [(131.0, -2.0, 0.8), (320.0, -2.0, 1.2), (1200.0, 1.5, 1.0), (3600.0, 1.0, 1.0), (8620.0, 1.0, 0.8)],
        komp: Some((-20.0, 2.5, 15.0, 250.0, 15.0, 3.0)),
        hall: Some((0.18, 2.0, 2.8)),
    },
    Vorlage {
        name: "Blechbläser",
        tiefsperre: true,
        eq: [(131.0, -1.0, 0.8), (320.0, -1.0, 1.0), (1000.0, 0.0, 1.0), (3000.0, -1.5, 1.5), (8620.0, 1.0, 0.8)],
        komp: Some((-16.0, 2.5, 10.0, 200.0, 15.0, 2.0)),
        hall: Some((0.20, 2.2, 2.5)),
    },
    Vorlage {
        name: "Ukulele / Mandoline",
        tiefsperre: true,
        eq: [(150.0, -3.0, 0.8), (320.0, -1.0, 1.0), (1000.0, 0.0, 1.0), (3600.0, 2.0, 1.0), (10000.0, 2.0, 0.8)],
        komp: Some((-18.0, 3.0, 8.0, 180.0, 12.0, 3.0)),
        hall: Some((0.15, 1.6, 3.0)),
    },
    Vorlage {
        name: "Raummikrofon (ganzer Raum)",
        tiefsperre: true,
        eq: [(131.0, -3.0, 0.8), (320.0, -2.0, 1.0), (1000.0, 0.0, 1.0), (3600.0, 2.0, 1.0), (8620.0, 2.0, 0.8)],
        komp: Some((-26.0, 2.5, 20.0, 300.0, 20.0, 5.0)),
        hall: None,
    },
];

/// Vorlage auf einen Eingang anwenden (Lautstärke, Panorama und An/Aus bleiben)
pub fn vorlage_anwenden(g: &Geteilt, c: usize, v: usize) {
    let Some(v) = KANAL_VORLAGEN.get(v) else { return };
    let fx = &g.regler.fx[c];
    fx.neutral();
    fx.tiefsperre.store(v.tiefsperre, Relaxed);
    let mit_eq = v.eq.iter().any(|b| b.1 != 0.0);
    fx.eq_an.store(mit_eq, Relaxed);
    for (b, &(f, db, q)) in v.eq.iter().enumerate() {
        fx.eq_freq[b].set(f);
        fx.eq_gain[b].set(db);
        fx.eq_q[b].set(q);
    }
    if let Some((th, ra, at, re, kn, mu)) = v.komp {
        fx.komp_an.store(true, Relaxed);
        fx.schwelle.set(th);
        fx.ratio.set(ra);
        fx.attack_ms.set(at);
        fx.release_ms.set(re);
        fx.knee.set(kn);
        fx.makeup.set(mu);
    }
    if let Some((mix, gr, ab)) = v.hall {
        fx.hall_an.store(true, Relaxed);
        fx.hall_mix.set(mix);
        fx.hall_groesse.set(gr);
        fx.hall_abkling.set(ab);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kanal_an(g: &Geteilt) {
        g.regler.kanal_an[0].store(true, Relaxed);
    }

    #[test]
    fn neutral_ist_durchlass() {
        let g = Geteilt::neu();
        kanal_an(&g);
        let mut m = Mischer::neu(48_000);
        let x: Vec<f32> = (0..2048).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
        m.beginnen(x.len());
        m.kanal(0, &x, &g);
        // nach dem Einschwingen der Gleichanteil-Sperre praktisch unverändert und ohne Verzögerung
        for i in 1024..2048 {
            assert!((m.links[i] - x[i]).abs() < 0.02, "i {i}: {} vs {}", m.links[i], x[i]);
        }
    }

    #[test]
    fn eq_wie_web_audio() {
        // Glocke 1 kHz +6 dB: bei 1 kHz genau +6 dB, weit daneben ~0
        assert!((band_antwort_db(1, 1000.0, 6.0, 1.0, 1000.0) - 6.0).abs() < 0.05);
        assert!(band_antwort_db(1, 1000.0, 6.0, 1.0, 50.0).abs() < 0.3);
        // Lowshelf: tief voll, hoch neutral
        assert!((band_antwort_db(0, 131.0, -6.0, 0.8, 30.0) + 6.0).abs() < 0.5);
        assert!(band_antwort_db(0, 131.0, -6.0, 0.8, 5000.0).abs() < 0.2);
        // Highshelf
        assert!((band_antwort_db(2, 8620.0, 4.0, 0.8, 20_000.0) - 4.0).abs() < 0.7);
    }

    #[test]
    fn kennlinie_wie_songcenter() {
        // unter dem Knie unverändert, weit darüber: th + (in − th)/ratio
        assert_eq!(komp_kennlinie(-60.0, -24.0, 4.0, 30.0), -60.0);
        assert!((komp_kennlinie(0.0, -24.0, 4.0, 30.0) - (-18.0)).abs() < 1e-4);
        // stetig am Knie-Ende
        let a = komp_kennlinie(-9.0001, -24.0, 4.0, 30.0);
        let b = komp_kennlinie(-8.9999, -24.0, 4.0, 30.0);
        assert!((a - b).abs() < 0.01);
    }

    #[test]
    fn kompressor_senkt() {
        let g = Geteilt::neu();
        kanal_an(&g);
        let fx = &g.regler.fx[0];
        fx.komp_an.store(true, Relaxed);
        fx.schwelle.set(-20.0);
        fx.ratio.set(4.0);
        fx.knee.set(0.0);
        let mut m = Mischer::neu(48_000);
        let x: Vec<f32> = (0..48_000).map(|i| (i as f32 * 0.06).sin() * 0.9).collect();
        m.beginnen(x.len());
        m.kanal(0, &x, &g);
        let spitze = m.links[24_000..].iter().fold(0.0f32, |a, &v| a.max(v.abs()));
        assert!(spitze < 0.5, "nicht komprimiert: {spitze}");
        assert!(g.anzeige.komp_gr[0].get() > 5.0);
    }

    #[test]
    fn hall_klingt_aus() {
        let g = Geteilt::neu();
        kanal_an(&g);
        let fx = &g.regler.fx[0];
        fx.hall_an.store(true, Relaxed);
        fx.hall_mix.set(0.5);
        fx.hall_groesse.set(1.0);
        fx.hall_abkling.set(2.5);
        g.regler.stereo.store(true, Relaxed);
        let mut m = Mischer::neu(48_000);
        // Impuls, dann 2 s Stille
        let mut x = vec![0.0f32; 96_000];
        x[0] = 1.0;
        m.beginnen(x.len());
        m.kanal(0, &x, &g);
        let energie = |a: usize, b: usize| m.links[a..b].iter().map(|v| v * v).sum::<f32>();
        let frueh = energie(2_000, 12_000);
        let spaet = energie(84_000, 96_000);
        assert!(frueh > 1e-4, "kein Hall: {frueh}");
        assert!(spaet < frueh * 1e-4, "klingt nicht aus: früh {frueh}, spät {spaet}");
        // trockenes Signal sofort (ohne Verzögerung) am Ausgang, abgesenkt um den Mix
        assert!((m.links[0] - 0.5).abs() < 0.05, "trocken: {}", m.links[0]);
        // Rt60 entspricht der Songcenter-Hüllkurve: 1 s × (1 − 0,001^(1/2,5)) ≈ 0,94 s
        assert!((hall_rt60(1.0, 2.5) - 0.937).abs() < 0.01);
    }

    #[test]
    fn vorlagen_gueltig() {
        let g = Geteilt::neu();
        for v in 0..KANAL_VORLAGEN.len() {
            vorlage_anwenden(&g, 0, v);
            let text = crate::gemeinsam::kanal_text(&g, 0);
            crate::gemeinsam::kanal_setzen(&g, 1, &text);
            assert_eq!(crate::gemeinsam::kanal_text(&g, 1), text, "Vorlage {v}");
        }
    }
}
