//! Zustand, den GUI-, Netz- und Audio-Threads teilen (nur Atomics im Echtzeitpfad).

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

/// Höchstzahl Eingangskanäle im Mischpult
pub const MAX_KANAELE: usize = 64;
/// Länge des Rings für die Frequenzanzeige im Equalizer
pub const SPEKTRUM_N: usize = 4096;

/// Gemeinsame Uhr aller Threads in µs (Bezug: erster Aufruf)
pub fn uhr_us() -> u64 {
    static BEGINN: OnceLock<std::time::Instant> = OnceLock::new();
    BEGINN.get_or_init(std::time::Instant::now).elapsed().as_micros() as u64
}

/// Metronom des Raums, wie der Server es vorgibt (Zeiten in Serveruhr-µs)
pub struct MetronomZustand {
    pub an: AtomicBool,
    pub bpm: AtomicF32,
    pub takt: AtomicU32,
    pub start_us: AtomicU64,
    pub stand: AtomicU32,
}

pub struct AtomicF32(AtomicU32);

impl AtomicF32 {
    pub const fn neu(v: f32) -> Self {
        AtomicF32(AtomicU32::new(v.to_bits()))
    }
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Relaxed))
    }
    pub fn set(&self, v: f32) {
        self.0.store(v.to_bits(), Relaxed)
    }
    /// Spitzenwert merken (für Pegelanzeigen)
    pub fn max(&self, v: f32) {
        let mut alt = self.0.load(Relaxed);
        while v > f32::from_bits(alt) {
            match self.0.compare_exchange_weak(alt, v.to_bits(), Relaxed, Relaxed) {
                Ok(_) => break,
                Err(x) => alt = x,
            }
        }
    }
    /// Wert holen und auf 0 setzen
    pub fn nehmen(&self) -> f32 {
        f32::from_bits(self.0.swap(0, Relaxed))
    }
}

/// EQ-Bänder wie im Songcenter: Lowshelf, 3 × Glocke, Highshelf
pub const EQ_BAENDER: usize = 5;
/// (Typ 0 = Lowshelf, 1 = Glocke, 2 = Highshelf; Vorgabe-Frequenz; Vorgabe-Q)
pub const EQ_VORGABE: [(u8, f32, f32); EQ_BAENDER] = [(0, 131.0, 0.8), (1, 320.0, 1.0), (1, 1000.0, 1.0), (1, 3600.0, 1.0), (2, 8620.0, 0.8)];

/// Effekt-Einstellungen eines Eingangs (wie Songcenter: EQ / Kompressor / Hall, plus Trittschallfilter)
pub struct KanalFx {
    pub tiefsperre: AtomicBool,
    pub eq_an: AtomicBool,
    pub eq_freq: [AtomicF32; EQ_BAENDER],
    pub eq_gain: [AtomicF32; EQ_BAENDER],
    pub eq_q: [AtomicF32; EQ_BAENDER],
    pub komp_an: AtomicBool,
    pub schwelle: AtomicF32,
    pub ratio: AtomicF32,
    pub attack_ms: AtomicF32,
    pub release_ms: AtomicF32,
    pub knee: AtomicF32,
    pub makeup: AtomicF32,
    pub hall_an: AtomicBool,
    /// Anteil 0..1 (trocken = 1 − Anteil, wie im Songcenter)
    pub hall_mix: AtomicF32,
    /// Länge in Sekunden
    pub hall_groesse: AtomicF32,
    /// Abklingen (Exponent der Hüllkurve)
    pub hall_abkling: AtomicF32,
}

impl KanalFx {
    pub const fn neu() -> Self {
        KanalFx {
            tiefsperre: AtomicBool::new(false),
            eq_an: AtomicBool::new(false),
            eq_freq: [AtomicF32::neu(131.0), AtomicF32::neu(320.0), AtomicF32::neu(1000.0), AtomicF32::neu(3600.0), AtomicF32::neu(8620.0)],
            eq_gain: [AtomicF32::neu(0.0), AtomicF32::neu(0.0), AtomicF32::neu(0.0), AtomicF32::neu(0.0), AtomicF32::neu(0.0)],
            eq_q: [AtomicF32::neu(0.8), AtomicF32::neu(1.0), AtomicF32::neu(1.0), AtomicF32::neu(1.0), AtomicF32::neu(0.8)],
            komp_an: AtomicBool::new(false),
            schwelle: AtomicF32::neu(-24.0),
            ratio: AtomicF32::neu(4.0),
            attack_ms: AtomicF32::neu(10.0),
            release_ms: AtomicF32::neu(250.0),
            knee: AtomicF32::neu(30.0),
            makeup: AtomicF32::neu(0.0),
            hall_an: AtomicBool::new(false),
            hall_mix: AtomicF32::neu(0.25),
            hall_groesse: AtomicF32::neu(2.2),
            hall_abkling: AtomicF32::neu(2.5),
        }
    }

