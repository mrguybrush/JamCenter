//! WASAPI-Anbindung: Geräte auflisten und Ströme mit möglichst kleiner Periode öffnen.
//!
//! Reihenfolge beim Öffnen (jeweils ereignisgesteuert, eigener Thread mit MMCSS „Pro Audio“):
//!  1. exklusiv (falls gewünscht): kleinste Geräteperiode, kein Windows-Mixer dazwischen
//!  2. geteilt mit niedriger Latenz (IAudioClient3, kleinere Engine-Periode, z. B. 2,7 ms)
//!  3. geteilt Standard (10 ms Engine-Periode)
//! Im geteilten Modus wird der RAW-Modus angefordert: keine Windows-Effekte
//! (Rauschunterdrückung, AGC …), die Musik verfälschen und Latenz kosten.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use windows::core::{implement, Interface, HSTRING, PCWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE, S_OK, WAIT_TIMEOUT};
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc};
use windows::Win32::System::Com::*;
use windows::Win32::System::Com::{IAgileObject, IAgileObject_Impl};
use windows::Win32::System::Threading::{AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW, GetCurrentProcessId, SetEvent, WaitForSingleObject};

pub struct Com;

impl Com {
    pub fn init() -> Com {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        Com
    }
    /// Single-Threaded Apartment: nötig für ASIO-Treiber (und für die Oberfläche)
    pub fn sta() -> Com {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }
        Com
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        unsafe { CoUninitialize() }
    }
}

/// Thread beim Multimedia-Klassenplaner anmelden (Echtzeitpriorität ohne Adminrechte)
pub struct Mmcss(HANDLE);

impl Mmcss {
    pub fn neu() -> Option<Mmcss> {
        let mut idx = 0u32;
        unsafe { AvSetMmThreadCharacteristicsW(windows::core::w!("Pro Audio"), &mut idx).ok().map(Mmcss) }
    }
}

impl Drop for Mmcss {
    fn drop(&mut self) {
        unsafe {
            let _ = AvRevertMmThreadCharacteristics(self.0);
        }
    }
}

#[derive(Clone, Debug)]
pub struct Geraet {
    pub id: String,
    pub name: String,
}

fn fehlertext(x: windows::core::Error) -> String {
    let c = x.code();
    if c == AUDCLNT_E_DEVICE_INVALIDATED {
        "Gerät wurde getrennt".into()
    } else if c == AUDCLNT_E_DEVICE_IN_USE {
        "Gerät wird von einem anderen Programm exklusiv benutzt".into()
    } else if c == AUDCLNT_E_EXCLUSIVE_MODE_NOT_ALLOWED {
        "exklusiver Zugriff ist in den Windows-Soundeinstellungen verboten".into()
    } else if c == AUDCLNT_E_UNSUPPORTED_FORMAT {
        "Format wird nicht unterstützt".into()
    } else {
        format!("{} (0x{:08X})", x.message().trim(), c.0 as u32)
    }
}

unsafe fn pwstr_nehmen(p: windows::core::PWSTR) -> String {
    let s = p.to_string().unwrap_or_default();
    CoTaskMemFree(Some(p.0 as _));
    s
}

unsafe fn geraet_name(d: &IMMDevice) -> Option<String> {
    let store = d.OpenPropertyStore(STGM_READ).ok()?;
    let mut pv = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
    let p = PropVariantToStringAlloc(&pv).ok();
    let _ = PropVariantClear(&mut pv);
    Some(pwstr_nehmen(p?))
}

fn auflister() -> Result<IMMDeviceEnumerator, String> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(fehlertext) }
}

pub fn geraete(aufnahme: bool) -> Vec<Geraet> {
    let mut v = Vec::new();
    unsafe {
        let Ok(en) = auflister() else { return v };
        let flow = if aufnahme { eCapture } else { eRender };
        let Ok(col) = en.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) else { return v };
        for i in 0..col.GetCount().unwrap_or(0) {
            let Ok(d) = col.Item(i) else { continue };
            let Ok(id) = d.GetId() else { continue };
            let id = pwstr_nehmen(id);
            let name = geraet_name(&d).unwrap_or_else(|| id.clone());
            v.push(Geraet { id, name });
        }
    }
    v.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    v
}

