//! ASIO-Anbindung (Steinberg Audio Stream Input/Output) – ohne SDK, direkt über die
//! vtable des Treibers. ASIO ruft EINEN Callback für Ein- und Ausgang auf: Mikrofonblock
//! lesen, sofort verschicken, Wiedergabeblock füllen – alles mit der kleinsten Blockgröße,
//! die das Audiointerface schafft (oft 32–64 Samples = 0,7–1,3 ms).
//!
//! Nur 64-Bit: Dort gibt es keine thiscall-Besonderheit, die Methoden sind normale
//! Funktionen mit dem Objektzeiger als erstem Argument.

use crate::gemeinsam::Geteilt;
use crate::netz::Sender;
use crate::puffer::Wiedergabe;
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering::*};
use std::sync::Arc;
use std::time::Duration;
use windows::core::{GUID, HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::CLSIDFromString;
use windows::Win32::System::Registry::*;
use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, GetDesktopWindow, PeekMessageW, TranslateMessage, MSG, PM_REMOVE};

windows_targets::link!("ole32.dll" "system" fn CoCreateInstance(rclsid: *const GUID, punkouter: *mut c_void, dwclscontext: u32, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT);

const ASE_OK: i32 = 0;

/// Oberfläche bittet den laufenden Treiber, sein Einstellungsfenster zu öffnen.
pub static PANEL_OEFFNEN: AtomicBool = AtomicBool::new(false);
static NEUSTART: AtomicBool = AtomicBool::new(false);
static LATENZ_NEU: AtomicBool = AtomicBool::new(false);
static LAUF: AtomicPtr<Lauf> = AtomicPtr::new(null_mut());
static RUFE: AtomicU64 = AtomicU64::new(0);

#[repr(C)]
struct KanalInfo {
    channel: i32,
    is_input: i32,
    is_active: i32,
    channel_group: i32,
    typ: i32,
    name: [u8; 32],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PufferInfo {
    is_input: i32,
    channel_num: i32,
    buffers: [*mut c_void; 2],
}

#[repr(C)]
struct Rueckrufe {
    buffer_switch: unsafe extern "C" fn(i32, i32),
    sample_rate_did_change: unsafe extern "C" fn(f64),
    asio_message: unsafe extern "C" fn(i32, i32, *mut c_void, *mut f64) -> i32,
    buffer_switch_time_info: unsafe extern "C" fn(*mut c_void, i32, i32) -> *mut c_void,
}

#[repr(C)]
struct Vtbl {
    query_interface: unsafe extern "system" fn(*mut Iasio, *const GUID, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut Iasio) -> u32,
    release: unsafe extern "system" fn(*mut Iasio) -> u32,
    init: unsafe extern "system" fn(*mut Iasio, *mut c_void) -> i32,
    get_driver_name: unsafe extern "system" fn(*mut Iasio, *mut u8),
    get_driver_version: unsafe extern "system" fn(*mut Iasio) -> i32,
    get_error_message: unsafe extern "system" fn(*mut Iasio, *mut u8),
    start: unsafe extern "system" fn(*mut Iasio) -> i32,
    stop: unsafe extern "system" fn(*mut Iasio) -> i32,
    get_channels: unsafe extern "system" fn(*mut Iasio, *mut i32, *mut i32) -> i32,
    get_latencies: unsafe extern "system" fn(*mut Iasio, *mut i32, *mut i32) -> i32,
    get_buffer_size: unsafe extern "system" fn(*mut Iasio, *mut i32, *mut i32, *mut i32, *mut i32) -> i32,
    can_sample_rate: unsafe extern "system" fn(*mut Iasio, f64) -> i32,
    get_sample_rate: unsafe extern "system" fn(*mut Iasio, *mut f64) -> i32,
    set_sample_rate: unsafe extern "system" fn(*mut Iasio, f64) -> i32,
    get_clock_sources: unsafe extern "system" fn(*mut Iasio, *mut c_void, *mut i32) -> i32,
    set_clock_source: unsafe extern "system" fn(*mut Iasio, i32) -> i32,
    get_sample_position: unsafe extern "system" fn(*mut Iasio, *mut u64, *mut u64) -> i32,
    get_channel_info: unsafe extern "system" fn(*mut Iasio, *mut KanalInfo) -> i32,
    create_buffers: unsafe extern "system" fn(*mut Iasio, *mut PufferInfo, i32, i32, *const Rueckrufe) -> i32,
    dispose_buffers: unsafe extern "system" fn(*mut Iasio) -> i32,
    control_panel: unsafe extern "system" fn(*mut Iasio) -> i32,
    future: unsafe extern "system" fn(*mut Iasio, i32, *mut c_void) -> i32,
    output_ready: unsafe extern "system" fn(*mut Iasio) -> i32,
}

#[repr(C)]
struct Iasio {
    vtbl: *const Vtbl,
}

#[derive(Clone, Debug)]
pub struct TreiberInfo {
    pub name: String,
    pub clsid: GUID,
}

/// Installierte ASIO-Treiber aus HKLM\SOFTWARE\ASIO
pub fn treiber_liste() -> Vec<TreiberInfo> {
    let mut v = Vec::new();
    unsafe {
        let mut schluessel = HKEY::default();
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, windows::core::w!("SOFTWARE\\ASIO"), 0, KEY_READ, &mut schluessel).is_err() {
            return v;
        }
        for i in 0..64u32 {
            let mut name = [0u16; 256];
            let mut len = name.len() as u32;
            if RegEnumKeyExW(schluessel, i, PWSTR(name.as_mut_ptr()), &mut len, None, PWSTR::null(), None, None).is_err() {
                break;
            }
            let n = String::from_utf16_lossy(&name[..len as usize]);
            let mut wert = [0u16; 64];
            let mut groesse = (wert.len() * 2) as u32;
            let sub: Vec<u16> = name[..len as usize].iter().copied().chain(std::iter::once(0)).collect();
            if RegGetValueW(
                schluessel,
                PCWSTR(sub.as_ptr()),
                windows::core::w!("CLSID"),
                RRF_RT_REG_SZ,
                None,
                Some(wert.as_mut_ptr() as *mut c_void),
                Some(&mut groesse),
            )
            .is_err()
            {
                continue;
            }
            if let Ok(clsid) = CLSIDFromString(PCWSTR(wert.as_ptr())) {
                v.push(TreiberInfo { name: n, clsid });
            }
        }
        let _ = RegCloseKey(schluessel);
    }
    v
}

fn c_text(b: &[u8]) -> String {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..n]).trim().to_string()
}