    /// alles aus, Werte wie Songcenter-Vorgabe
    pub fn neutral(&self) {
        self.tiefsperre.store(false, Relaxed);
        self.eq_an.store(false, Relaxed);
        for (b, &(_, f, q)) in EQ_VORGABE.iter().enumerate() {
            self.eq_freq[b].set(f);
            self.eq_gain[b].set(0.0);
            self.eq_q[b].set(q);
        }
        self.komp_an.store(false, Relaxed);
        self.schwelle.set(-24.0);
        self.ratio.set(4.0);
        self.attack_ms.set(10.0);
        self.release_ms.set(250.0);
        self.knee.set(30.0);
        self.makeup.set(0.0);
        self.hall_an.store(false, Relaxed);
        self.hall_mix.set(0.25);
        self.hall_groesse.set(2.2);
        self.hall_abkling.set(2.5);
    }

    /// Ist irgendein Effekt an?
    pub fn aktiv(&self) -> bool {
        self.tiefsperre.load(Relaxed) || self.eq_an.load(Relaxed) || self.komp_an.load(Relaxed) || self.hall_an.load(Relaxed)
    }

    /// Effekte als „schlüssel=wert“-Liste (Teil des Kanal-Textes)
    pub fn text(&self) -> String {
        let b = |x: &AtomicBool| x.load(Relaxed) as u8;
        let mut t = format!("lc={},eq={}", b(&self.tiefsperre), b(&self.eq_an));
        for i in 0..EQ_BAENDER {
            t += &format!(",b{i}={}/{}/{}", self.eq_freq[i].get(), self.eq_gain[i].get(), self.eq_q[i].get());
        }
        t += &format!(
            ",k={},th={},ra={},at={},re={},kn={},mu={},h={},hm={},hg={},hd={}",
            b(&self.komp_an),
            self.schwelle.get(),
            self.ratio.get(),
            self.attack_ms.get(),
            self.release_ms.get(),
            self.knee.get(),
            self.makeup.get(),
            b(&self.hall_an),
            self.hall_mix.get(),
            self.hall_groesse.get(),
            self.hall_abkling.get()
        );
        t
    }

    /// Einen Wert aus „schlüssel=wert“ übernehmen (unbekannte Schlüssel werden ignoriert)
    pub fn wert_setzen(&self, k: &str, v: &str) {
        let z = |min: f32, max: f32| v.parse::<f32>().ok().map(|x| x.clamp(min, max));
        let an = v == "1";
        match k {
            "lc" => self.tiefsperre.store(an, Relaxed),
            "eq" => self.eq_an.store(an, Relaxed),
            "k" => self.komp_an.store(an, Relaxed),
            "h" => self.hall_an.store(an, Relaxed),
            _ => {}
        }
        let setze = |a: &AtomicF32, w: Option<f32>| {
            if let Some(w) = w {
                a.set(w)
            }
        };
        match k {
            "th" => setze(&self.schwelle, z(-60.0, 0.0)),
            "ra" => setze(&self.ratio, z(1.0, 20.0)),
            "at" => setze(&self.attack_ms, z(0.0, 200.0)),
            "re" => setze(&self.release_ms, z(10.0, 1000.0)),
            "kn" => setze(&self.knee, z(0.0, 40.0)),
            "mu" => setze(&self.makeup, z(0.0, 24.0)),
            "hm" => setze(&self.hall_mix, z(0.0, 1.0)),
            "hg" => setze(&self.hall_groesse, z(0.2, 6.0)),
            "hd" => setze(&self.hall_abkling, z(0.5, 8.0)),
            _ => {}
        }
        if let Some(i) = k.strip_prefix('b').and_then(|x| x.parse::<usize>().ok()).filter(|&i| i < EQ_BAENDER) {
            let w: Vec<f32> = v.split('/').filter_map(|x| x.parse().ok()).collect();
            if w.len() == 3 {
                self.eq_freq[i].set(w[0].clamp(20.0, 20_000.0));
                self.eq_gain[i].set(w[1].clamp(-18.0, 18.0));
                self.eq_q[i].set(w[2].clamp(0.2, 12.0));
            }
        }
    }
}

/// Einstellungen, die die Oberfläche jederzeit ändern kann.
pub struct Regler {
    pub eingang_db: AtomicF32,
    pub stumm: AtomicBool,
    pub lautstaerke_db: AtomicF32,
    pub redundanz: AtomicBool,
    pub puffer_auto: AtomicBool,
    pub puffer_ms: AtomicF32,
    pub schutz: AtomicBool,
    /// Metronom-Lautstärke hier im Raum (−40 = aus)
    pub metronom_db: AtomicF32,
    /// Lautstärke der Spur (Audiodatei für den ganzen Raum)
    pub spur_db: AtomicF32,
    /// Mischpult: Eingangskanal an/aus und Lautstärke vor dem Verschicken
    pub kanal_an: [AtomicBool; MAX_KANAELE],
    pub kanal_db: [AtomicF32; MAX_KANAELE],
    /// noch keine gespeicherte Mischung für dieses Gerät -> beim Öffnen Vorgabe setzen
    pub kanaele_vorgabe: AtomicBool,
    /// Effekte je Eingang (alle ohne zusätzliche Latenz)
    pub kanal_pan: [AtomicF32; MAX_KANAELE],
    /// Effekte je Eingang (EQ, Kompressor, Hall – alle ohne zusätzliche Latenz)
    pub fx: [KanalFx; MAX_KANAELE],
    /// Stereo senden (sonst Mono)
    pub stereo: AtomicBool,
    /// Klangform des Metronoms (Index in puffer::KLANGFORMEN)
    pub metronom_klang: AtomicU32,
}