/// Kanalzahl eines Aufnahmegeräts (Mischformat), für das Mischpult vor dem Verbinden
pub fn kanalzahl(id: &str) -> usize {
    unsafe {
        let Ok(en) = auflister() else { return 1 };
        let g = if id.is_empty() { en.GetDefaultAudioEndpoint(eCapture, eConsole) } else { en.GetDevice(&HSTRING::from(id)) };
        let Ok(g) = g else { return 1 };
        let Ok(c) = g.Activate::<IAudioClient>(CLSCTX_ALL, None) else { return 1 };
        let Ok(mix) = c.GetMixFormat() else { return 1 };
        let k = format_lesen(mix).map(|f| f.kanaele).unwrap_or(1);
        CoTaskMemFree(Some(mix as _));
        k.max(1)
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Typ {
    F32,
    I16,
    /// 32-Bit-Container (auch 24 gültige Bits, linksbündig)
    I32,
    /// 24 Bit gepackt (3 Byte)
    I24,
}

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub struct Format {
    pub rate: u32,
    pub kanaele: usize,
    pub typ: Typ,
    pub bits: u16,
}

unsafe fn format_lesen(p: *const WAVEFORMATEX) -> Option<Format> {
    let w = std::ptr::read_unaligned(p);
    let tag = w.wFormatTag;
    let bits = w.wBitsPerSample;
    let float = match tag {
        0xFFFE => {
            let e = std::ptr::read_unaligned(p as *const WAVEFORMATEXTENSIBLE);
            let sub = e.SubFormat;
            if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                true
            } else if sub == KSDATAFORMAT_SUBTYPE_PCM {
                false
            } else {
                return None;
            }
        }
        3 => true,
        1 => false,
        _ => return None,
    };
    let typ = match (float, bits) {
        (true, 32) => Typ::F32,
        (false, 16) => Typ::I16,
        (false, 24) => Typ::I24,
        (false, 32) => Typ::I32,
        _ => return None,
    };
    Some(Format { rate: w.nSamplesPerSec, kanaele: w.nChannels as usize, typ, bits })
}

fn wfx_bauen(rate: u32, kanaele: u16, bits: u16, gueltig: u16, float: bool) -> WAVEFORMATEXTENSIBLE {
    let block = kanaele * bits / 8;
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: 0xFFFE,
            nChannels: kanaele,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * block as u32,
            nBlockAlign: block,
            wBitsPerSample: bits,
            cbSize: 22,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: gueltig },
        dwChannelMask: match kanaele {
            1 => 0x4,
            2 => 0x3,
            _ => 0,
        },
        SubFormat: if float { KSDATAFORMAT_SUBTYPE_IEEE_FLOAT } else { KSDATAFORMAT_SUBTYPE_PCM },
    }
}

#[allow(dead_code)]
pub struct Strom {
    client: IAudioClient,
    aufnahme: Option<IAudioCaptureClient>,
    wiedergabe: Option<IAudioRenderClient>,
    ereignis: HANDLE,
    pub format: Format,
    pub puffer: u32,
    pub periode: u32,
    /// Wiedergabe: so viele Frames werden im Gerätepuffer gehalten
    pub vorlauf: u32,
    pub exklusiv: bool,
    pub latenz_ms: f32,
    pub beschreibung: String,
    pub name: String,
}

impl Drop for Strom {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.ereignis);
        }
    }
}

unsafe fn roh_setzen(c: &IAudioClient2) {
    let p = AudioClientProperties {
        cbSize: std::mem::size_of::<AudioClientProperties>() as u32,
        bIsOffload: false.into(),
        eCategory: AudioCategory_Media,
        Options: AUDCLNT_STREAMOPTIONS_RAW,
    };
    let _ = c.SetClientProperties(&p);
}