/// Geladener Treiber (nur im Thread benutzen, der ihn geladen hat – STA)
pub struct Treiber {
    p: *mut Iasio,
    pub name: String,
}

pub struct Kanaele {
    pub eingaenge: Vec<String>,
    pub ausgaenge: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Bloecke {
    pub min: u32,
    pub max: u32,
    pub bevorzugt: u32,
    pub moeglich: Vec<u32>,
}

impl Treiber {
    fn v(&self) -> &Vtbl {
        unsafe { &*(*self.p).vtbl }
    }

    /// `fenster` muss zum aufrufenden Thread gehören (manche Treiber steuern sich über Fensternachrichten)
    pub fn laden(info: &TreiberInfo, fenster: HWND) -> Result<Treiber, String> {
        unsafe {
            let mut p: *mut c_void = null_mut();
            // Besonderheit von ASIO: die Schnittstellen-ID ist die CLSID des Treibers
            let hr = CoCreateInstance(&info.clsid, null_mut(), 1 /* CLSCTX_INPROC_SERVER */, &info.clsid, &mut p);
            if hr.is_err() || p.is_null() {
                return Err(format!("ASIO-Treiber „{}“ lässt sich nicht laden (0x{:08X})", info.name, hr.0 as u32));
            }
            let t = Treiber { p: p as *mut Iasio, name: info.name.clone() };
            let fenster = if fenster.0.is_null() { GetDesktopWindow() } else { fenster };
            if (t.v().init)(t.p, fenster.0) == 0 {
                let mut m = [0u8; 128];
                (t.v().get_error_message)(t.p, m.as_mut_ptr());
                return Err(format!("ASIO-Treiber „{}“: {}", info.name, c_text(&m)));
            }
            Ok(t)
        }
    }

