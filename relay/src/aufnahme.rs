//! Aufnahmen auf dem Server.
//!
//! Während einer Aufnahme schickt jede App zusätzlich ihren eigenen Ton (die Mischung, die sie
//! auch verschickt) mit der Serverzeit des Spielens. Der Server legt die Spuren danach
//! zeitrichtig übereinander – so klingt die Aufnahme, als hätten beide Räume ohne jede
//! Netzverzögerung zusammen gespielt. Das Echtzeit-Weiterleiten ist davon nicht betroffen:
//! Die UDP-Threads reichen die Pakete nur in eine Warteschlange, geschrieben, gemischt und
//! nach MP3 umgewandelt wird in eigenen Threads mit normaler Priorität.
//!
//! Ablage: <Ordner>/<Kennung>/<Name>.mp3, Kennung = Hash aus Raumname und Passwort (von der App).
//! Liste und Download über eine einfache TCP-Verbindung (Port AUFNAHME_TCP_PORT).

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// höchstens 3 Stunden je Aufnahme
const MAX_SEKUNDEN: u64 = 3 * 3600;
/// Aufnahmen werden nach 60 Tagen gelöscht
const AUFBEWAHREN: Duration = Duration::from_secs(60 * 24 * 3600);
const ZIEL_RATE: u32 = 48_000;