unsafe fn fertigstellen(client: IAudioClient, format: Format, aufnahme: bool, exklusiv: bool, periode: u32, art: &str) -> Result<Strom, String> {
    let ereignis = CreateEventW(None, false, false, PCWSTR::null()).map_err(fehlertext)?;
    let strom_fehler = |e| {
        let _ = CloseHandle(ereignis);
        fehlertext(e)
    };
    client.SetEventHandle(ereignis).map_err(strom_fehler)?;
    let puffer = client.GetBufferSize().map_err(strom_fehler)?;
    let latenz_hns = client.GetStreamLatency().unwrap_or(0).max(0);
    let (a, w) = if aufnahme {
        (Some(client.GetService::<IAudioCaptureClient>().map_err(strom_fehler)?), None)
    } else {
        (None, Some(client.GetService::<IAudioRenderClient>().map_err(strom_fehler)?))
    };
    let periode = periode.clamp(16, puffer.max(16));
    let vorlauf = if exklusiv { puffer } else { puffer.min(periode * 2) };
    let ms = |f: u32| f as f32 * 1000.0 / format.rate as f32;
    let latenz_ms = if aufnahme { ms(periode) } else { ms(vorlauf) } + latenz_hns as f32 / 10_000.0;
    let beschreibung = format!(
        "{art}, {:.1} kHz, {} Kanal/Kanäle, Periode {:.1} ms",
        format.rate as f32 / 1000.0,
        format.kanaele,
        ms(periode)
    )
    .replace("1 Kanal/Kanäle", "1 Kanal")
    .replace("Kanal/Kanäle", "Kanäle");
    Ok(Strom {
        client,
        aufnahme: a,
        wiedergabe: w,
        ereignis,
        format,
        puffer,
        periode,
        vorlauf,
        exklusiv,
        latenz_ms,
        beschreibung,
        name: String::new(),
    })
}

unsafe fn oeffnen_exklusiv(g: &IMMDevice, aufnahme: bool) -> Result<Strom, String> {
    let client: IAudioClient = g.Activate(CLSCTX_ALL, None).map_err(fehlertext)?;
    let mix = client.GetMixFormat().map_err(fehlertext)?;
    let mixf = format_lesen(mix);
    CoTaskMemFree(Some(mix as _));
    let mix_kanaele = mixf.map(|f| f.kanaele as u16).unwrap_or(2);
    let mix_rate = mixf.map(|f| f.rate).unwrap_or(48_000);
    let mut def = 0i64;
    let mut min = 0i64;
    client.GetDevicePeriod(Some(&mut def), Some(&mut min)).map_err(fehlertext)?;

    let mut raten = vec![48_000, mix_rate, 44_100, 96_000];
    raten.dedup();
    let mut kanaele: Vec<u16> = if aufnahme { vec![mix_kanaele, 2, 1] } else { vec![2, mix_kanaele, 1] };
    kanaele.dedup();
    // (Bits, gültige Bits, Gleitkomma)
    let typen = [(32u16, 32u16, true), (32, 24, false), (24, 24, false), (16, 16, false), (32, 32, false)];
    let mut gefunden = None;
    'suche: for &r in &raten {
        for &k in &kanaele {
            for &(b, v, f) in &typen {
                let wfx = wfx_bauen(r, k, b, v, f);
                if client.IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, &wfx as *const _ as *const WAVEFORMATEX, None) == S_OK {
                    gefunden = Some(wfx);
                    break 'suche;
                }
            }
        }
    }
    let wfx = gefunden.ok_or_else(|| "kein passendes Format für exklusiven Zugriff".to_string())?;
    let format = format_lesen(&wfx as *const _ as *const WAVEFORMATEX).ok_or("Format")?;
    let pwfx = &wfx as *const _ as *const WAVEFORMATEX;
    // kleinste Geräteperiode, aber nicht unter 1 ms
    let mut periode = min.max(10_000);
    let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
    let mut client = client;
    if let Err(err) = client.Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, flags, periode, periode, pwfx, None) {
        if err.code() != AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED {
            return Err(fehlertext(err));
        }
        let frames = client.GetBufferSize().map_err(fehlertext)?;
        periode = (10_000_000f64 * frames as f64 / format.rate as f64 + 0.5) as i64;
        client = g.Activate(CLSCTX_ALL, None).map_err(fehlertext)?;
        client.Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, flags, periode, periode, pwfx, None).map_err(fehlertext)?;
    }
    let frames = client.GetBufferSize().map_err(fehlertext)?;
    fertigstellen(client, format, aufnahme, true, frames, "exklusiv")
}

