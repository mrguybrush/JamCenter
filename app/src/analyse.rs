//! Rückkopplungsschutz: erkennt einen anhaltenden reinen Ton, der gleichzeitig im
//! eigenen Mikrofon und im empfangenen Signal dominiert (Schleife Lautsprecher →
//! Mikrofon → anderer Raum → zurück). Dann wird die Lautstärke um 6 dB gesenkt.
//! Einzelne gehaltene Töne (z. B. Blockflöte) lösen nicht aus, solange sie nicht in
//! BEIDEN Richtungen gleich laut und gleich hoch über 1,5 s anliegen.

pub const N: usize = 1024;

pub struct Spektrum {
    fenster: Vec<f32>,
    cos: Vec<f32>,
    sin: Vec<f32>,
    re: Vec<f32>,
    im: Vec<f32>,
    puffer: Vec<f32>,
}

#[derive(Clone, Copy, Debug)]
pub struct Ergebnis {
    pub frequenz: f32,
    /// Anteil der Energie im stärksten Spektralbereich (0..1)
    pub anteil: f32,
    pub pegel_db: f32,
}

impl Spektrum {
    pub fn neu() -> Self {
        let fenster = (0..N).map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / N as f32).cos()).collect();
        let cos = (0..N / 2).map(|i| (std::f32::consts::TAU * i as f32 / N as f32).cos()).collect();
        let sin = (0..N / 2).map(|i| -(std::f32::consts::TAU * i as f32 / N as f32).sin()).collect();
        Spektrum { fenster, cos, sin, re: vec![0.0; N], im: vec![0.0; N], puffer: Vec::with_capacity(N) }
    }

    /// Proben sammeln; liefert alle N Proben ein Ergebnis.
    pub fn zufuehren(&mut self, proben: &[f32], rate: u32, mut f: impl FnMut(Ergebnis)) {
        let mut rest = proben;
        while !rest.is_empty() {
            let n = (N - self.puffer.len()).min(rest.len());
            self.puffer.extend_from_slice(&rest[..n]);
            rest = &rest[n..];
            if self.puffer.len() == N {
                let e = self.berechnen(rate);
                self.puffer.clear();
                f(e);
            }
        }
    }

    fn berechnen(&mut self, rate: u32) -> Ergebnis {
        let mut quadrat = 0.0f32;
        for i in 0..N {
            let x = self.puffer[i];
            quadrat += x * x;
            // Bitumkehr-Reihenfolge
            let j = (i as u32).reverse_bits() >> (32 - N.trailing_zeros());
            self.re[j as usize] = x * self.fenster[i];
            self.im[j as usize] = 0.0;
        }
        let mut laenge = 2;
        while laenge <= N {
            let halb = laenge / 2;
            let stufe = N / laenge;
            for start in (0..N).step_by(laenge) {
                for k in 0..halb {
                    let (wr, wi) = (self.cos[k * stufe], self.sin[k * stufe]);
                    let a = start + k;
                    let b = a + halb;
                    let tr = self.re[b] * wr - self.im[b] * wi;
                    let ti = self.re[b] * wi + self.im[b] * wr;
                    self.re[b] = self.re[a] - tr;
                    self.im[b] = self.im[a] - ti;
                    self.re[a] += tr;
                    self.im[a] += ti;
                }
            }
            laenge *= 2;
        }
        let bin_hz = rate as f32 / N as f32;
        let von = (80.0 / bin_hz).ceil() as usize;
        let bis = ((8000.0 / bin_hz) as usize).min(N / 2 - 2);
        let mut gesamt = 0.0f32;
        let mut best = (0usize, 0.0f32);
        let mag = |re: &[f32], im: &[f32], k: usize| re[k] * re[k] + im[k] * im[k];
        for k in 2..N / 2 {
            let m = mag(&self.re, &self.im, k);
            gesamt += m;
            if k >= von && k <= bis && m > best.1 {
                best = (k, m);
            }
        }
        let k = best.0.max(1);
        let spitze = mag(&self.re, &self.im, k - 1) + best.1 + mag(&self.re, &self.im, k + 1);
        let rms = (quadrat / N as f32).sqrt();
        Ergebnis {
            frequenz: k as f32 * bin_hz,
            anteil: if gesamt > 0.0 { spitze / gesamt } else { 0.0 },
            pegel_db: 20.0 * rms.max(1e-9).log10(),
        }
    }
}