pub enum Auftrag {
    /// metronom = (Klangform, Metronom zu Beginn), wenn es mit aufgenommen werden soll
    Start { raum: [u8; 32], kennung: [u8; 32], name: String, start_us: u64, metronom: Option<(u8, protokoll::Metronom)>, spur: Option<protokoll::Spur> },
    /// Spur des Raums wurde während der Aufnahme gestartet/gestoppt (Serverzeit der Änderung)
    Spur { raum: [u8; 32], spur: protokoll::Spur, jetzt: u64 },
    /// Metronom wurde während der Aufnahme umgestellt (Serverzeit der Änderung)
    Metronom { raum: [u8; 32], metronom: protokoll::Metronom, jetzt: u64 },
    Ton { raum: [u8; 32], sitzung: u64, seq: u32, zeit_us: u64, rate: u32, kanaele: u8, daten: Vec<u8> },
    Stop { raum: [u8; 32] },
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Ablageordner: <StateDirectory>/aufnahmen (systemd), sonst ./aufnahmen
pub fn basis() -> PathBuf {
    std::env::var_os("STATE_DIRECTORY").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")).join("aufnahmen")
}

/// Aufnahme-Threads dürfen die Echtzeit-Weiterleitung nie ausbremsen
pub fn normale_prioritaet() {
    #[cfg(target_os = "linux")]
    unsafe {
        // musl hat zusätzliche Felder – alles 0 = Priorität 0
        let p: libc::sched_param = std::mem::zeroed();
        libc::sched_setscheduler(0, libc::SCHED_OTHER, &p);
        libc::setpriority(libc::PRIO_PROCESS, 0, 10);
    }
}

struct Spur {
    pfad: PathBuf,
    datei: BufWriter<File>,
    rate: u32,
    kanaele: u8,
    /// geschriebene Frames
    pos: i64,
    seq: Option<u32>,
    /// Pakete, davon vor dem Beginn, zu spät, Lücken; Zeit des ersten Pakets relativ zum Beginn (ms)
    stat: [u32; 4],
    erstes_ms: Option<f64>,
    /// Stichproben (geschriebene Position, Sollposition laut Zeitstempel) für den Taktausgleich
    punkte: Vec<(i64, i64)>,
}

struct Laufend {
    ordner: PathBuf,
    name: String,
    start_us: u64,
    spuren: HashMap<u64, Spur>,
    /// Klangform des Metronoms und Verlauf (Serverzeit der Änderung, Einstellung)
    klang: Option<u8>,
    verlauf: Vec<(u64, protokoll::Metronom)>,
    /// Spur-Verlauf (Serverzeit der Änderung, Zustand)
    spuren_verlauf: Vec<(u64, protokoll::Spur)>,
}

/// Schläge des Metronoms während der Aufnahme: (Zeit ab Beginn in s, betont, Bassdrum).
/// Wie in der App: ein neues Tempo gilt ab seinem Start (nächster Taktanfang), bis dahin läuft
/// das alte weiter; Ausschalten wirkt sofort.
fn metronom_schlaege(verlauf: &[(u64, protokoll::Metronom)], start_us: u64, ende_us: u64) -> Vec<(f64, bool, bool)> {
    let mut schlaege = Vec::new();
    for (j, &(_, m)) in verlauf.iter().enumerate() {
        if !m.an || m.bpm <= 0.0 {
            continue;
        }
        let bis = match verlauf.get(j + 1) {
            Some(&(_, n)) if n.an => n.start_us.max(m.start_us),
            Some(&(t, _)) => t,
            None => ende_us,
        }
        .min(ende_us);
        let schlag = 60e6 / m.bpm as f64;
        let takt = m.takt.max(1) as i64;
        let ab = start_us.max(m.start_us) as f64;
        let mut k = ((ab - m.start_us as f64) / schlag).ceil().max(0.0) as i64;
        loop {
            let tb = m.start_us as f64 + k as f64 * schlag;
            if tb >= bis as f64 {
                break;
            }
            schlaege.push(((tb - start_us as f64) / 1e6, takt > 1 && k % takt == 0, protokoll::klang::bassdrum(k, takt)));
            k += 1;
        }
    }
    schlaege
}

pub fn starten() -> SyncSender<Auftrag> {
    let (tx, rx) = mpsc::sync_channel::<Auftrag>(4096);
    thread::Builder::new().name("aufnahme".into()).spawn(move || schreiber(rx)).expect("Thread");
    thread::Builder::new().name("aufnahme-tcp".into()).spawn(tcp_dienst).expect("Thread");
    tx
}

fn freier_name(ordner: &Path, name: &str) -> String {
    let mut n = name.to_string();
    let mut i = 2;
    while ordner.join(format!("{n}.mp3")).exists() || ordner.join(format!("{n}.wav")).exists() || ordner.join(format!("{n}.laeuft")).exists() {
        n = format!("{name}_{i}");
        i += 1;
    }
    n
}

fn schreiber(rx: Receiver<Auftrag>) {
    normale_prioritaet();
    let mut laufend: HashMap<[u8; 32], Laufend> = HashMap::new();
    let mut letzte_pflege = Instant::now() - Duration::from_secs(7200);
    loop {
        let auftrag = match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(a) => Some(a),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(_) => return,
        };
        if letzte_pflege.elapsed() > Duration::from_secs(3600) {
            letzte_pflege = Instant::now();
            alte_loeschen();
        }
        match auftrag {
            Some(Auftrag::Start { raum, kennung, name, start_us, metronom, spur }) => {
                if laufend.contains_key(&raum) {
                    continue;
                }
                let ordner = basis().join(hex(&kennung));
                if let Err(e) = fs::create_dir_all(&ordner) {
                    eprintln!("Aufnahme: Ordner {}: {e}", ordner.display());
                    continue;
                }
                let name = freier_name(&ordner, &name);
                let _ = File::create(ordner.join(format!("{name}.laeuft")));
                println!("Aufnahme {name} gestartet");
                laufend.insert(
                    raum,
                    Laufend {
                        ordner,
                        name,
                        start_us,
                        spuren: HashMap::new(),
                        klang: metronom.map(|m| m.0),
                        verlauf: metronom.map(|m| vec![(start_us, m.1)]).unwrap_or_default(),
                        spuren_verlauf: spur.map(|s| vec![(start_us, s)]).unwrap_or_default(),
                    },
                );
            }
            Some(Auftrag::Metronom { raum, metronom, jetzt }) => {
                if let Some(l) = laufend.get_mut(&raum) {
                    if l.klang.is_some() {
                        l.verlauf.push((jetzt, metronom));
                    }
                }
            }
            Some(Auftrag::Spur { raum, spur, jetzt }) => {
                if let Some(l) = laufend.get_mut(&raum) {
                    l.spuren_verlauf.push((jetzt, spur));
                }
            }
            Some(Auftrag::Ton { raum, sitzung, seq, zeit_us, rate, kanaele, daten }) => {
                let Some(l) = laufend.get_mut(&raum) else { continue };
                if !l.spuren.contains_key(&sitzung) {
                    let pfad = l.ordner.join(format!(".{}.{sitzung:x}.roh", l.name));
                    let Ok(f) = File::create(&pfad) else { continue };
                    l.spuren.insert(sitzung, Spur { pfad, datei: BufWriter::with_capacity(1 << 18, f), rate, kanaele, pos: 0, seq: None, stat: [0; 4], erstes_ms: None, punkte: Vec::new() });
                }
                let start_us = l.start_us;
                let sp = l.spuren.get_mut(&sitzung).unwrap();
                if sp.rate != rate || sp.kanaele != kanaele {
                    continue;
                }
                ton_einfuegen(sp, start_us, seq, zeit_us, &daten);
                if sp.pos > (MAX_SEKUNDEN * rate as u64) as i64 {
                    if let Some(l) = laufend.remove(&raum) {
                        abschliessen(l);
                    }
                }
            }
            Some(Auftrag::Stop { raum }) => {
                if let Some(l) = laufend.remove(&raum) {
                    abschliessen(l);
                }
            }
            None => {}
        }
    }
}