    pub fn kanaele(&self) -> Kanaele {
        let mut k = Kanaele { eingaenge: Vec::new(), ausgaenge: Vec::new() };
        unsafe {
            let (mut ein, mut aus) = (0i32, 0i32);
            if (self.v().get_channels)(self.p, &mut ein, &mut aus) != ASE_OK {
                return k;
            }
            for (eingang, n, ziel) in [(1, ein, &mut k.eingaenge), (0, aus, &mut k.ausgaenge)] {
                for i in 0..n.clamp(0, 256) {
                    let mut info = KanalInfo { channel: i, is_input: eingang, is_active: 0, channel_group: 0, typ: 0, name: [0; 32] };
                    let name = if (self.v().get_channel_info)(self.p, &mut info) == ASE_OK { c_text(&info.name) } else { String::new() };
                    ziel.push(name);
                }
            }
        }
        k
    }

    pub fn bloecke(&self) -> Bloecke {
        let (mut min, mut max, mut bev, mut gran) = (0i32, 0i32, 0i32, 0i32);
        unsafe {
            (self.v().get_buffer_size)(self.p, &mut min, &mut max, &mut bev, &mut gran);
        }
        let (min, max) = (min.max(1) as u32, max.max(min.max(1)) as u32);
        let bev = (bev.max(1) as u32).clamp(min, max);
        let mut moeglich = Vec::new();
        if gran == -1 {
            let mut x = min.next_power_of_two();
            while x <= max && moeglich.len() < 16 {
                moeglich.push(x);
                x *= 2;
            }
        } else if gran > 0 {
            let mut x = min;
            while x <= max && moeglich.len() < 64 {
                // nur übliche Größen anbieten (Vielfache von 16 bzw. Zweierpotenzen)
                if x == min || x == max || x % 16 == 0 {
                    moeglich.push(x);
                }
                x += gran as u32;
            }
        }
        if !moeglich.contains(&bev) {
            moeglich.push(bev);
        }
        moeglich.sort_unstable();
        moeglich.dedup();
        Bloecke { min, max, bevorzugt: bev, moeglich }
    }

    pub fn rate_waehlen(&self) -> f64 {
        unsafe {
            for r in [48_000.0, 44_100.0] {
                if (self.v().can_sample_rate)(self.p, r) == ASE_OK {
                    let mut akt = 0.0;
                    (self.v().get_sample_rate)(self.p, &mut akt);
                    if (akt - r).abs() > 0.5 {
                        (self.v().set_sample_rate)(self.p, r);
                    }
                    break;
                }
            }
            let mut akt = 0.0;
            (self.v().get_sample_rate)(self.p, &mut akt);
            if akt < 8000.0 {
                48_000.0
            } else {
                akt
            }
        }
    }

    pub fn panel(&self) {
        unsafe {
            (self.v().control_panel)(self.p);
        }
    }
}

impl Drop for Treiber {
    fn drop(&mut self) {
        unsafe {
            (self.v().release)(self.p);
        }
    }
}

#[derive(Clone, Copy)]
struct KanalPuffer {
    puffer: [*mut c_void; 2],
    typ: i32,
}

/// Zustand, den der Treiber-Callback benutzt (gehört während des Laufs dem Callback)
struct Lauf {
    eingaenge: Vec<KanalPuffer>,
    kanal_tmp: Vec<f32>,
    geteilt: Arc<Geteilt>,
    ausgang: [Option<KanalPuffer>; 2],
    block: usize,
    sender: Sender,
    wiedergabe: Wiedergabe,
    mono_aus: Vec<f32>,
    paare_aus: Vec<f32>,
    output_ready: bool,
    treiber: *mut Iasio,
}

fn typ_unterstuetzt(t: i32) -> bool {
    matches!(t, 16..=20 | 24..=27)
}

fn typ_name(t: i32) -> &'static str {
    match t {
        16 => "16 Bit",
        17 => "24 Bit",
        18 => "32 Bit",
        19 => "32 Bit Float",
        20 => "64 Bit Float",
        24..=27 => "24 Bit in 32",
        _ => "?",
    }
}