/// Messwerte für die Anzeige.
pub struct Anzeige {
    pub pegel_ein: AtomicF32,
    pub pegel_aus: AtomicF32,
    pub ein_latenz_ms: AtomicF32,
    pub aus_latenz_ms: AtomicF32,
    pub ein_info: Mutex<String>,
    pub aus_info: Mutex<String>,
    pub puffer_ist_ms: AtomicF32,
    pub puffer_ziel_ms: AtomicF32,
    pub unterlaeufe: AtomicU32,
    pub rtt_relay_ms: AtomicF32,
    pub rtt_direkt_ms: AtomicF32,
    /// 0 = keine, 1 = über den Server, 2 = direkt
    pub weg: AtomicU8,
    pub partner: Mutex<String>,
    pub partner_da: AtomicBool,
    pub status: Mutex<String>,
    pub fehler: Mutex<Option<String>>,
    pub verlust_prozent: AtomicF32,
    pub partner_puffer_ms: AtomicF32,
    pub partner_aus_ms: AtomicF32,
    pub partner_ein_ms: AtomicF32,
    pub partner_verlust: AtomicF32,
    pub rueckkopplungen: AtomicU32,
    pub kanal_pegel: [AtomicF32; MAX_KANAELE],
    /// Kompressor je Kanal: Eingangspegel, Absenkung (dB, positiv), Ausgangspegel (linear, Spitze seit dem letzten Lesen)
    pub komp_ein: [AtomicF32; MAX_KANAELE],
    pub komp_gr: [AtomicF32; MAX_KANAELE],
    pub komp_aus: [AtomicF32; MAX_KANAELE],
    /// Frequenzanzeige: Eingang (nach dem EQ), den das Effektfenster zeigt (u32::MAX = keiner)
    pub spektrum_kanal: AtomicU32,
    pub spektrum_rate: AtomicU32,
    pub spektrum_pos: AtomicU32,
    pub spektrum: [AtomicF32; SPEKTRUM_N],
    pub kanal_anzahl: AtomicU32,
    /// davon Kanäle des Haupteingangs (danach folgen die Zusatzquellen)
    pub haupt_kanaele: AtomicU32,
    pub kanal_namen: Mutex<Vec<String>>,
}

/// Ergebnis der Spektralanalyse des eigenen Mikrofons (für den Rückkopplungsschutz).
pub struct Analyse {
    pub frequenz: AtomicF32,
    pub anteil: AtomicF32,
    pub pegel_db: AtomicF32,
}

pub struct Geteilt {
    pub regler: Regler,
    pub anzeige: Anzeige,
    pub analyse: Analyse,
    pub stop: AtomicBool,
    /// Aktuelles Ziel der Audiopakete: (IPv4 << 16 | Port), 0 = nicht senden
    pub ziel: AtomicU64,
    pub quell_rate: AtomicU32,
    pub paket_frames: AtomicU32,
    /// gemessener Netzjitter (Verspätung gegenüber dem frühesten Paket), ms
    pub jitter_ms: AtomicF32,
    pub reset: AtomicBool,
    /// Serveruhr − eigene Uhr in µs
    pub uhr_versatz: AtomicI64,
    pub uhr_ok: AtomicBool,
    pub metronom: MetronomZustand,
    /// Änderungswunsch der Oberfläche (an, bpm, takt) – schickt der Netz-Thread an den Server
    pub metronom_wunsch: Mutex<Option<(bool, f32, u8)>>,
    /// Adresse des Servers (wie `ziel` kodiert) – dorthin geht der Ton für die Aufnahme
    pub server_ziel: AtomicU64,
    /// Aufnahme des Raums läuft (Stand vom Server), Beginn in Serverzeit, Name
    pub aufnahme_an: AtomicBool,
    pub aufnahme_start: AtomicU64,
    pub aufnahme_name: Mutex<String>,
    /// Kanäle der Zusatzquellen (Name, Vorgabe-Panorama) – hängen im Mischpult hinter dem Haupteingang
    pub zusatz_namen: Mutex<Vec<(String, f32)>>,
    /// Aufnahme vom Server, die gerade angehört wird (mischt die Wiedergabe hinzu)
    pub vorhoeren: Mutex<Option<crate::abspielen::Vorhoeren>>,
    /// Spur des Raums laut Server: an, Serverzeit von Position 0, Änderungszähler, (Datei, Ablage-Kennung)
    pub spur_an: AtomicBool,
    pub spur_start: AtomicU64,
    pub spur_stand: AtomicU32,
    pub spur_datei: Mutex<(String, [u8; 32])>,
    /// Wunsch der Oberfläche: (an, Kennung, Datei, ab ms)
    pub spur_wunsch: Mutex<Option<(bool, [u8; 32], String, u32)>>,
    /// geladene Spur (Stereo-Paare PCM16) für die Wiedergabe
    pub spur_ton: Mutex<Option<SpurTon>>,
    /// Wunsch der Oberfläche: (an, Ablage-Kennung, Name, Klangform des Metronoms falls mit aufnehmen)
    pub aufnahme_wunsch: Mutex<Option<(bool, [u8; 32], String, Option<u8>)>>,
}

