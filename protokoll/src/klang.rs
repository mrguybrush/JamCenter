//! Metronom-Klänge (kleiner Synthesizer) – gemeinsam für die App (Wiedergabe) und den Server
//! (Metronom in der Aufnahme), damit beide genau gleich klingen.

/// Eine klingende Metronom-Stimme (kleiner Synthesizer, alles berechnet, keine Samples)
#[derive(Clone, Copy)]
pub struct Stimme {
    form: u32,
    betont: bool,
    /// Schlagnummer (Bassdrum/Snare wechseln sich ab)
    schlag: i64,
    n: u32,
    laenge: u32,
    phase: f32,
    rausch: u32,
    bp: [f32; 2],
    hp: f32,
    hp_x: f32,
}

impl Stimme {
    /// klingt noch?
    pub fn klingt(&self) -> bool {
        self.n < self.laenge
    }

    pub fn still() -> Self {
        Stimme { form: 0, betont: false, schlag: 0, n: 0, laenge: 0, phase: 0.0, rausch: 0x1234_5678, bp: [0.0; 2], hp: 0.0, hp_x: 0.0 }
    }

    pub fn start(form: u32, betont: bool, schlag: i64, rate: f64) -> Self {
        let sek = match form {
            1 => 1.2,
            5 => 0.4,
            _ => 0.15,
        };
        Stimme { form, betont, schlag, laenge: (sek * rate) as u32, rausch: 0x9e37_79b9 ^ (schlag as u32), ..Stimme::still() }
    }

    #[inline]
    fn rauschen(&mut self) -> f32 {
        self.rausch ^= self.rausch << 13;
        self.rausch ^= self.rausch >> 17;
        self.rausch ^= self.rausch << 5;
        (self.rausch as f32 / u32::MAX as f32) * 2.0 - 1.0
    }

    /// nächstes Sample (0 nach Ende)
    #[inline]
    pub fn probe(&mut self, rate: f32) -> f32 {
        if self.n >= self.laenge {
            return 0.0;
        }
        let t = self.n as f32 / rate;
        self.n += 1;
        let tau = std::f32::consts::TAU;
        let anstieg = (self.n as f32 / (0.0005 * rate)).min(1.0);
        let b = self.betont;
        let y = match self.form {
            // Glocke: unharmonische Teiltöne, lang ausklingend
            1 => {
                let f0 = if b { 1318.5 } else { 880.0 };
                let teile = [(1.0, 1.0, 0.45), (2.76, 0.5, 0.2), (5.40, 0.25, 0.08), (8.93, 0.12, 0.04)];
                teile.iter().map(|&(v, a, d)| (tau * f0 * v * t).sin() * a * (-t / d).exp()).sum::<f32>() * 0.6
            }
            // Holzblock: kurzer, hohler Ton
            2 => {
                let f = if b { 1250.0 } else { 900.0 };
                (tau * f * t).sin() * (-t / 0.02).exp() + 0.4 * (tau * 2.7 * f * t).sin() * (-t / 0.008).exp()
            }
            // Rimshot: Rauschknall im Bandpass + heller Ton
            3 => {
                let x = self.rauschen();
                // einfacher Resonator um 2 kHz
                let w = tau * 2000.0 / rate;
                let r = 0.97;
                let y = x * (1.0 - r) + 2.0 * r * w.cos() * self.bp[0] - r * r * self.bp[1];
                self.bp[1] = self.bp[0];
                self.bp[0] = y;
                (y * 1.5 * (-t / 0.006).exp() + 0.6 * (tau * 1700.0 * t).sin() * (-t / 0.015).exp()) * if b { 1.0 } else { 0.7 }
            }
            // Kuhglocke: zwei rechteckähnliche Töne
            4 => {
                let eckig = |f: f32| {
                    let p = tau * f * t;
                    p.sin() + (3.0 * p).sin() / 3.0 + (5.0 * p).sin() / 5.0
                };
                (eckig(587.0) + eckig(845.0)) * 0.35 * (-t / if b { 0.12 } else { 0.07 }).exp()
            }
            // schlag 0 = Bassdrum, 1 = Snare (Muster siehe bassdrum())
            5 => {
                if self.schlag % 2 == 0 {
                    let f = 48.0 + 95.0 * (-t / 0.03).exp();
                    self.phase += tau * f / rate;
                    self.phase.sin() * (-t / 0.18).exp() * 1.2
                } else {
                    let x = self.rauschen();
                    (x * 0.55 * (-t / 0.07).exp() + 0.45 * (tau * 190.0 * t).sin() * (-t / 0.05).exp()) * 1.1
                }
            }
            // Hi-Hat: hochpassgefiltertes Rauschen
            6 => {
                let x = self.rauschen();
                let y = x - self.hp_x + 0.6 * self.hp;
                self.hp_x = x;
                self.hp = y;
                y * 0.6 * (-t / if b { 0.06 } else { 0.025 }).exp()
            }
            // Klick: kurzer Sinus
            _ => {
                let f = if b { 1760.0 } else { 1175.0 };
                (tau * f * t).sin() * (-t / 0.012).exp() * if b { 1.0 } else { 0.7 }
            }
        };
        y * anstieg
    }
}

/// Bassdrum oder Snare auf Schlag k? Bassdrum immer auf der Eins, Snare auf den Nachschlägen:
/// 2/4 und 4/4 B S B S, 3/4 B S S, 5/4 B S B S S, 6/8 B S S B S S, 7/8 B S B S B S S,
/// ohne Betonung abwechselnd.
pub fn bassdrum(k: i64, takt: i64) -> bool {
    let i = k % takt.max(1);
    match takt {
        1 => k % 2 == 0,
        3 => i == 0,
        6 => i % 3 == 0,
        t if t % 2 == 1 => i % 2 == 0 && i != t - 1,
        _ => i % 2 == 0,
    }
}