unsafe fn oeffnen_geteilt_niedrig(g: &IMMDevice, aufnahme: bool) -> Result<Strom, String> {
    let client: IAudioClient3 = g.Activate(CLSCTX_ALL, None).map_err(fehlertext)?;
    roh_setzen(&client);
    let mix = client.GetMixFormat().map_err(fehlertext)?;
    let Some(format) = format_lesen(mix) else {
        CoTaskMemFree(Some(mix as _));
        return Err("unbekanntes Mischformat".into());
    };
    let (mut def, mut fund, mut min, mut max) = (0u32, 0u32, 0u32, 0u32);
    let r = client.GetSharedModeEnginePeriod(mix, &mut def, &mut fund, &mut min, &mut max);
    if r.is_err() || min == 0 || min >= def {
        CoTaskMemFree(Some(mix as _));
        return Err("Treiber bietet keine kleinere Periode".into());
    }
    let r = client.InitializeSharedAudioStream(AUDCLNT_STREAMFLAGS_EVENTCALLBACK, min, mix, None);
    CoTaskMemFree(Some(mix as _));
    r.map_err(fehlertext)?;
    // tatsächliche Periode (ein anderes Programm kann die Engine schon größer betreiben)
    let mut periode = min;
    let mut pf: *mut WAVEFORMATEX = std::ptr::null_mut();
    let mut akt = 0u32;
    if client.GetCurrentSharedModeEnginePeriod(&mut pf, &mut akt).is_ok() {
        if akt > 0 {
            periode = akt;
        }
        if !pf.is_null() {
            CoTaskMemFree(Some(pf as _));
        }
    }
    let client: IAudioClient = client.cast().map_err(fehlertext)?;
    fertigstellen(client, format, aufnahme, false, periode, "geteilt, niedrige Latenz")
}

unsafe fn oeffnen_geteilt(g: &IMMDevice, aufnahme: bool) -> Result<Strom, String> {
    let client: IAudioClient = g.Activate(CLSCTX_ALL, None).map_err(fehlertext)?;
    if let Ok(c2) = client.cast::<IAudioClient2>() {
        roh_setzen(&c2);
    }
    let mix = client.GetMixFormat().map_err(fehlertext)?;
    let Some(format) = format_lesen(mix) else {
        CoTaskMemFree(Some(mix as _));
        return Err("unbekanntes Mischformat".into());
    };
    let mut def = 0i64;
    let _ = client.GetDevicePeriod(Some(&mut def), None);
    let r = client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, 0, 0, mix, None);
    CoTaskMemFree(Some(mix as _));
    r.map_err(fehlertext)?;
    let periode = ((def.max(10_000) as f64) * format.rate as f64 / 10_000_000.0) as u32;
    fertigstellen(client, format, aufnahme, false, periode, "geteilt")
}

/// Loopback eines Ausgabegeräts: alles, was dort gerade abgespielt wird (geteilt, Mischformat)
pub fn oeffnen_loopback(id: &str) -> Result<Strom, String> {
    unsafe {
        let en = auflister()?;
        let g = if id.is_empty() { en.GetDefaultAudioEndpoint(eRender, eConsole) } else { en.GetDevice(&HSTRING::from(id)) }
            .map_err(|e| format!("Ausgang nicht gefunden: {}", fehlertext(e)))?;
        let name = geraet_name(&g).unwrap_or_default();
        let client: IAudioClient = g.Activate(CLSCTX_ALL, None).map_err(fehlertext)?;
        let mix = client.GetMixFormat().map_err(fehlertext)?;
        let Some(format) = format_lesen(mix) else {
            CoTaskMemFree(Some(mix as _));
            return Err("unbekanntes Mischformat".into());
        };
        let mut def = 0i64;
        let _ = client.GetDevicePeriod(Some(&mut def), None);
        let r = client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK, 0, 0, mix, None);
        CoTaskMemFree(Some(mix as _));
        r.map_err(fehlertext)?;
        let periode = ((def.max(10_000) as f64) * format.rate as f64 / 10_000_000.0) as u32;
        let mut s = fertigstellen(client, format, true, false, periode, "Loopback")?;
        s.name = name;
        Ok(s)
    }
}

/// Meldet das Ende von ActivateAudioInterfaceAsync
#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct Aktiviert(HANDLE);

impl IAgileObject_Impl for Aktiviert_Impl {}

impl IActivateAudioInterfaceCompletionHandler_Impl for Aktiviert_Impl {
    fn ActivateCompleted(&self, _op: Option<&IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        unsafe { SetEvent(self.0) }
    }
}

/// PROPVARIANT mit VT_BLOB (das windows-Crate baut keinen)
#[repr(C)]
struct BlobVariant {
    vt: u16,
    r1: u16,
    r2: u16,
    r3: u16,
    groesse: u32,
    daten: *const u8,
    _rest: usize,
}