#[inline]
unsafe fn lesen(p: *const c_void, typ: i32, aus: &mut [f32]) {
    let n = aus.len();
    match typ {
        16 => {
            let q = std::slice::from_raw_parts(p as *const i16, n);
            for (a, &x) in aus.iter_mut().zip(q) {
                *a = x as f32 * (1.0 / 32768.0);
            }
        }
        17 => {
            let q = std::slice::from_raw_parts(p as *const u8, n * 3);
            for (i, a) in aus.iter_mut().enumerate() {
                let v = (q[i * 3] as i32) | ((q[i * 3 + 1] as i32) << 8) | ((q[i * 3 + 2] as i8 as i32) << 16);
                *a = v as f32 * (1.0 / 8_388_608.0);
            }
        }
        18 | 24..=27 => {
            let teiler = match typ {
                24 => 32_768.0,
                25 => 131_072.0,
                26 => 524_288.0,
                27 => 8_388_608.0,
                _ => 2_147_483_648.0,
            };
            let q = std::slice::from_raw_parts(p as *const i32, n);
            for (a, &x) in aus.iter_mut().zip(q) {
                *a = (x as f64 / teiler) as f32;
            }
        }
        19 => aus.copy_from_slice(std::slice::from_raw_parts(p as *const f32, n)),
        20 => {
            let q = std::slice::from_raw_parts(p as *const f64, n);
            for (a, &x) in aus.iter_mut().zip(q) {
                *a = x as f32;
            }
        }
        _ => aus.fill(0.0),
    }
}