/// Zustand des Rückkopplungsschutzes (läuft im Netz-Thread auf dem empfangenen Signal)
pub struct Waechter {
    dauer_ms: f32,
    frequenz: f32,
}

impl Waechter {
    pub fn neu() -> Self {
        Waechter { dauer_ms: 0.0, frequenz: 0.0 }
    }

    /// `ein` = empfangenes Signal, `mikro` = letzte Analyse des eigenen Mikrofons.
    /// Liefert true, wenn eine Rückkopplung erkannt wurde.
    pub fn pruefen(&mut self, ein: Ergebnis, mikro: Ergebnis, block_ms: f32) -> bool {
        let toleranz = 2.0 * 48_000.0 / N as f32;
        let verdacht = ein.anteil > 0.8
            && ein.pegel_db > -24.0
            && mikro.anteil > 0.5
            && mikro.pegel_db > -30.0
            && (ein.frequenz - mikro.frequenz).abs() < toleranz
            && (self.dauer_ms == 0.0 || (ein.frequenz - self.frequenz).abs() < toleranz);
        if verdacht {
            self.dauer_ms += block_ms;
            self.frequenz = ein.frequenz;
            if self.dauer_ms >= 1500.0 {
                self.dauer_ms = 0.0;
                return true;
            }
        } else {
            self.dauer_ms = 0.0;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sinus_erkannt() {
        let mut s = Spektrum::neu();
        let p: Vec<f32> = (0..N).map(|i| (i as f32 * 1000.0 / 48_000.0 * std::f32::consts::TAU).sin() * 0.5).collect();
        let mut erg = None;
        s.zufuehren(&p, 48_000, |e| erg = Some(e));
        let e = erg.unwrap();
        assert!((e.frequenz - 1000.0).abs() < 50.0, "{e:?}");
        assert!(e.anteil > 0.9, "{e:?}");
        // Rauschen: kein dominanter Ton
        let mut x = 1u32;
        let r: Vec<f32> = (0..N)
            .map(|_| {
                x = x.wrapping_mul(1664525).wrapping_add(1013904223);
                (x >> 8) as f32 / 16_777_216.0 - 0.5
            })
            .collect();
        s.zufuehren(&r, 48_000, |e| erg = Some(e));
        assert!(erg.unwrap().anteil < 0.2, "{:?}", erg);
    }
}

/// Betragsspektrum (Hann-Fenster, Länge = Zweierpotenz), Betrag normiert auf 1 = Vollaussteuerung
pub fn fft_betrag(x: &[f32]) -> Vec<f32> {
    let n = x.len();
    debug_assert!(n.is_power_of_two());
    let mut re: Vec<f32> = x.iter().enumerate().map(|(i, &v)| v * (0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / n as f32).cos())).collect();
    let mut im = vec![0.0f32; n];
    // Bitumkehr
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
        }
    }
    let mut laenge = 2;
    while laenge <= n {
        let w = -std::f32::consts::TAU / laenge as f32;
        for start in (0..n).step_by(laenge) {
            for k in 0..laenge / 2 {
                let (s, c) = (w * k as f32).sin_cos();
                let (a, b) = (start + k, start + k + laenge / 2);
                let tr = re[b] * c - im[b] * s;
                let ti = re[b] * s + im[b] * c;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
            }
        }
        laenge <<= 1;
    }
    // Hann-Fenster: Summe = n/2; Sinus mit Amplitude 1 ergibt Betrag 1
    (0..n / 2).map(|i| (re[i] * re[i] + im[i] * im[i]).sqrt() * 4.0 / n as f32).collect()
}

#[cfg(test)]
mod fft_tests {
    #[test]
    fn sinus_spitze() {
        let n = 4096;
        let x: Vec<f32> = (0..n).map(|i| (std::f32::consts::TAU * 1000.0 * i as f32 / 48_000.0).sin() * 0.5).collect();
        let b = super::fft_betrag(&x);
        let (k, &m) = b.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap();
        assert!((k as f32 * 48_000.0 / n as f32 - 1000.0).abs() < 12.0);
        assert!((m - 0.5).abs() < 0.1, "{m}");
    }
}