/// Paket an die richtige Stelle der Spur schreiben (fehlende Stücke = Stille)
fn ton_einfuegen(sp: &mut Spur, start_us: u64, seq: u32, zeit_us: u64, daten: &[u8]) {
    let k = sp.kanaele as usize;
    let mut frames = daten.len() / (2 * k);
    let mut daten = daten;
    if let Some(alt) = sp.seq {
        // älteres (vertauschtes) Paket
        if seq.wrapping_sub(alt) > 0x8000_0000 || seq == alt {
            return;
        }
    }
    let rate = sp.rate as i64;
    sp.stat[0] += 1;
    sp.erstes_ms.get_or_insert((zeit_us as f64 - start_us as f64) / 1000.0);
    let mut soll = ((zeit_us as i64 - start_us as i64) * rate) / 1_000_000;
    if soll < 0 {
        // vor dem Aufnahmebeginn gespielt: vorderen Teil weglassen
        let weg = (-soll) as usize;
        if weg >= frames {
            sp.stat[1] += 1;
            return;
        }
        daten = &daten[weg * 2 * k..];
        frames -= weg;
        soll = 0;
    }
    let fortlaufend = sp.seq.is_some_and(|alt| seq == alt.wrapping_add(1));
    let diff = soll - sp.pos;
    let grenze = rate * 30 / 1000;
    let klein = rate * 2 / 1000;
    // Fortlaufende Pakete immer lückenlos anhängen: Schwankungen der Zeitstempel (Uhrabgleich,
    // Taktabweichung der Soundkarte) gleicht das Mischen über `punkte` aus. Früher wurden Pakete
    // schon ab 30 ms Abweichung verworfen – jede Schnittstelle knisterte.
    let weit = rate * 400 / 1000;
    if fortlaufend && diff.abs() <= weit {
        // anhängen
    } else if diff > grenze || (!fortlaufend && diff > klein) {
        // Lücke (Paketverlust oder Uhrabgleich): mit Stille auffüllen
        let null = vec![0u8; (diff as usize) * 2 * k];
        let _ = sp.datei.write_all(&null);
        sp.pos += diff;
        sp.stat[3] += 1;
    } else if diff < -grenze {
        // deutlich zu spät: weglassen
        sp.stat[2] += 1;
        sp.seq = Some(seq);
        return;
    }
    if sp.stat[0] % 25 == 1 {
        sp.punkte.push((sp.pos, soll));
    }
    let _ = sp.datei.write_all(daten);
    sp.pos += frames as i64;
    sp.seq = Some(seq);
}

/// Taktausgleich einer Spur: Sollposition ≈ a + b · geschriebene Position (Ausgleichsgerade).
/// Unplausible Steigungen (> 2 % Abweichung) werden nicht übernommen, dann nur der Versatz.
fn taktausgleich(punkte: &[(i64, i64)]) -> (f64, f64) {
    if punkte.is_empty() {
        return (0.0, 1.0);
    }
    let n = punkte.len() as f64;
    let mx = punkte.iter().map(|p| p.0 as f64).sum::<f64>() / n;
    let my = punkte.iter().map(|p| p.1 as f64).sum::<f64>() / n;
    let sxx: f64 = punkte.iter().map(|p| (p.0 as f64 - mx).powi(2)).sum();
    let sxy: f64 = punkte.iter().map(|p| (p.0 as f64 - mx) * (p.1 as f64 - my)).sum();
    let b = if punkte.len() >= 8 && sxx > 0.0 { sxy / sxx } else { 1.0 };
    let b = if (b - 1.0).abs() <= 0.02 { b } else { 1.0 };
    (my - b * mx, b)
}

fn abschliessen(l: Laufend) {
    let Laufend { ordner, name, spuren, start_us, klang, verlauf, spuren_verlauf } = l;
    let mut liste = Vec::new();
    for (sitzung, mut sp) in spuren {
        let _ = sp.datei.flush();
        let (a, b) = taktausgleich(&sp.punkte);
        println!(
            "Aufnahme {name}: Spur {sitzung:x} {} Hz {} Kan., {:.1} s, Pakete {}, vor Beginn {}, zu spät {}, Lücken {}, erstes Paket {:+.0} ms, Takt {:.5}, Versatz {:+.1} ms",
            sp.rate,
            sp.kanaele,
            sp.pos as f64 / sp.rate as f64,
            sp.stat[0],
            sp.stat[1],
            sp.stat[2],
            sp.stat[3],
            sp.erstes_ms.unwrap_or(0.0),
            b,
            a * 1000.0 / sp.rate as f64
        );
        liste.push(SpurDatei { pfad: sp.pfad, rate: sp.rate, kanaele: sp.kanaele, frames: sp.pos, a, b, offset: 0, pegel: 1.0, loeschen: true });
    }
    println!("Aufnahme {name} beendet ({} Spuren)", liste.len());
    // Länge der Aufnahme = längste Spur
    let dauer_us = liste.iter().map(|s| (s.ende_sek() * 1e6) as u64).max().unwrap_or(0);
    let metronom = klang.map(|k| (k, metronom_schlaege(&verlauf, start_us, start_us + dauer_us)));
    thread::Builder::new()
        .name("aufnahme-mix".into())
        .spawn(move || {
            normale_prioritaet();
            let marke = ordner.join(format!("{name}.laeuft"));
            let wav = ordner.join(format!("{name}.wav"));
            // Spur(en), die während der Aufnahme liefen, zeitrichtig dazu (ein Weg ohne Netz: so, wie sie erklang)
            let mut liste = liste;
            let ok_spuren = !liste.is_empty() && liste.iter().any(|s| s.frames > 0);
            if ok_spuren {
                liste.extend(spuren_vorbereiten(&ordner, &name, &spuren_verlauf, start_us, start_us + dauer_us));
            }
            let ok = ok_spuren && mischen(&liste, metronom.as_ref(), &wav).is_ok();
            for s in &liste {
                if s.loeschen {
                    let _ = fs::remove_file(&s.pfad);
                }
            }
            if ok {
                let mp3 = ordner.join(format!("{name}.mp3"));
                if mp3_umwandeln(&wav, &mp3) {
                    let _ = fs::remove_file(&wav);
                } else {
                    let _ = fs::remove_file(&mp3);
                }
            }
            let _ = fs::remove_file(marke);
        })
        .ok();
}