/// Windows-Ton ohne Jamcenter selbst (Prozess-Loopback, Windows 10 ab 2004): alles, was andere
/// Programme abspielen – nicht aber der Ton der Gegenstelle und das Metronom (kein Echo).
pub fn oeffnen_system_ohne_mich() -> Result<Strom, String> {
    unsafe {
        let mut params = AUDIOCLIENT_ACTIVATION_PARAMS { ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, ..Default::default() };
        params.Anonymous.ProcessLoopbackParams = AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
            TargetProcessId: GetCurrentProcessId(),
            ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
        };
        let pv = BlobVariant {
            vt: 65, // VT_BLOB
            r1: 0,
            r2: 0,
            r3: 0,
            groesse: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            daten: &params as *const _ as *const u8,
            _rest: 0,
        };
        let ereignis = CreateEventW(None, false, false, PCWSTR::null()).map_err(fehlertext)?;
        let handler: IActivateAudioInterfaceCompletionHandler = Aktiviert(ereignis).into();
        let op = ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&pv as *const _ as *const windows::core::PROPVARIANT),
            &handler,
        );
        let op = match op {
            Ok(o) => o,
            Err(e) => {
                let _ = CloseHandle(ereignis);
                return Err(format!("Windows-Ton nicht verfügbar (ab Windows 10 Version 2004): {}", fehlertext(e)));
            }
        };
        let gewartet = WaitForSingleObject(ereignis, 5000);
        let _ = CloseHandle(ereignis);
        if gewartet == WAIT_TIMEOUT {
            return Err("Windows-Ton: keine Antwort von Windows".into());
        }
        let mut hr = windows::core::HRESULT(0);
        let mut unk: Option<windows::core::IUnknown> = None;
        op.GetActivateResult(&mut hr, &mut unk).map_err(fehlertext)?;
        hr.ok().map_err(|e| format!("Windows-Ton: {}", fehlertext(e)))?;
        let client: IAudioClient = unk.ok_or("Windows-Ton: kein Audio-Client")?.cast().map_err(fehlertext)?;
        // Prozess-Loopback kennt kein Mischformat: 48 kHz Stereo, Gleitkomma vorgeben
        let wfx = wfx_bauen(48_000, 2, 32, 32, true);
        let format = format_lesen(&wfx as *const _ as *const WAVEFORMATEX).ok_or("Format")?;
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                100_000,
                0,
                &wfx as *const _ as *const WAVEFORMATEX,
                None,
            )
            .map_err(fehlertext)?;
        let mut s = fertigstellen(client, format, true, false, 480, "Windows-Ton ohne Jamcenter")?;
        s.name = "Windows-Ton".into();
        Ok(s)
    }
}

/// Gerät öffnen (leere ID = Windows-Standardgerät). Liefert auch bei Rückfall eine Beschreibung.
pub fn oeffnen(id: &str, aufnahme: bool, exklusiv: bool) -> Result<Strom, String> {
    unsafe {
        let en = auflister()?;
        let flow = if aufnahme { eCapture } else { eRender };
        let mut hinweis = "";
        let geraet = if id.is_empty() {
            en.GetDefaultAudioEndpoint(flow, eConsole)
        } else {
            en.GetDevice(&HSTRING::from(id)).or_else(|_| {
                hinweis = " (gewähltes Gerät fehlt, Standardgerät benutzt)";
                en.GetDefaultAudioEndpoint(flow, eConsole)
            })
        }
        .map_err(|e| format!("Kein {} gefunden: {}", if aufnahme { "Mikrofon" } else { "Ausgabegerät" }, fehlertext(e)))?;
        let name = geraet_name(&geraet).unwrap_or_default();
        let mut fehler = Vec::new();
        let versuche: [(bool, unsafe fn(&IMMDevice, bool) -> Result<Strom, String>); 3] =
            [(exklusiv, oeffnen_exklusiv), (true, oeffnen_geteilt_niedrig), (true, oeffnen_geteilt)];
        for (aktiv, f) in versuche {
            if !aktiv {
                continue;
            }
            match f(&geraet, aufnahme) {
                Ok(mut s) => {
                    s.name = name.clone();
                    if exklusiv && !s.exklusiv {
                        s.beschreibung.push_str(" – exklusiv nicht möglich");
                    }
                    s.beschreibung.push_str(hinweis);
                    return Ok(s);
                }
                Err(e) => fehler.push(e),
            }
        }
        Err(format!("{name}: {}", fehler.join("; ")))
    }
}