#[inline]
unsafe fn schreiben(p: *mut c_void, typ: i32, ein: &[f32]) {
    let n = ein.len();
    match typ {
        16 => {
            let q = std::slice::from_raw_parts_mut(p as *mut i16, n);
            for (z, &x) in q.iter_mut().zip(ein) {
                *z = (x.clamp(-1.0, 1.0) * 32767.0).round() as i16;
            }
        }
        17 => {
            let q = std::slice::from_raw_parts_mut(p as *mut u8, n * 3);
            for (i, &x) in ein.iter().enumerate() {
                let v = (x.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32;
                q[i * 3] = v as u8;
                q[i * 3 + 1] = (v >> 8) as u8;
                q[i * 3 + 2] = (v >> 16) as u8;
            }
        }
        18 | 24..=27 => {
            let faktor = match typ {
                24 => 32_767.0,
                25 => 131_071.0,
                26 => 524_287.0,
                27 => 8_388_607.0,
                _ => 2_147_483_647.0,
            };
            let q = std::slice::from_raw_parts_mut(p as *mut i32, n);
            for (z, &x) in q.iter_mut().zip(ein) {
                *z = (x.clamp(-1.0, 1.0) as f64 * faktor).round() as i32;
            }
        }
        19 => std::slice::from_raw_parts_mut(p as *mut f32, n).copy_from_slice(ein),
        20 => {
            let q = std::slice::from_raw_parts_mut(p as *mut f64, n);
            for (z, &x) in q.iter_mut().zip(ein) {
                *z = x as f64;
            }
        }
        _ => {}
    }
}

unsafe extern "C" fn puffer_wechsel(index: i32, _direkt: i32) {
    RUFE.fetch_add(1, Relaxed);
    let p = LAUF.load(Acquire);
    if p.is_null() {
        return;
    }
    let l = &mut *p;
    let i = (index & 1) as usize;
    let n = l.block;
    if !l.eingaenge.is_empty() {
        // Mischpult mit Effekten: jeden Eingang bearbeiten und in die Summe legen
        l.sender.mischer.beginnen(n);
        for (c, k) in l.eingaenge.iter().enumerate() {
            lesen(k.puffer[i], k.typ, &mut l.kanal_tmp[..n]);
            l.sender.mischer.kanal(c, &l.kanal_tmp[..n], &l.geteilt);
        }
        l.sender.mischer.beenden(&l.geteilt);
        l.sender.senden_aus_mischer();
    }
    if l.ausgang[0].is_some() {
        l.wiedergabe.fuellen(&mut l.paare_aus[..n * 2]);
        // Ausgangspaar: links/rechts; nur ein Ausgang: Mono-Summe
        let zwei = l.ausgang[1].is_some();
        for (s, k) in l.ausgang.iter().enumerate() {
            let Some(k) = k else { continue };
            for (j, lr) in l.paare_aus[..n * 2].chunks_exact(2).enumerate() {
                l.mono_aus[j] = if zwei { lr[s] } else { 0.5 * (lr[0] + lr[1]) };
            }
            schreiben(k.puffer[i], k.typ, &l.mono_aus[..n]);
        }
    }
    if l.output_ready {
        ((*(*l.treiber).vtbl).output_ready)(l.treiber);
    }
}

unsafe extern "C" fn puffer_wechsel_zeit(params: *mut c_void, index: i32, direkt: i32) -> *mut c_void {
    puffer_wechsel(index, direkt);
    params
}

unsafe extern "C" fn rate_geaendert(_r: f64) {
    NEUSTART.store(true, Relaxed);
}

unsafe extern "C" fn nachricht(selektor: i32, wert: i32, _m: *mut c_void, _o: *mut f64) -> i32 {
    match selektor {
        1 => matches!(wert, 2..=6) as i32, // kAsioSelectorSupported
        2 => 2,                            // kAsioEngineVersion
        3 | 4 => {
            // kAsioResetRequest, kAsioBufferSizeChange: neu einrichten
            NEUSTART.store(true, Relaxed);
            1
        }
        5 => 1, // kAsioResyncRequest
        6 => {
            LATENZ_NEU.store(true, Relaxed);
            1
        }
        _ => 0, // u. a. kAsioSupportsTimeInfo: nein -> bufferSwitch
    }
}

static RUECKRUFE: Rueckrufe = Rueckrufe {
    buffer_switch: puffer_wechsel,
    sample_rate_did_change: rate_geaendert,
    asio_message: nachricht,
    buffer_switch_time_info: puffer_wechsel_zeit,
};

pub struct Auftrag {
    pub treiber: TreiberInfo,
    pub eingang: usize,
    pub ausgang: usize,
    /// 0 = Vorgabe des Treibers
    pub block: u32,
}

struct Eingerichtet {
    rate: f64,
    block: usize,
    infos: Vec<PufferInfo>,
    typen: Vec<i32>,
    beschreibung: String,
}

unsafe fn einrichten(t: &Treiber, a: &Auftrag) -> Result<Eingerichtet, String> {
    let rate = t.rate_waehlen();
    let b = t.bloecke();
    let block = if a.block > 0 && b.moeglich.contains(&a.block) { a.block } else { b.bevorzugt };
    let (mut n_ein, mut n_aus) = (0i32, 0i32);
    (t.v().get_channels)(t.p, &mut n_ein, &mut n_aus);
    let mut infos = Vec::new();
    // alle Eingänge (das Mischpult wählt aus, ohne den Treiber neu einzurichten)
    for k in 0..n_ein.clamp(0, crate::gemeinsam::MAX_KANAELE as i32) {
        infos.push(PufferInfo { is_input: 1, channel_num: k, buffers: [null_mut(); 2] });
    }
    for k in [a.ausgang, a.ausgang + 1] {
        if (k as i32) < n_aus {
            infos.push(PufferInfo { is_input: 0, channel_num: k as i32, buffers: [null_mut(); 2] });
        }
    }
    if infos.is_empty() {
        return Err("Der ASIO-Treiber meldet keine Kanäle".into());
    }
    let mut typen = Vec::new();
    for pi in &infos {
        let mut info = KanalInfo { channel: pi.channel_num, is_input: pi.is_input, is_active: 0, channel_group: 0, typ: -1, name: [0; 32] };
        (t.v().get_channel_info)(t.p, &mut info);
        if !typ_unterstuetzt(info.typ) {
            return Err(format!("Sample-Format {} des ASIO-Treibers wird nicht unterstützt", info.typ));
        }
        typen.push(info.typ);
    }
    let r = (t.v().create_buffers)(t.p, infos.as_mut_ptr(), infos.len() as i32, block as i32, &RUECKRUFE);
    if r != ASE_OK {
        return Err(format!("ASIO-Puffer ({block} Samples) konnten nicht angelegt werden (Fehler {r})"));
    }
    let beschreibung = format!(
        "ASIO {}, {:.1} kHz, Block {} Samples = {:.2} ms, {}",
        t.name,
        rate / 1000.0,
        block,
        block as f64 * 1000.0 / rate,
        typ_name(typen[0])
    );
    Ok(Eingerichtet { rate, block: block as usize, infos, typen, beschreibung })
}

/// Unsichtbares Fenster, das dem aktuellen Thread gehört (für init() des Treibers)
pub struct EigenesFenster(pub HWND);

impl EigenesFenster {
    pub fn neu() -> Self {
        use windows::Win32::UI::WindowsAndMessaging::*;
        unsafe {
            EigenesFenster(
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    windows::core::w!("STATIC"),
                    windows::core::w!("Jamcenter ASIO"),
                    WS_POPUP,
                    0,
                    0,
                    0,
                    0,
                    HWND::default(),
                    HMENU::default(),
                    windows::Win32::Foundation::HINSTANCE::default(),
                    None,
                )
                .unwrap_or_default(),
            )
        }
    }
}