/// Rohdatei einer Spur mit Taktausgleich (Sollposition = a + b · Position)
struct SpurDatei {
    pfad: PathBuf,
    rate: u32,
    kanaele: u8,
    /// Frames ab Dateibeginn, die gelesen werden (Ende der Spur)
    frames: i64,
    a: f64,
    b: f64,
    /// Beginn der Proben in der Datei (WAV-Kopf)
    offset: u64,
    pegel: f32,
    /// Hilfsdatei, nach dem Mischen löschen
    loeschen: bool,
}

/// Kopf einer 16-Bit-PCM-WAV lesen: (Rate, Kanäle, Beginn der Daten, Zahl der Frames)
fn wav_lesen(pfad: &Path) -> Option<(u32, u8, u64, i64)> {
    let mut f = File::open(pfad).ok()?;
    let mut kopf = [0u8; 12];
    f.read_exact(&mut kopf).ok()?;
    if &kopf[0..4] != b"RIFF" || &kopf[8..12] != b"WAVE" {
        return None;
    }
    let mut fmt: Option<(u32, u8)> = None;
    let mut pos = 12u64;
    loop {
        let mut c = [0u8; 8];
        f.read_exact(&mut c).ok()?;
        let laenge = u32::from_le_bytes(c[4..8].try_into().ok()?) as u64;
        pos += 8;
        if &c[0..4] == b"fmt " {
            let mut d = vec![0u8; laenge.min(64) as usize];
            f.read_exact(&mut d).ok()?;
            let art = u16::from_le_bytes([d[0], d[1]]);
            let kanaele = u16::from_le_bytes([d[2], d[3]]);
            let rate = u32::from_le_bytes(d[4..8].try_into().ok()?);
            let bits = u16::from_le_bytes([d[14], d[15]]);
            if !(art == 1 || art == 0xFFFE) || bits != 16 || !(1..=2).contains(&kanaele) || !(8000..=192_000).contains(&rate) {
                return None;
            }
            fmt = Some((rate, kanaele as u8));
            f.seek(SeekFrom::Start(pos + laenge + (laenge & 1))).ok()?;
        } else if &c[0..4] == b"data" {
            let (rate, kanaele) = fmt?;
            // lame --decode schreibt bei Datenströmen teils 0 oder eine zu große Länge
            let datei = f.metadata().ok()?.len();
            let n = if laenge == 0 || pos + laenge > datei { datei - pos } else { laenge };
            return Some((rate, kanaele, pos, (n / (2 * kanaele as u64)) as i64));
        } else {
            f.seek(SeekFrom::Start(pos + laenge + (laenge & 1))).ok()?;
        }
        pos += laenge + (laenge & 1);
    }
}

/// Spur-Abschnitte der Aufnahme als Mischquellen: MP3 wird mit lame zu WAV entpackt.
/// Pegel −3 dB, damit Mikrofone und Spur ungefähr im Verhältnis bleiben.
fn spuren_vorbereiten(ordner: &Path, name: &str, verlauf: &[(u64, protokoll::Spur)], start_us: u64, ende_us: u64) -> Vec<SpurDatei> {
    let mut liste = Vec::new();
    let mut entpackt: HashMap<String, Option<(PathBuf, bool)>> = HashMap::new();
    for (j, (_, s)) in verlauf.iter().enumerate() {
        if !s.an {
            continue;
        }
        let bis = verlauf.get(j + 1).map(|v| v.0).unwrap_or(ende_us).min(ende_us);
        if bis <= start_us.max(s.start_us) {
            continue;
        }
        let quelle = basis().join(hex(&s.kennung)).join(&s.datei);
        let datei = entpackt
            .entry(s.datei.clone())
            .or_insert_with(|| {
                if s.datei.ends_with(".wav") {
                    Some((quelle.clone(), false))
                } else {
                    let ziel = ordner.join(format!(".{name}.spur{}.wav", liste.len()));
                    mp3_entpacken(&quelle, &ziel).then_some((ziel, true))
                }
            })
            .clone();
        let Some((pfad, loeschen)) = datei else { continue };
        let Some((rate, kanaele, offset, frames)) = wav_lesen(&pfad) else { continue };
        // Position 0 der Datei erklang zur Serverzeit s.start_us
        let a = (s.start_us as f64 - start_us as f64) * rate as f64 / 1e6;
        let laenge = ((bis - s.start_us.min(bis)) as f64 * rate as f64 / 1e6) as i64;
        liste.push(SpurDatei { pfad, rate, kanaele, frames: frames.min(laenge), a, b: 1.0, offset, pegel: 0.7, loeschen });
    }
    // jede entpackte Hilfsdatei nur einmal löschen
    let mut gesehen = std::collections::HashSet::new();
    for s in liste.iter_mut() {
        if s.loeschen && !gesehen.insert(s.pfad.clone()) {
            s.loeschen = false;
        }
    }
    liste
}

fn mp3_entpacken(mp3: &Path, wav: &Path) -> bool {
    let args = ["--quiet", "--decode"];
    let mit_chrt = std::process::Command::new("chrt").args(["-o", "0", "nice", "-n", "19", "lame"]).args(args).arg(mp3).arg(wav).status();
    match mit_chrt {
        Ok(s) if s.success() => true,
        _ => std::process::Command::new("lame").args(args).arg(mp3).arg(wav).status().is_ok_and(|s| s.success()),
    }
}