impl Geteilt {
    pub fn neu() -> Self {
        Geteilt {
            regler: Regler {
                eingang_db: AtomicF32::neu(0.0),
                stumm: AtomicBool::new(false),
                lautstaerke_db: AtomicF32::neu(0.0),
                redundanz: AtomicBool::new(true),
                puffer_auto: AtomicBool::new(false),
                puffer_ms: AtomicF32::neu(2.0),
                schutz: AtomicBool::new(true),
                metronom_db: AtomicF32::neu(-12.0),
                spur_db: AtomicF32::neu(-6.0),
                kanal_an: [const { AtomicBool::new(false) }; MAX_KANAELE],
                kanal_db: [const { AtomicF32::neu(0.0) }; MAX_KANAELE],
                kanaele_vorgabe: AtomicBool::new(true),
                kanal_pan: [const { AtomicF32::neu(0.0) }; MAX_KANAELE],
                fx: [const { KanalFx::neu() }; MAX_KANAELE],
                stereo: AtomicBool::new(false),
                metronom_klang: AtomicU32::new(0),
            },
            anzeige: Anzeige {
                pegel_ein: AtomicF32::neu(0.0),
                pegel_aus: AtomicF32::neu(0.0),
                ein_latenz_ms: AtomicF32::neu(0.0),
                aus_latenz_ms: AtomicF32::neu(0.0),
                ein_info: Mutex::new(String::new()),
                aus_info: Mutex::new(String::new()),
                puffer_ist_ms: AtomicF32::neu(0.0),
                puffer_ziel_ms: AtomicF32::neu(0.0),
                unterlaeufe: AtomicU32::new(0),
                rtt_relay_ms: AtomicF32::neu(-1.0),
                rtt_direkt_ms: AtomicF32::neu(-1.0),
                weg: AtomicU8::new(0),
                partner: Mutex::new(String::new()),
                partner_da: AtomicBool::new(false),
                status: Mutex::new(String::new()),
                fehler: Mutex::new(None),
                verlust_prozent: AtomicF32::neu(0.0),
                partner_puffer_ms: AtomicF32::neu(-1.0),
                partner_aus_ms: AtomicF32::neu(-1.0),
                partner_ein_ms: AtomicF32::neu(-1.0),
                partner_verlust: AtomicF32::neu(0.0),
                rueckkopplungen: AtomicU32::new(0),
                kanal_pegel: [const { AtomicF32::neu(0.0) }; MAX_KANAELE],
                komp_ein: [const { AtomicF32::neu(0.0) }; MAX_KANAELE],
                komp_gr: [const { AtomicF32::neu(0.0) }; MAX_KANAELE],
                komp_aus: [const { AtomicF32::neu(0.0) }; MAX_KANAELE],
                spektrum_kanal: AtomicU32::new(u32::MAX),
                spektrum_rate: AtomicU32::new(48_000),
                spektrum_pos: AtomicU32::new(0),
                spektrum: [const { AtomicF32::neu(0.0) }; SPEKTRUM_N],
                kanal_anzahl: AtomicU32::new(0),
                haupt_kanaele: AtomicU32::new(0),
                kanal_namen: Mutex::new(Vec::new()),
            },
            analyse: Analyse {
                frequenz: AtomicF32::neu(0.0),
                anteil: AtomicF32::neu(0.0),
                pegel_db: AtomicF32::neu(-120.0),
            },
            stop: AtomicBool::new(false),
            ziel: AtomicU64::new(0),
            quell_rate: AtomicU32::new(0),
            paket_frames: AtomicU32::new(0),
            jitter_ms: AtomicF32::neu(0.0),
            reset: AtomicBool::new(false),
            uhr_versatz: AtomicI64::new(0),
            uhr_ok: AtomicBool::new(false),
            metronom: MetronomZustand {
                an: AtomicBool::new(false),
                bpm: AtomicF32::neu(100.0),
                takt: AtomicU32::new(4),
                start_us: AtomicU64::new(0),
                stand: AtomicU32::new(0),
            },
            metronom_wunsch: Mutex::new(None),
            server_ziel: AtomicU64::new(0),
            zusatz_namen: Mutex::new(Vec::new()),
            vorhoeren: Mutex::new(None),
            spur_an: AtomicBool::new(false),
            spur_start: AtomicU64::new(0),
            spur_stand: AtomicU32::new(0),
            spur_datei: Mutex::new((String::new(), [0; 32])),
            spur_wunsch: Mutex::new(None),
            spur_ton: Mutex::new(None),
            aufnahme_an: AtomicBool::new(false),
            aufnahme_start: AtomicU64::new(0),
            aufnahme_name: Mutex::new(String::new()),
            aufnahme_wunsch: Mutex::new(None),
        }
    }