#[inline]
unsafe fn probe_lesen(p: *const u8, idx: usize, t: Typ) -> f32 {
    match t {
        Typ::F32 => (p as *const f32).add(idx).read_unaligned(),
        Typ::I16 => (p as *const i16).add(idx).read_unaligned() as f32 * (1.0 / 32768.0),
        Typ::I32 => (p as *const i32).add(idx).read_unaligned() as f32 * (1.0 / 2147483648.0),
        Typ::I24 => {
            let q = p.add(idx * 3);
            let v = (q.read() as i32) | ((q.add(1).read() as i32) << 8) | ((q.add(2).read() as i8 as i32) << 16);
            v as f32 * (1.0 / 8388608.0)
        }
    }
}

#[inline]
unsafe fn probe_schreiben(p: *mut u8, idx: usize, t: Typ, x: f32) {
    let x = x.clamp(-1.0, 1.0);
    match t {
        Typ::F32 => (p as *mut f32).add(idx).write_unaligned(x),
        Typ::I16 => (p as *mut i16).add(idx).write_unaligned((x * 32767.0).round() as i16),
        Typ::I32 => (p as *mut i32).add(idx).write_unaligned((x as f64 * 2147483647.0).round() as i32),
        Typ::I24 => {
            let v = (x * 8388607.0).round() as i32;
            let q = p.add(idx * 3);
            q.write(v as u8);
            q.add(1).write((v >> 8) as u8);
            q.add(2).write((v >> 16) as u8);
        }
    }
}

impl Strom {
    /// Aufnahme-Schleife: ruft `f` mit allen Kanälen (verschachtelt, Geräterate) und der
    /// Kanalzahl auf, bis `stop` gesetzt ist. Gemischt wird im Sender nach dem Mischpult.
    pub fn aufnahme_laufen(&self, stop: &AtomicBool, mut f: impl FnMut(&[f32], usize)) -> Result<(), String> {
        unsafe {
            let cap = self.aufnahme.as_ref().ok_or("kein Aufnahmestrom")?;
            let _mm = Mmcss::neu();
            self.client.Start().map_err(fehlertext)?;
            let mut mono: Vec<f32> = Vec::with_capacity(8192);
            let k = self.format.kanaele.max(1);
            while !stop.load(Relaxed) {
                if WaitForSingleObject(self.ereignis, 200) == WAIT_TIMEOUT {
                    // Lebenszeichen prüfen (Gerät getrennt?). Loopback liefert nichts, solange nichts
                    // spielt, und der Prozess-Loopback kennt GetCurrentPadding gar nicht (E_NOTIMPL).
                    if let Err(e) = self.client.GetCurrentPadding() {
                        if e.code() == AUDCLNT_E_DEVICE_INVALIDATED {
                            return Err(fehlertext(e));
                        }
                    }
                    continue;
                }
                loop {
                    let n = cap.GetNextPacketSize().map_err(fehlertext)?;
                    if n == 0 {
                        break;
                    }
                    let mut daten: *mut u8 = std::ptr::null_mut();
                    let mut frames = 0u32;
                    let mut flags = 0u32;
                    cap.GetBuffer(&mut daten, &mut frames, &mut flags, None, None).map_err(fehlertext)?;
                    mono.clear();
                    if flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0 || daten.is_null() {
                        mono.resize(frames as usize * k, 0.0);
                    } else {
                        for i in 0..frames as usize * k {
                            mono.push(probe_lesen(daten, i, self.format.typ));
                        }
                    }
                    cap.ReleaseBuffer(frames).map_err(fehlertext)?;
                    f(&mono, k);
                }
            }
            let _ = self.client.Stop();
            Ok(())
        }
    }