impl SpurDatei {
    /// Ende der Spur in Sekunden ab Aufnahmebeginn
    fn ende_sek(&self) -> f64 {
        ((self.a + self.b * self.frames as f64) / self.rate as f64).max(0.0)
    }
}

/// Lese eine Spur Frame für Frame als Stereo bei 48 kHz (lineare Umrechnung, mit Taktausgleich)
struct SpurLeser {
    r: BufReader<File>,
    kanaele: usize,
    schritt: f64,
    pos: f64,
    a: [f32; 2],
    b: [f32; 2],
    ende: bool,
    /// Stille vor dem ersten Frame (Ausgabe-Frames)
    vorlauf: u64,
    /// noch zu lesende Frames
    rest: i64,
    pegel: f32,
}

impl SpurLeser {
    fn neu(s: &SpurDatei) -> std::io::Result<Self> {
        let mut datei = File::open(&s.pfad)?;
        datei.seek(SeekFrom::Start(s.offset))?;
        let mut l = SpurLeser {
            r: BufReader::with_capacity(1 << 18, datei),
            kanaele: s.kanaele as usize,
            schritt: s.rate as f64 / ZIEL_RATE as f64 / s.b,
            pos: 0.0,
            a: [0.0; 2],
            b: [0.0; 2],
            ende: false,
            vorlauf: 0,
            rest: s.frames,
            pegel: s.pegel,
        };
        // Position 0 gehört zur Zeit a (Frames der Spur): später -> Stille davor, früher -> Anfang überspringen
        let start = s.a / s.b;
        if start > 0.0 {
            l.vorlauf = (start * ZIEL_RATE as f64 / s.rate as f64) as u64;
        } else {
            let mut weg = (-start) as u64;
            let mut buf = [0u8; 4];
            while weg > 0 && l.r.read_exact(&mut buf[..2 * l.kanaele]).is_ok() {
                weg -= 1;
                l.rest -= 1;
            }
        }
        l.a = l.frame();
        l.b = l.frame();
        Ok(l)
    }

    fn frame(&mut self) -> [f32; 2] {
        let mut buf = [0u8; 4];
        let n = 2 * self.kanaele;
        self.rest -= 1;
        if self.rest < 0 || self.r.read_exact(&mut buf[..n]).is_err() {
            self.ende = true;
            return [0.0; 2];
        }
        let l = i16::from_le_bytes([buf[0], buf[1]]) as f32 / 32768.0 * self.pegel;
        let r = if self.kanaele == 2 { i16::from_le_bytes([buf[2], buf[3]]) as f32 / 32768.0 * self.pegel } else { l };
        [l, r]
    }

    fn naechstes(&mut self) -> [f32; 2] {
        if self.vorlauf > 0 {
            self.vorlauf -= 1;
            return [0.0; 2];
        }
        let t = self.pos as f32;
        let y = [self.a[0] + (self.b[0] - self.a[0]) * t, self.a[1] + (self.b[1] - self.a[1]) * t];
        self.pos += self.schritt;
        while self.pos >= 1.0 {
            self.pos -= 1.0;
            self.a = self.b;
            self.b = self.frame();
        }
        y
    }
}

fn wav_kopf(w: &mut impl Write, frames: u64) -> std::io::Result<()> {
    let daten = (frames * 4).min(u32::MAX as u64 - 36) as u32;
    w.write_all(b"RIFF")?;
    w.write_all(&(36 + daten).to_le_bytes())?;
    w.write_all(b"WAVEfmt ")?;
    w.write_all(&16u32.to_le_bytes())?;
    w.write_all(&1u16.to_le_bytes())?;
    w.write_all(&2u16.to_le_bytes())?;
    w.write_all(&ZIEL_RATE.to_le_bytes())?;
    w.write_all(&(ZIEL_RATE * 4).to_le_bytes())?;
    w.write_all(&4u16.to_le_bytes())?;
    w.write_all(&16u16.to_le_bytes())?;
    w.write_all(b"data")?;
    w.write_all(&daten.to_le_bytes())
}

/// Alle Spuren zeitrichtig zu einer Stereo-WAV (48 kHz) zusammenmischen, optional mit Metronom
fn mischen(spuren: &[SpurDatei], metronom: Option<&(u8, Vec<(f64, bool, bool)>)>, ziel: &Path) -> std::io::Result<()> {
    let laenge = spuren.iter().map(|s| (s.ende_sek() * ZIEL_RATE as f64) as u64).max().unwrap_or(0);
    let mut leser: Vec<SpurLeser> = spuren.iter().filter_map(|s| SpurLeser::neu(s).ok()).collect();
    let mut w = BufWriter::with_capacity(1 << 18, File::create(ziel)?);
    wav_kopf(&mut w, laenge)?;
    let mut stimmen = [protokoll::klang::Stimme::still(); 3];
    let mut naechste = 0;
    let mut schlag = 0;
    for i in 0..laenge {
        let mut m = [0.0f32; 2];
        if let Some((form, schlaege)) = metronom {
            while schlag < schlaege.len() && (schlaege[schlag].0 * ZIEL_RATE as f64) as u64 <= i {
                let (_, betont, bd) = schlaege[schlag];
                stimmen[naechste] = protokoll::klang::Stimme::start(*form as u32, betont, if bd { 0 } else { 1 }, ZIEL_RATE as f64);
                naechste = (naechste + 1) % stimmen.len();
                schlag += 1;
            }
            let k: f32 = stimmen.iter_mut().map(|s| s.probe(ZIEL_RATE as f32)).sum::<f32>() * 0.35;
            m[0] += k;
            m[1] += k;
        }
        for l in leser.iter_mut() {
            if !l.ende {
                let y = l.naechstes();
                m[0] += y[0];
                m[1] += y[1];
            }
        }
        for x in m {
            // sanft begrenzen statt hart abschneiden
            let y = if x.abs() <= 0.9 { x } else { x.signum() * (0.9 + 0.1 * ((x.abs() - 0.9) / 0.1).tanh()) };
            w.write_all(&((y * 32767.0).round() as i16).to_le_bytes())?;
        }
    }
    w.flush()
}