    /// Messwerte vor einer neuen Verbindung zurücksetzen
    pub fn anzeige_zuruecksetzen(&self) {
        let a = &self.anzeige;
        a.ein_latenz_ms.set(0.0);
        a.aus_latenz_ms.set(0.0);
        a.ein_info.lock().unwrap().clear();
        a.aus_info.lock().unwrap().clear();
        a.puffer_ist_ms.set(0.0);
        a.puffer_ziel_ms.set(0.0);
        a.unterlaeufe.store(0, Relaxed);
        a.rtt_relay_ms.set(-1.0);
        a.rtt_direkt_ms.set(-1.0);
        a.weg.store(0, Relaxed);
        a.partner.lock().unwrap().clear();
        a.partner_da.store(false, Relaxed);
        *a.status.lock().unwrap() = "Verbinde …".into();
        *a.fehler.lock().unwrap() = None;
        a.verlust_prozent.set(0.0);
        a.partner_puffer_ms.set(-1.0);
        a.partner_aus_ms.set(-1.0);
        a.partner_ein_ms.set(-1.0);
        a.partner_verlust.set(0.0);
        self.stop.store(false, Relaxed);
        self.ziel.store(0, Relaxed);
        self.quell_rate.store(0, Relaxed);
        self.paket_frames.store(0, Relaxed);
        self.jitter_ms.set(0.0);
        self.reset.store(true, Relaxed);
        self.uhr_ok.store(false, Relaxed);
        self.metronom.an.store(false, Relaxed);
        *self.metronom_wunsch.lock().unwrap() = None;
        self.server_ziel.store(0, Relaxed);
        self.aufnahme_an.store(false, Relaxed);
        *self.aufnahme_wunsch.lock().unwrap() = None;
        self.spur_an.store(false, Relaxed);
        *self.spur_wunsch.lock().unwrap() = None;
    }

    pub fn fehler(&self, text: impl Into<String>) {
        *self.anzeige.fehler.lock().unwrap() = Some(text.into());
    }

    pub fn status(&self, text: impl Into<String>) {
        let t = text.into();
        let mut s = self.anzeige.status.lock().unwrap();
        if *s != t {
            *s = t;
        }
    }
}