    /// Wiedergabe-Schleife: `f` füllt Stereo-Paare (Geräterate), bis `stop` gesetzt ist.
    pub fn wiedergabe_laufen(&self, stop: &AtomicBool, mut f: impl FnMut(&mut [f32])) -> Result<(), String> {
        unsafe {
            let ren = self.wiedergabe.as_ref().ok_or("kein Wiedergabestrom")?;
            let _mm = Mmcss::neu();
            let k = self.format.kanaele.max(1);
            // erster Puffer mit Stille, sonst knackt es beim Start
            let n = self.vorlauf.min(self.puffer);
            ren.GetBuffer(n).map_err(fehlertext)?;
            ren.ReleaseBuffer(n, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32).map_err(fehlertext)?;
            self.client.Start().map_err(fehlertext)?;
            // Block als Stereo-Paare (L, R)
            let mut paare = vec![0.0f32; self.puffer as usize * 2];
            while !stop.load(Relaxed) {
                if WaitForSingleObject(self.ereignis, 200) == WAIT_TIMEOUT {
                    self.client.GetCurrentPadding().map_err(fehlertext)?;
                    continue;
                }
                let frames = if self.exklusiv {
                    self.puffer
                } else {
                    let pad = self.client.GetCurrentPadding().map_err(fehlertext)?;
                    self.vorlauf.saturating_sub(pad)
                };
                if frames == 0 {
                    continue;
                }
                let block = &mut paare[..frames as usize * 2];
                f(block);
                let p = ren.GetBuffer(frames).map_err(fehlertext)?;
                for (i, lr) in block.chunks_exact(2).enumerate() {
                    for c in 0..k {
                        // Mono-Gerät: Summe; sonst Kanal 1 = links, 2 = rechts, weitere still
                        let x = match (k, c) {
                            (1, _) => 0.5 * (lr[0] + lr[1]),
                            (_, 0) => lr[0],
                            (_, 1) => lr[1],
                            _ => 0.0,
                        };
                        probe_schreiben(p, i * k + c, self.format.typ, x);
                    }
                }
                ren.ReleaseBuffer(frames, 0).map_err(fehlertext)?;
            }
            let _ = self.client.Stop();
            Ok(())
        }
    }
}

/// Text für „--diagnose“: alle Geräte mit den erreichbaren Perioden (spielt nichts ab).
pub fn diagnose() -> String {
    let _com = Com::init();
    let mut t = String::new();
    for aufnahme in [true, false] {
        t.push_str(if aufnahme { "== Mikrofone ==\n" } else { "== Ausgabegeräte ==\n" });
        let mut liste = geraete(aufnahme);
        liste.insert(0, Geraet { id: String::new(), name: "(Standardgerät)".into() });
        for g in liste {
            t.push_str(&format!("{}\n", g.name));
            for ex in [true, false] {
                match oeffnen(&g.id, aufnahme, ex) {
                    Ok(s) => t.push_str(&format!("   {}: {} | Puffer {} Frames | Latenz ≈ {:.1} ms\n", if ex { "Wunsch exklusiv" } else { "Wunsch geteilt " }, s.beschreibung, s.puffer, s.latenz_ms)),
                    Err(e) => t.push_str(&format!("   Fehler: {e}\n")),
                }
            }
        }
    }
    t
}

/// Prüfwerkzeug: liefert das Gerät wirklich so viele Frames, wie seine Rate verspricht?
pub fn takt_messen(id: &str, exklusiv: bool, sek: u64) {
    let _com = Com::init();
    let s = match oeffnen(id, true, exklusiv) {
        Ok(s) => s,
        Err(e) => {
            println!("Fehler: {e}");
            return;
        }
    };
    println!("{} | {} | Rate {} | Puffer {} | Latenz {:.1} ms", s.name, s.beschreibung, s.format.rate, s.puffer, s.latenz_ms);
    let stop = AtomicBool::new(false);
    let beginn = std::time::Instant::now();
    let mut frames = 0u64;
    let mut aufrufe = 0u64;
    let mut groessen = std::collections::BTreeMap::<usize, u32>::new();
    let mut naechste = 1u64;
    let _ = s.aufnahme_laufen(&stop, |b, k| {
        let n = b.len() / k.max(1);
        frames += n as u64;
        aufrufe += 1;
        *groessen.entry(n).or_default() += 1;
        let t = beginn.elapsed().as_secs_f64();
        if t >= naechste as f64 {
            println!("[{t:.1}s] {:.0} Frames/s (soll {}), {} Blöcke, Größen {:?}", frames as f64 / t, s.format.rate, aufrufe, groessen.iter().take(6).collect::<Vec<_>>());
            naechste += 1;
            if naechste > sek {
                stop.store(true, Relaxed);
            }
        }
    });
}