impl Drop for EigenesFenster {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(self.0);
        }
    }
}

fn nachrichten_verarbeiten() {
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, HWND::default(), 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Puffer-Infos auf Kanalpuffer für den Callback verteilen
fn kanalpuffer(e: &Eingerichtet) -> (Vec<KanalPuffer>, [Option<KanalPuffer>; 2]) {
    let mut eingang = Vec::new();
    let mut ausgang = [None, None];
    for (pi, &typ) in e.infos.iter().zip(&e.typen) {
        let k = KanalPuffer { puffer: pi.buffers, typ };
        if pi.is_input != 0 {
            eingang.push(k);
        } else if ausgang[0].is_none() {
            ausgang[0] = Some(k);
        } else {
            ausgang[1] = Some(k);
        }
    }
    (eingang, ausgang)
}

const KEINE_DATEN: &str = "liefert keine Daten. Im ASIO-Panel prüfen, ob das Gerät eingeschaltet ist und nicht von Windows \
                           oder einem anderen Programm benutzt wird – oder „Windows-Audio“ wählen.";

/// Läuft im eigenen Thread (STA): lädt den Treiber, richtet ihn ein und startet ihn.
/// Bei Reset-Anforderung des Treibers (z. B. neue Blockgröße im Panel) wird neu eingerichtet.
pub fn laufen(a: Auftrag, g: Arc<Geteilt>, mut sender: Sender, mut wiedergabe: Wiedergabe) {
    let _com = crate::audio::Com::sta();
    let fenster = EigenesFenster::neu();
    let t = match Treiber::laden(&a.treiber, fenster.0) {
        Ok(t) => t,
        Err(e) => {
            g.fehler(e);
            return;
        }
    };
    let namen: Vec<String> = t.kanaele().eingaenge.into_iter().enumerate().map(|(i, n)| if n.is_empty() { format!("Eingang {}", i + 1) } else { n }).collect();
    g.kanaele_geoeffnet(namen, Some(a.eingang));
    unsafe {
        while !g.stop.load(Relaxed) {
            NEUSTART.store(false, Relaxed);
            let e = match einrichten(&t, &a) {
                Ok(e) => e,
                Err(x) => {
                    g.fehler(x);
                    return;
                }
            };
            let rate = e.rate.round() as u32;
            sender.rate_setzen(rate);
            wiedergabe.rate_setzen(rate);
            let (eingang, ausgang) = kanalpuffer(&e);
            // Ausgangspuffer vorab mit Stille füllen
            let stille = vec![0.0f32; e.block];
            for k in ausgang.iter().flatten() {
                for &p in &k.puffer {
                    schreiben(p, k.typ, &stille);
                }
            }
            let output_ready = (t.v().output_ready)(t.p) == ASE_OK;
            let lauf = Box::new(Lauf {
                eingaenge: eingang,
                kanal_tmp: vec![0.0; e.block],
                geteilt: g.clone(),
                ausgang,
                block: e.block,
                sender,
                wiedergabe,
                mono_aus: vec![0.0; e.block],
                paare_aus: vec![0.0; e.block * 2],
                output_ready,
                treiber: t.p,
            });
            LAUF.store(Box::into_raw(lauf), Release);
            let latenz = |t: &Treiber| {
                let (mut li, mut lo) = (0i32, 0i32);
                (t.v().get_latencies)(t.p, &mut li, &mut lo);
                let ms = |x: i32| (x.max(0) as f64 * 1000.0 / e.rate) as f32;
                g.anzeige.ein_latenz_ms.set(ms(li));
                g.anzeige.aus_latenz_ms.set(ms(lo));
            };
            latenz(&t);
            *g.anzeige.ein_info.lock().unwrap() = e.beschreibung.clone();
            *g.anzeige.aus_info.lock().unwrap() = e.beschreibung.clone();
            RUFE.store(0, Relaxed);
            let start = (t.v().start)(t.p);
            if start == ASE_OK {
                let beginn = std::time::Instant::now();
                let mut gemeldet = false;
                while !g.stop.load(Relaxed) && !NEUSTART.load(Relaxed) {
                    nachrichten_verarbeiten();
                    if PANEL_OEFFNEN.swap(false, Relaxed) {
                        t.panel();
                    }
                    if LATENZ_NEU.swap(false, Relaxed) {
                        latenz(&t);
                    }
                    // Überwachung: Treiber gestartet, aber keine Rückrufe -> klare Meldung statt Stille
                    let rufe = RUFE.load(Relaxed);
                    if rufe == 0 && !gemeldet && beginn.elapsed() > Duration::from_millis(1500) {
                        g.fehler(format!("ASIO-Treiber „{}“ {KEINE_DATEN}", t.name));
                        gemeldet = true;
                    } else if rufe > 0 && gemeldet {
                        *g.anzeige.fehler.lock().unwrap() = None;
                        gemeldet = false;
                    }
                    std::thread::sleep(Duration::from_millis(15));
                }
                (t.v().stop)(t.p);
            } else {
                g.fehler(format!("ASIO-Treiber startet nicht (Fehler {start}) – ist das Interface angeschlossen?"));
            }
            (t.v().dispose_buffers)(t.p);
            let lauf = Box::from_raw(LAUF.swap(null_mut(), AcqRel));
            sender = lauf.sender;
            wiedergabe = lauf.wiedergabe;
            if start != ASE_OK {
                return;
            }
        }
    }
}

/// Für „--diagnose“: Treiber, Kanäle, Blockgrößen, gemeldete Latenzen und ein 2-s-Lauf
/// mit kleinster Blockgröße (Ausgang = Stille).
pub fn diagnose(g: Arc<Geteilt>) -> String {
    let _com = crate::audio::Com::sta();
    let mut text = String::from("== ASIO ==\n");
    let liste = treiber_liste();
    if liste.is_empty() {
        text.push_str("keine ASIO-Treiber installiert\n");
    }
    for info in liste {
        text.push_str(&format!("{}\n", info.name));
        let fenster = EigenesFenster::neu();
        let t = match Treiber::laden(&info, fenster.0) {
            Ok(t) => t,
            Err(e) => {
                text.push_str(&format!("   Fehler: {e}\n"));
                continue;
            }
        };
        let k = t.kanaele();
        text.push_str(&format!("   Eingänge: {:?}\n   Ausgänge: {:?}\n", k.eingaenge, k.ausgaenge));
        let b = t.bloecke();
        text.push_str(&format!("   Blockgrößen: min {} max {} Vorgabe {}\n", b.min, b.max, b.bevorzugt));
        let a = Auftrag { treiber: info.clone(), eingang: 0, ausgang: 0, block: b.min };
        unsafe {
            let e = match einrichten(&t, &a) {
                Ok(e) => e,
                Err(x) => {
                    text.push_str(&format!("   Fehler: {x}\n"));
                    continue;
                }
            };
            let (mut li, mut lo) = (0i32, 0i32);
            (t.v().get_latencies)(t.p, &mut li, &mut lo);
            text.push_str(&format!(
                "   {} | Treiberlatenz ein {:.2} ms, aus {:.2} ms\n",
                e.beschreibung,
                li as f64 * 1000.0 / e.rate,
                lo as f64 * 1000.0 / e.rate
            ));
            let (_prod, cons) = rtrb::RingBuffer::new(1024);
            let sock = Arc::new(std::net::UdpSocket::bind("127.0.0.1:0").unwrap());
            let krypto = crate::netz::Krypto::neu(&[0; 32], 1);
            let rate = e.rate.round() as u32;
            let (eingang, ausgang) = kanalpuffer(&e);
            let lauf = Box::new(Lauf {
                eingaenge: eingang,
                kanal_tmp: vec![0.0; e.block],
                geteilt: g.clone(),
                ausgang,
                block: e.block,
                sender: Sender::neu(sock, krypto, g.clone(), rate),
                wiedergabe: Wiedergabe::neu(cons, rate, g.clone()),
                mono_aus: vec![0.0; e.block],
                paare_aus: vec![0.0; e.block * 2],
                output_ready: (t.v().output_ready)(t.p) == ASE_OK,
                treiber: t.p,
            });
            LAUF.store(Box::into_raw(lauf), Release);
            RUFE.store(0, Relaxed);
            g.anzeige.pegel_ein.nehmen();
            let s = (t.v().start)(t.p);
            let beginn = std::time::Instant::now();
            while beginn.elapsed() < Duration::from_secs(2) {
                nachrichten_verarbeiten();
                std::thread::sleep(Duration::from_millis(10));
            }
            let (mut pos, mut zeit) = (0u64, 0u64);
            (t.v().get_sample_position)(t.p, &mut pos, &mut zeit);
            (t.v().stop)(t.p);
            let n = RUFE.load(Relaxed);
            (t.v().dispose_buffers)(t.p);
            drop(Box::from_raw(LAUF.swap(null_mut(), AcqRel)));
            let pegel = g.anzeige.pegel_ein.nehmen();
            text.push_str(&format!(
                "   Lauftest 2 s: start {} | {} Rückrufe (erwartet ≈ {:.0}) | Treiberposition {} | Eingangspegel {:.1} dBFS\n",
                if s == ASE_OK { "ok" } else { "FEHLER" },
                n,
                2.0 * e.rate / e.block as f64,
                ((pos & 0xffff_ffff) << 32) | (pos >> 32),
                20.0 * pegel.max(1e-9).log10()
            ));
            if n == 0 {
                text.push_str(&format!("   -> Treiber {KEINE_DATEN}\n"));
            }
        }
    }
    text
}