pub fn db_zu_faktor(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Dauerhaft gespeicherte Einstellungen (%APPDATA%\Jamcenter\einstellungen.txt)
#[derive(Clone, Debug)]
pub struct Einstellungen {
    pub raum: String,
    pub passwort: String,
    pub name: String,
    pub server: String,
    pub eingang: String,
    pub ausgang: String,
    /// leer = Windows-Audio (WASAPI), sonst Name des ASIO-Treibers
    pub treiber: String,
    pub asio_eingang: usize,
    /// erster Kanal des Ausgangspaars
    pub asio_ausgang: usize,
    /// 0 = Vorgabe des Treibers
    pub asio_block: u32,
    pub exklusiv: bool,
    pub redundanz: bool,
    pub schutz: bool,
    pub puffer_auto: bool,
    pub puffer_ms: f32,
    pub eingang_db: f32,
    pub lautstaerke_db: f32,
    pub metronom_db: f32,
    /// Mischpult: „Geräteschlüssel|an:dB;an:dB;…“
    pub kanaele: String,
    pub stereo: bool,
    pub hall_groesse: f32,
    pub metronom_klang: u32,
    /// zuletzt gewähltes Mix-Preset (nur Anzeige)
    pub mix_preset: String,
    /// Metronom mit aufnehmen
    pub aufnahme_metronom: bool,
    /// weitere Tonquellen („system;aus:<id>;ein:<id>“, siehe zusatz.rs)
    pub zusatz: String,
    pub spur_db: f32,
}

impl Default for Einstellungen {
    fn default() -> Self {
        Einstellungen {
            raum: String::new(),
            passwort: String::new(),
            name: std::env::var("COMPUTERNAME").unwrap_or_default(),
            server: protokoll::STANDARD_SERVER.into(),
            eingang: String::new(),
            ausgang: String::new(),
            treiber: String::new(),
            asio_eingang: 0,
            asio_ausgang: 0,
            asio_block: 0,
            exklusiv: true,
            redundanz: true,
            schutz: true,
            puffer_auto: false,
            puffer_ms: 2.0,
            eingang_db: 0.0,
            lautstaerke_db: 0.0,
            metronom_db: -12.0,
            kanaele: String::new(),
            stereo: false,
            hall_groesse: 0.5,
            metronom_klang: 0,
            mix_preset: String::new(),
            aufnahme_metronom: true,
            zusatz: String::new(),
            spur_db: -6.0,
        }
    }
}

fn ordner() -> Option<std::path::PathBuf> {
    let a = std::env::var_os("APPDATA")?;
    Some(std::path::Path::new(&a).join("Jamcenter"))
}

fn maskieren(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\n', "\\n").replace('\r', "")
}

fn demaskieren(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('n') => out.push('\n'),
                Some(x) => out.push(x),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

impl Einstellungen {
    pub fn laden() -> Self {
        let mut e = Einstellungen::default();
        let Some(o) = ordner() else { return e };
        // alte Einstellungen der „Klangbrücke“ einmalig übernehmen
        let alt = o.parent().map(|p| p.join("Klangbruecke").join("einstellungen.txt"));
        let Ok(text) = std::fs::read_to_string(o.join("einstellungen.txt")).or_else(|_| std::fs::read_to_string(alt.ok_or(std::io::ErrorKind::NotFound)?)) else {
            return e;
        };
        // Ab Version 1.2 ist der Puffer fest auf 2 ms voreingestellt – ältere Einstellungen einmal umstellen
        let puffer_neu = text.lines().any(|z| z.starts_with("puffer_vorgabe="));
        for zeile in text.lines() {
            let Some((k, v)) = zeile.split_once('=') else { continue };
            let v = demaskieren(v);
            let b = v == "1";
            let f = v.parse::<f32>().ok();
            match k {
                "raum" => e.raum = v,
                "passwort" => e.passwort = crate::tresor::entpacken(&v).unwrap_or_default(),
                "name" => e.name = v,
                "server" if !v.is_empty() => e.server = v,
                "eingang" => e.eingang = v,
                "ausgang" => e.ausgang = v,
                "treiber" => e.treiber = v,
                "asio_eingang" => e.asio_eingang = v.parse().unwrap_or(0),
                "asio_ausgang" => e.asio_ausgang = v.parse().unwrap_or(0),
                "asio_block" => e.asio_block = v.parse().unwrap_or(0),
                "exklusiv" => e.exklusiv = b,
                "redundanz" => e.redundanz = b,
                "schutz" => e.schutz = b,
                "puffer_auto" if puffer_neu => e.puffer_auto = b,
                "puffer_ms" if puffer_neu => e.puffer_ms = f.unwrap_or(2.0).clamp(0.0, 100.0),
                "eingang_db" => e.eingang_db = f.unwrap_or(0.0).clamp(-20.0, 30.0),
                "lautstaerke_db" => e.lautstaerke_db = f.unwrap_or(0.0).clamp(-40.0, 12.0),
                "metronom_db" => e.metronom_db = f.unwrap_or(-12.0).clamp(-40.0, 6.0),
                "kanaele" => e.kanaele = v,
                "stereo" => e.stereo = b,
                "hall_groesse" => e.hall_groesse = f.unwrap_or(0.5).clamp(0.0, 1.0),
                "metronom_klang" => e.metronom_klang = v.parse().unwrap_or(0),
                "mix_preset" => e.mix_preset = v,
                "aufnahme_metronom" => e.aufnahme_metronom = b,
                "zusatz" => e.zusatz = v,
                "spur_db" => e.spur_db = f.unwrap_or(-6.0).clamp(-40.0, 6.0),
                _ => {}
            }
        }
        e
    }

    pub fn speichern(&self) {
        let Some(o) = ordner() else { return };
        let _ = std::fs::create_dir_all(&o);
        let b = |x: bool| if x { "1" } else { "0" };
        let text = format!(
            "raum={}\npasswort={}\nname={}\nserver={}\neingang={}\nausgang={}\ntreiber={}\nasio_eingang={}\nasio_ausgang={}\nasio_block={}\nexklusiv={}\nredundanz={}\nschutz={}\npuffer_auto={}\npuffer_ms={}\neingang_db={}\nlautstaerke_db={}\nmetronom_db={}\nkanaele={}\nstereo={}\nhall_groesse={}\nmetronom_klang={}\nmix_preset={}\npuffer_vorgabe=2\naufnahme_metronom={}\nzusatz={}\nspur_db={}\n",
            maskieren(&self.raum),
            maskieren(&crate::tresor::verpacken(&self.passwort)),
            maskieren(&self.name),
            maskieren(&self.server),
            maskieren(&self.eingang),
            maskieren(&self.ausgang),
            maskieren(&self.treiber),
            self.asio_eingang,
            self.asio_ausgang,
            self.asio_block,
            b(self.exklusiv),
            b(self.redundanz),
            b(self.schutz),
            b(self.puffer_auto),
            self.puffer_ms,
            self.eingang_db,
            self.lautstaerke_db,
            self.metronom_db,
            maskieren(&self.kanaele),
            b(self.stereo),
            self.hall_groesse,
            self.metronom_klang,
            maskieren(&self.mix_preset),
            b(self.aufnahme_metronom),
            maskieren(&self.zusatz),
            self.spur_db
        );
        let _ = std::fs::write(o.join("einstellungen.txt"), text);
    }
}

/// Ein Eingang als Text: „an=1,db=-3,pan=0,lc=…,eq=…,b0=f/g/q,…,k=…,th=…,h=…“
pub fn kanal_text(g: &Geteilt, i: usize) -> String {
    let r = &g.regler;
    format!("an={},db={},pan={},{}", r.kanal_an[i].load(Relaxed) as u8, r.kanal_db[i].get(), r.kanal_pan[i].get(), r.fx[i].text())
}

/// Eingang neutral stellen (ohne Effekte)
pub fn kanal_neutral(g: &Geteilt, i: usize, an: bool, db: f32) {
    let r = &g.regler;
    r.kanal_an[i].store(an, Relaxed);
    r.kanal_db[i].set(db);
    r.kanal_pan[i].set(0.0);
    r.fx[i].neutral();
}

/// Text eines Eingangs übernehmen (fehlende Felder = neutral). Liest auch die älteren
/// Formate „an:dB“ und „an:dB:pan:tief:mitte:hoch:tiefsperre:schwelle:verhältnis:hall“.
pub fn kanal_setzen(g: &Geteilt, i: usize, text: &str) {
    let r = &g.regler;
    if text.contains('=') {
        kanal_neutral(g, i, false, 0.0);
        for teil in text.split(',') {
            let Some((k, v)) = teil.split_once('=') else { continue };
            match k {
                "an" => r.kanal_an[i].store(v == "1", Relaxed),
                "db" => r.kanal_db[i].set(v.parse::<f32>().unwrap_or(0.0).clamp(-40.0, 10.0)),
                "pan" => r.kanal_pan[i].set(v.parse::<f32>().unwrap_or(0.0).clamp(-1.0, 1.0)),
                _ => r.fx[i].wert_setzen(k, v),
            }
        }
        return;
    }
    let f: Vec<&str> = text.split(':').collect();
    let zahl = |k: usize, vorgabe: f32, min: f32, max: f32| f.get(k).and_then(|x| x.parse::<f32>().ok()).unwrap_or(vorgabe).clamp(min, max);
    kanal_neutral(g, i, f.first() == Some(&"1"), zahl(1, 0.0, -40.0, 10.0));
    r.kanal_pan[i].set(zahl(2, 0.0, -1.0, 1.0));
    // altes 3-Band-Modell auf Band 1, 3, 5 abbilden
    let fx = &r.fx[i];
    let (tief, mitte, hoch) = (zahl(3, 0.0, -15.0, 15.0), zahl(4, 0.0, -15.0, 15.0), zahl(5, 0.0, -15.0, 15.0));
    if tief != 0.0 || mitte != 0.0 || hoch != 0.0 {
        fx.eq_an.store(true, Relaxed);
        fx.eq_gain[0].set(tief);
        fx.eq_gain[2].set(mitte);
        fx.eq_gain[4].set(hoch);
    }
    fx.tiefsperre.store(f.get(6) == Some(&"1"), Relaxed);
    let schwelle = zahl(7, 0.0, -50.0, 0.0);
    if schwelle < -0.5 {
        fx.komp_an.store(true, Relaxed);
        fx.schwelle.set(schwelle);
        fx.ratio.set(zahl(8, 3.0, 1.0, 20.0));
    }
    let hall = zahl(9, 0.0, 0.0, 1.0);
    if hall > 0.001 {
        fx.hall_an.store(true, Relaxed);
        fx.hall_mix.set(hall);
    }
}

/// Mischung als Text: „stereo~0~anzahl~k1;k2;…“ – unabhängig vom Gerät
pub fn mix_text(g: &Geteilt, n: usize) -> String {
    let n = n.clamp(1, MAX_KANAELE);
    let k: Vec<String> = (0..n).map(|i| kanal_text(g, i)).collect();
    format!("{}~0~{}~{}", g.regler.stereo.load(Relaxed) as u8, n, k.join(";"))
}

/// Mischung übernehmen. Hat das Gerät weniger Eingänge als die Mischung, werden die
/// überzähligen verworfen; hat es mehr, bleiben die übrigen aus. Liefert „stereo“ zurück.
pub fn mix_anwenden(g: &Geteilt, text: &str, verfuegbar: usize) -> bool {
    let verfuegbar = verfuegbar.clamp(1, MAX_KANAELE);
    let r = &g.regler;
    let (kopf, liste) = match text.rsplit_once('~') {
        Some((k, l)) => (Some(k), l),
        None => (None, text),
    };
    if let Some(k) = kopf {
        r.stereo.store(k.split('~').next() == Some("1"), Relaxed);
    }
    let kanaele: Vec<&str> = liste.split(';').filter(|x| !x.is_empty()).collect();
    for i in 0..MAX_KANAELE {
        match kanaele.get(i) {
            Some(t) if i < verfuegbar => kanal_setzen(g, i, t),
            _ => kanal_neutral(g, i, false, 0.0),
        }
    }
    r.kanaele_vorgabe.store(false, Relaxed);
    r.stereo.load(Relaxed)
}

fn presets_datei() -> Option<std::path::PathBuf> {
    Some(ordner()?.join("presets.txt"))
}

/// Gespeicherte Mix-Presets (Name, Mix-Text)
pub fn presets_laden() -> Vec<(String, String)> {
    let Some(d) = presets_datei() else { return Vec::new() };
    let Ok(t) = std::fs::read_to_string(d) else { return Vec::new() };
    t.lines()
        .filter_map(|z| z.split_once('='))
        .map(|(n, m)| (demaskieren(n), m.to_string()))
        .filter(|(n, _)| !n.trim().is_empty())
        .collect()
}

pub fn presets_speichern(liste: &[(String, String)]) {
    let Some(d) = presets_datei() else { return };
    if let Some(o) = d.parent() {
        let _ = std::fs::create_dir_all(o);
    }
    let text: String = liste.iter().map(|(n, m)| format!("{}={}\n", maskieren(&n.replace('=', "-")), m)).collect();
    let _ = std::fs::write(d, text);
}

impl Einstellungen {
    /// Schlüssel des Eingangsgeräts, für das die Mischpult-Einstellung gilt
    pub fn kanal_schluessel(&self) -> String {
        let haupt = if self.treiber.is_empty() { format!("wasapi:{}", self.eingang) } else { format!("asio:{}", self.treiber) };
        // eigene Mischung je Kombination aus Haupteingang und Zusatzquellen
        if self.zusatz.is_empty() {
            haupt
        } else {
            format!("{haupt}+{}", self.zusatz)
        }
    }

    /// Gespeicherte Mischung in die Regler übernehmen (sonst: Vorgabe beim Öffnen des Geräts)
    pub fn kanaele_anwenden(&self, g: &Geteilt) {
        let r = &g.regler;
        let schluessel = self.kanal_schluessel();
        let werte = self.kanaele.split_once('|').filter(|(k, _)| *k == schluessel).map(|(_, w)| w.to_string());
        let stereo = self.stereo;
        match werte {
            Some(w) => {
                mix_anwenden(g, &w, MAX_KANAELE);
            }
            None => r.kanaele_vorgabe.store(true, Relaxed),
        }
        // Stereo und Hallgröße stellt das Hauptfenster ein
        r.stereo.store(stereo, Relaxed);

    }

    /// Aktuelle Mischung aus den Reglern merken
    pub fn kanaele_merken(&mut self, g: &Geteilt) {
        let n = (g.anzeige.kanal_anzahl.load(Relaxed) as usize).clamp(1, MAX_KANAELE);
        self.kanaele = format!("{}|{}", self.kanal_schluessel(), mix_text(g, n));
    }
}

impl Geteilt {
    /// Gerät mit `n` Eingangskanälen geöffnet: Namen merken, ggf. Vorgabe-Mischung setzen
    /// (Windows-Audio: alle Kanäle gleich laut; ASIO: nur `nur_kanal`).
    pub fn kanaele_geoeffnet(&self, mut namen: Vec<String>, nur_kanal: Option<usize>) {
        let haupt = namen.len().clamp(1, MAX_KANAELE);
        namen.truncate(haupt);
        let zusatz = self.zusatz_namen.lock().unwrap().clone();
        let zusatz: Vec<(String, f32)> = zusatz.into_iter().take(MAX_KANAELE - haupt).collect();
        namen.extend(zusatz.iter().map(|z| z.0.clone()));
        let n = namen.len().clamp(1, MAX_KANAELE);
        self.anzeige.haupt_kanaele.store(haupt as u32, Relaxed);
        self.anzeige.kanal_anzahl.store(n as u32, Relaxed);
        *self.anzeige.kanal_namen.lock().unwrap() = namen;
        let r = &self.regler;
        if r.kanaele_vorgabe.swap(false, Relaxed) {
            let db = -20.0 * (haupt as f32).log10();
            for i in 0..MAX_KANAELE {
                let an = match nur_kanal {
                    Some(k) => i == k.min(haupt - 1),
                    None => i < haupt,
                };
                kanal_neutral(self, i, an, if nur_kanal.is_some() { 0.0 } else { (db * 2.0).round() / 2.0 });
            }
            // Zusatzquellen: an, volle Lautstärke, Stereo-Paare links/rechts
            for (j, (_, pan)) in zusatz.iter().enumerate() {
                kanal_neutral(self, haupt + j, true, 0.0);
                r.kanal_pan[haupt + j].set(*pan);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Preset mit 4 Kanälen auf ein Gerät mit 2 Eingängen: Kanäle 3 und 4 werden verworfen
    #[test]
    fn preset_passt_sich_an() {
        let g = Geteilt::neu();
        for i in 0..4 {
            kanal_neutral(&g, i, true, -3.0 * i as f32);
            g.regler.kanal_pan[i].set(0.25 * i as f32 - 0.5);
            g.regler.fx[i].hall_an.store(true, Relaxed);
            g.regler.fx[i].hall_mix.set(0.1 * i as f32);
            g.regler.fx[i].eq_gain[3].set(i as f32);
        }
        g.regler.stereo.store(true, Relaxed);
        let text = mix_text(&g, 4);
        assert!(text.starts_with("1~0~4~"), "{text}");

        let h = Geteilt::neu();
        kanal_neutral(&h, 3, true, 5.0);
        assert!(mix_anwenden(&h, &text, 2));
        assert!(h.regler.kanal_an[0].load(Relaxed) && h.regler.kanal_an[1].load(Relaxed));
        assert_eq!(h.regler.kanal_db[1].get(), -3.0);
        assert!((h.regler.kanal_pan[1].get() + 0.25).abs() < 1e-6);
        assert!(h.regler.fx[1].hall_an.load(Relaxed));
        assert!((h.regler.fx[1].hall_mix.get() - 0.1).abs() < 1e-6);
        assert_eq!(h.regler.fx[1].eq_gain[3].get(), 1.0);
        // nicht einstellbare Kanäle verworfen, übrige aus
        assert!(!h.regler.kanal_an[2].load(Relaxed) && !h.regler.kanal_an[3].load(Relaxed));
        // ältere Formate werden noch gelesen
        mix_anwenden(&h, "1:-6;0:-2", 8);
        assert!(h.regler.kanal_an[0].load(Relaxed) && !h.regler.kanal_an[1].load(Relaxed));
        assert_eq!(h.regler.kanal_db[0].get(), -6.0);
        mix_anwenden(&h, "0~0.5~1~1:0:0:3:0:-2:1:-20:4:0.3", 8);
        let fx = &h.regler.fx[0];
        assert!(fx.eq_an.load(Relaxed) && fx.komp_an.load(Relaxed) && fx.hall_an.load(Relaxed) && fx.tiefsperre.load(Relaxed));
        assert_eq!(fx.eq_gain[0].get(), 3.0);
        assert_eq!(fx.eq_gain[4].get(), -2.0);
    }
}

/// Geladene Spur: Schlüssel (Kennung+Datei), Rate, Stereo-Paare
pub struct SpurTon {
    pub schluessel: String,
    pub rate: u32,
    pub daten: std::sync::Arc<Vec<i16>>,
}

pub fn spur_schluessel(kennung: &[u8; 32], datei: &str) -> String {
    format!("{}/{datei}", kennung.iter().take(8).map(|b| format!("{b:02x}")).collect::<String>())
}