/// MP3 mit lame (niedrige Priorität); ohne lame bleibt die WAV-Datei
fn mp3_umwandeln(wav: &Path, mp3: &Path) -> bool {
    let args = ["--quiet", "-b", "192"];
    let mit_chrt = std::process::Command::new("chrt")
        .args(["-o", "0", "nice", "-n", "19", "lame"])
        .args(args)
        .arg(wav)
        .arg(mp3)
        .status();
    match mit_chrt {
        Ok(s) if s.success() => true,
        _ => std::process::Command::new("lame").args(args).arg(wav).arg(mp3).status().is_ok_and(|s| s.success()),
    }
}

fn alte_loeschen() {
    let Ok(ordner) = fs::read_dir(basis()) else { return };
    for o in ordner.flatten() {
        let Ok(dateien) = fs::read_dir(o.path()) else { continue };
        for d in dateien.flatten() {
            let alt = d.metadata().and_then(|m| m.modified()).ok().and_then(|t| SystemTime::now().duration_since(t).ok());
            if alt.is_some_and(|a| a > AUFBEWAHREN) {
                let _ = fs::remove_file(d.path());
            }
        }
    }
}

// ---------------------------------------------------------------- Liste und Download (TCP)

fn kennung_ok(k: &str) -> bool {
    k.len() == 64 && k.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn datei_ok(d: &str) -> bool {
    !d.is_empty() && d.len() < 80 && !d.starts_with('.') && d.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.') && (d.ends_with(".mp3") || d.ends_with(".wav"))
}

fn tcp_dienst() {
    normale_prioritaet();
    let lauscher = match TcpListener::bind(("0.0.0.0", protokoll::AUFNAHME_TCP_PORT)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("TCP {} nicht verfügbar (Aufnahmen-Download): {e}", protokoll::AUFNAHME_TCP_PORT);
            return;
        }
    };
    println!("Aufnahmen-Download auf TCP {}", protokoll::AUFNAHME_TCP_PORT);
    for v in lauscher.incoming().flatten() {
        let _ = thread::Builder::new().name("aufnahme-holen".into()).spawn(move || {
            let _ = anfrage(v);
        });
    }
}

/// höchstens so groß darf eine hochgeladene Datei sein
const MAX_HOCHLADEN: u64 = 300_000_000;

/// Eine Zeile byteweise lesen (danach folgen beim Hochladen gleich die Daten – nichts vorauslesen)
fn zeile_lesen(v: &mut TcpStream) -> std::io::Result<String> {
    let mut z = Vec::new();
    let mut b = [0u8; 1];
    while z.len() < 400 {
        if v.read(&mut b)? == 0 || b[0] == b'\n' {
            break;
        }
        z.push(b[0]);
    }
    Ok(String::from_utf8_lossy(&z).into_owned())
}

/// Zeilen: „LISTE <kennung>“, „HOLE <kennung> <datei>“, „LOESCHEN <kennung> <datei>“,
/// „HOCH <kennung> <datei> <länge>“ + Daten (Spur hochladen)
fn anfrage(mut v: TcpStream) -> std::io::Result<()> {
    v.set_read_timeout(Some(Duration::from_secs(20)))?;
    v.set_write_timeout(Some(Duration::from_secs(60)))?;
    let zeile = zeile_lesen(&mut v)?;
    let teile: Vec<&str> = zeile.split_whitespace().collect();
    let antwort = |v: &mut TcpStream, t: &str| v.write_all(t.as_bytes());
    match teile.as_slice() {
        ["JAMCENTER", "1", "LISTE", k] if kennung_ok(k) => {
            let mut zeilen = Vec::new();
            if let Ok(d) = fs::read_dir(basis().join(k)) {
                for e in d.flatten() {
                    let n = e.file_name().to_string_lossy().into_owned();
                    let groesse = e.metadata().map(|m| m.len()).unwrap_or(0);
                    if let Some(name) = n.strip_suffix(".laeuft") {
                        zeilen.push(format!("{name}\t0\tumwandeln"));
                    } else if datei_ok(&n) {
                        zeilen.push(format!("{n}\t{groesse}\tfertig"));
                    }
                }
            }
            zeilen.sort();
            zeilen.reverse();
            let mut t = String::from("OK\n");
            for z in zeilen {
                t.push_str(&z);
                t.push('\n');
            }
            antwort(&mut v, &t)
        }
        ["JAMCENTER", "1", "HOLE", k, d] if kennung_ok(k) && datei_ok(d) => {
            let pfad = basis().join(k).join(d);
            match File::open(&pfad) {
                Ok(mut f) => {
                    let len = f.seek(SeekFrom::End(0))?;
                    f.seek(SeekFrom::Start(0))?;
                    antwort(&mut v, &format!("OK {len}\n"))?;
                    std::io::copy(&mut f, &mut v)?;
                    Ok(())
                }
                Err(_) => antwort(&mut v, "FEHLER Datei nicht gefunden\n"),
            }
        }
        ["JAMCENTER", "1", "LOESCHEN", k, d] if kennung_ok(k) && datei_ok(d) => match fs::remove_file(basis().join(k).join(d)) {
            Ok(()) => antwort(&mut v, "OK\n"),
            Err(_) => antwort(&mut v, "FEHLER Datei nicht gefunden\n"),
        },
        ["JAMCENTER", "1", "HOCH", k, d, n] if kennung_ok(k) && datei_ok(d) => {
            let Ok(laenge) = n.parse::<u64>() else { return antwort(&mut v, "FEHLER Länge fehlt\n") };
            if laenge > MAX_HOCHLADEN {
                return antwort(&mut v, "FEHLER Datei zu groß (höchstens 300 MB)\n");
            }
            let ordner = basis().join(k);
            fs::create_dir_all(&ordner)?;
            let ziel = ordner.join(d);
            if ziel.exists() {
                return antwort(&mut v, "FEHLER Eine Datei mit diesem Namen gibt es schon\n");
            }
            let teil = ordner.join(format!(".{d}.hoch"));
            let mut f = BufWriter::new(File::create(&teil)?);
            let kopiert = std::io::copy(&mut (&mut v).take(laenge), &mut f)?;
            f.flush()?;
            drop(f);
            if kopiert != laenge {
                let _ = fs::remove_file(&teil);
                return antwort(&mut v, "FEHLER Übertragung abgebrochen\n");
            }
            // WAV muss 16-Bit-PCM sein (so mischt sie die Aufnahme)
            if d.ends_with(".wav") && wav_lesen(&teil).is_none() {
                let _ = fs::remove_file(&teil);
                return antwort(&mut v, "FEHLER WAV muss 16 Bit PCM sein\n");
            }
            fs::rename(&teil, &ziel)?;
            println!("Spur hochgeladen: {d} ({laenge} Bytes)");
            antwort(&mut v, "OK\n")
        }
        _ => antwort(&mut v, "FEHLER Anfrage unbekannt\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spur(pfad: &Path, rate: u32) -> Spur {
        Spur { pfad: pfad.into(), datei: BufWriter::new(File::create(pfad).unwrap()), rate, kanaele: 1, pos: 0, seq: None, stat: [0; 4], erstes_ms: None, punkte: Vec::new() }
    }

    fn paket(n: usize, wert: i16) -> Vec<u8> {
        (0..n).flat_map(|_| wert.to_le_bytes()).collect()
    }

    #[test]
    fn zeitrichtig_und_luecken() {
        let d = std::env::temp_dir().join("jc_aufnahme_test");
        let _ = fs::create_dir_all(&d);
        let mut s = spur(&d.join("a.roh"), 48_000);
        // Beginn 10 ms nach Aufnahmestart -> 480 Frames Stille davor
        ton_einfuegen(&mut s, 1_000_000, 1, 1_010_000, &paket(480, 100));
        assert_eq!(s.pos, 960);
        // fortlaufend mit 1 ms Abweichung -> direkt angehängt
        ton_einfuegen(&mut s, 1_000_000, 2, 1_021_000, &paket(480, 100));
        assert_eq!(s.pos, 1440);
        // ein Paket verloren (seq 3 fehlt) -> Lücke mit Stille
        ton_einfuegen(&mut s, 1_000_000, 4, 1_040_000, &paket(480, 100));
        assert_eq!(s.pos, 1920 + 480);
        // altes Paket kommt zu spät -> verworfen
        ton_einfuegen(&mut s, 1_000_000, 3, 1_030_000, &paket(480, 100));
        assert_eq!(s.pos, 2400);
        drop(s);
        // Mischen zweier Spuren mit verschiedenen Raten
        let mut b = spur(&d.join("b.roh"), 44_100);
        ton_einfuegen(&mut b, 0, 1, 0, &paket(4410, 1000));
        drop(b);
        let wav = d.join("x.wav");
        let schlaege = metronom_schlaege(&[(0, protokoll::Metronom { an: true, bpm: 120.0, takt: 4, start_us: 0, stand: 1 })], 0, 1_000_000);
        assert_eq!(schlaege.len(), 2);
        let sd = |p: &str, rate, frames| SpurDatei { pfad: d.join(p), rate, kanaele: 1, frames, a: 0.0, b: 1.0, offset: 0, pegel: 1.0, loeschen: true };
        mischen(&[sd("a.roh", 48_000, 2400), sd("b.roh", 44_100, 4410)], Some(&(0, schlaege)), &wav).unwrap();
        let laenge = fs::metadata(&wav).unwrap().len();
        assert_eq!(laenge, 44 + 4800 * 4);
        let _ = fs::remove_dir_all(&d);
    }

    /// Zeitstempel schwanken um ±25 ms und die Soundkarte läuft 0,5 % schneller:
    /// nichts wird verworfen, der Ausgleich findet Takt und Versatz
    #[test]
    fn schwankende_zeitstempel() {
        let d = std::env::temp_dir().join("jc_aufnahme_test2");
        let _ = fs::create_dir_all(&d);
        let mut s = spur(&d.join("c.roh"), 48_000);
        let mut z = 12345u32;
        for i in 0..20000u32 {
            z = z.wrapping_mul(1103515245).wrapping_add(12345);
            let wackeln = (z >> 16) as i64 % 50_000 - 25_000;
            let soll_us = 30_000 + (i as f64 * 144.0 / 48_000.0 * 1e6 / 1.005) as i64 + wackeln;
            ton_einfuegen(&mut s, 0, i + 1, soll_us as u64, &paket(144, 10));
        }
        assert_eq!(s.stat[2], 0, "nichts verworfen");
        let (a, b) = taktausgleich(&s.punkte);
        assert!((b - 1.0 / 1.005).abs() < 0.002, "Takt {b}");
        // erstes Paket wurde an seiner Sollstelle (ca. 30 ms ± 25 ms) begonnen
        assert!(s.pos >= 20000 * 144 + 200 && s.pos <= 20000 * 144 + 2700, "pos {}", s.pos);
        assert!(a.abs() < 48.0 * 30.0, "Versatz {a}");
        let _ = fs::remove_dir_all(&d);
    }

    /// Spur (WAV 44,1 kHz Stereo) beginnt 0,5 s nach Aufnahmebeginn und wird nach 0,3 s gestoppt
    #[test]
    fn spur_in_aufnahme() {
        let d = std::env::temp_dir().join("jc_aufnahme_test3");
        let _ = fs::create_dir_all(&d);
        let wav = d.join("spur.wav");
        {
            let mut w = BufWriter::new(File::create(&wav).unwrap());
            // Kopf schreibt 48 kHz – für den Test auf 44,1 kHz umschreiben
            wav_kopf(&mut w, 44_100).unwrap();
            for _ in 0..44_100 {
                w.write_all(&8000i16.to_le_bytes()).unwrap();
                w.write_all(&8000i16.to_le_bytes()).unwrap();
            }
        }
        let mut b = fs::read(&wav).unwrap();
        b[24..28].copy_from_slice(&44_100u32.to_le_bytes());
        b[28..32].copy_from_slice(&(44_100u32 * 4).to_le_bytes());
        fs::write(&wav, &b).unwrap();
        let (rate, kanaele, offset, frames) = wav_lesen(&wav).unwrap();
        assert_eq!((rate, kanaele, offset, frames), (44_100, 2, 44, 44_100));
        // Mikrofon-Spur 1 s Stille
        let mut m = spur(&d.join("m.roh"), 48_000);
        ton_einfuegen(&mut m, 0, 1, 0, &paket(48_000, 0));
        drop(m);
        let laenge = (0.3 * rate as f64) as i64;
        let liste = [
            SpurDatei { pfad: d.join("m.roh"), rate: 48_000, kanaele: 1, frames: 48_000, a: 0.0, b: 1.0, offset: 0, pegel: 1.0, loeschen: true },
            SpurDatei { pfad: wav.clone(), rate, kanaele, frames: frames.min(laenge), a: 0.5 * rate as f64, b: 1.0, offset, pegel: 0.7, loeschen: false },
        ];
        let ziel = d.join("x.wav");
        mischen(&liste, None, &ziel).unwrap();
        let x: Vec<i16> = fs::read(&ziel).unwrap()[44..].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
        let bei = |sek: f64| x[(sek * 48_000.0) as usize * 2];
        assert_eq!(bei(0.45), 0);
        assert!((bei(0.6) as i32 - 5600).abs() < 30, "{}", bei(0.6));
        assert_eq!(bei(0.85), 0);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn metronom_verlauf() {
        let m = |an, bpm, start_us| protokoll::Metronom { an, bpm, takt: 4, start_us, stand: 1 };
        // 60 bpm ab 0; ab 4 s (nächster Takt) 120 bpm; bei 6 s aus
        let v = vec![(0, m(true, 60.0, 0)), (2_500_000, m(true, 120.0, 4_000_000)), (6_000_000, m(false, 120.0, 4_000_000))];
        let s = metronom_schlaege(&v, 1_000_000, 10_000_000);
        let zeiten: Vec<f64> = s.iter().map(|x| x.0).collect();
        // relativ zum Beginn (1 s): 0,1,2 (60 bpm bis 4 s), dann 3; 3,5; 4; 4,5 (120 bpm bis 6 s)
        assert_eq!(zeiten, vec![0.0, 1.0, 2.0, 3.0, 3.5, 4.0, 4.5]);
        assert!(s[3].1, "Schlag bei 4 s ist die Eins");
    }

    #[test]
    fn dateinamen_pruefen() {
        assert!(datei_ok("2026_12_01_1538.mp3"));
        assert!(!datei_ok("../x.mp3"));
        assert!(!datei_ok(".2026.roh"));
        assert!(!datei_ok("a.txt"));
        assert!(kennung_ok(&"a".repeat(64)));
        assert!(!kennung_ok("../.."));
    }
}
