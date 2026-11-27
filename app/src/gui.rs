//! Oberfläche: reines Win32 (keine GPU nötig – läuft auch auf alten Rechnern und in
//! Remote-Sitzungen), schwarzes Design mit selbst gezeichneten Knöpfen, Kippschaltern,
//! Reglern und Pegelanzeigen.

use crate::asio;
use crate::aufnahmen::{self, Eintrag};
use crate::audio::{self, Geraet};
use crate::gemeinsam::{self, uhr_us, AtomicF32, Einstellungen, Geteilt, KanalFx, EQ_BAENDER, EQ_VORGABE, MAX_KANAELE};
use crate::mischer::{self, KANAL_VORLAGEN};
use crate::puffer::KLANGFORMEN;
use crate::sitzung::{self, Sitzung};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_CAPTION_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, GetFocus, IsWindowEnabled, ReleaseCapture, SetCapture, SetFocus};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::Shell::{
    FOLDERID_Downloads, KNOWN_FOLDER_FLAG, SHGetKnownFolderPath,
    DefSubclassProc, RemoveWindowSubclass, SHBrowseForFolderW, SHGetPathFromIDListW, SetWindowSubclass, ShellExecuteW, BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS, BROWSEINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

// ---------------------------------------------------------------- Kennungen
const ID_RAUM: i32 = 101;
const ID_PASSWORT: i32 = 102;
const KEIN_SERVER: &str = "Bitte unter „Server“ die Adresse eines Jamcenter-Servers eintragen (beide Seiten denselben). Wie man einen eigenen Server einrichtet, steht unter „Eigener Server …“.";
const ID_NAME: i32 = 103;
const ID_SERVER: i32 = 104;
const ID_TREIBER: i32 = 110;
const ID_PANEL: i32 = 111;
const ID_EINGANG: i32 = 112;
const ID_MISCHPULT: i32 = 113;
const ID_VERST: i32 = 114;
const ID_STUMM: i32 = 115;
const ID_STEREO: i32 = 116;
const ID_AUSGANG: i32 = 117;
const ID_LAUT: i32 = 118;
const ID_EXKLUSIV: i32 = 119;
const ID_BLOCK: i32 = 120;
const ID_M_AN: i32 = 130;
const ID_M_TAP: i32 = 131;
const ID_M_BPM: i32 = 132;
const ID_M_TAKT: i32 = 133;
const ID_M_KLANG: i32 = 134;
const ID_M_LAUT: i32 = 135;
const ID_REDUNDANZ: i32 = 140;
const ID_SCHUTZ: i32 = 141;
const ID_AUTO: i32 = 142;
const ID_PUFFER: i32 = 143;
const ID_VERBINDEN: i32 = 150;
const ID_ANLEITUNG: i32 = 151;
const ID_AUFNAHME: i32 = 152;
const ID_AUFNAHMEN: i32 = 153;
const ID_AUFN_METRONOM: i32 = 154;
const ID_QUELLEN: i32 = 155;
// Fenster „Weitere Quellen“: Schalter = QU_ID + i
const ID_QU_FERTIG: i32 = 3990;
const QU_ID: i32 = 4000;
// Fenster „Aufnahmen“: Zeile i = AU_ID + 4·i (Download), + 1 (Löschen), + 2 (Anhören), + 3 (für alle)
const ID_AU_NEU: i32 = 2990;
const ID_AU_HOCH: i32 = 2991;
const ID_AU_SPUR_LAUT: i32 = 2992;
const AU_ID: i32 = 3000;
const WM_AU_LISTE: u32 = WM_APP + 2;
const WM_AU_FERTIG: u32 = WM_APP + 3;
const WM_AU_HOCH: u32 = WM_APP + 4;
/// Breite des Aufnahmen-Fensters
const AU_BREITE: i32 = 690;
// Mischpult: Streifen = MP_ID + Kanal * 8 + (0 Fader, 1 An, 2 Panorama, 3 FX, 4 Panorama Mitte)
const MP_ID: i32 = 1000;
const ID_PRESET: i32 = 900;
const ID_PRESET_SPEICHERN: i32 = 901;
const ID_PRESET_LOESCHEN: i32 = 902;
// Effektfenster
const ID_E_VORLAGE: i32 = 910;
const ID_E_TIEFSPERRE: i32 = 911;
const ID_E_EQ: i32 = 912;
const ID_E_KOMP: i32 = 913;
const ID_E_HALL: i32 = 914;
const ID_AL_SPEICHERN: i32 = 950;
const ID_AL_SCHLIESSEN: i32 = 951;

const TBM_GETPOS: u32 = WM_USER;
const TB_ENDTRACK: u32 = 8;
const TAKTARTEN: [&str; 7] = ["ohne Betonung", "2er-Takt", "3er-Takt", "4er-Takt", "5er-Takt", "6er-Takt", "7er-Takt"];
const STREIFEN: i32 = 62;
const JE_REIHE: usize = 16;
const STREIFEN_H: i32 = 300;

// ---------------------------------------------------------------- Farben (Windows: 0x00BBGGRR)
const fn rgb(r: u32, g: u32, b: u32) -> u32 {
    r | (g << 8) | (b << 16)
}
const C_BG: u32 = rgb(12, 12, 14);
const C_KARTE: u32 = rgb(24, 24, 28);
const C_FELD: u32 = rgb(36, 36, 42);
const C_RAND: u32 = rgb(58, 58, 66);
const C_TEXT: u32 = rgb(236, 236, 240);
const C_TEXT2: u32 = rgb(150, 150, 162);
const C_AKZENT: u32 = rgb(239, 50, 70);
const C_AKZENT_DUNKEL: u32 = rgb(185, 28, 48);
/// dunkler Rotton für gewählte Knöpfe
const C_AKZENT_GRUND: u32 = rgb(64, 18, 24);
const C_ROT: u32 = rgb(244, 63, 94);
const C_GRUEN: u32 = rgb(34, 197, 94);
const C_GELB: u32 = rgb(250, 204, 21);
const C_ORANGE: u32 = rgb(245, 158, 11);
/// Bandfarben wie im Songcenter
const EQ_FARBEN: [u32; EQ_BAENDER] = [rgb(213, 91, 91), rgb(213, 165, 91), rgb(140, 213, 91), rgb(91, 213, 176), rgb(123, 155, 213)];

// Knopfarten (GWLP_USERDATA des Knopfs)
const K_AN: isize = 0x01;
const K_SCHALTER: isize = 0x10;
const K_PRIMAER: isize = 0x20;
const K_GEFAHR: isize = 0x40;
const K_GEWAEHLT: isize = 0x80;

static PINSEL_BG: AtomicIsize = AtomicIsize::new(0);
static PINSEL_KARTE: AtomicIsize = AtomicIsize::new(0);
static PINSEL_FELD: AtomicIsize = AtomicIsize::new(0);
static H_HAUPT: AtomicIsize = AtomicIsize::new(0);
/// Bildschirm-Skalierung (DPI / 96) als f32-Bits
static SKALA: AtomicU32 = AtomicU32::new(0x3F80_0000);

/// Eckenradius für Knöpfe, Eingabefelder und Auswahllisten (überall gleich, wie bei Apple)
fn radius() -> i32 {
    (8.0 * f32::from_bits(SKALA.load(Relaxed))).round() as i32
}

fn skaliert(v: f32) -> i32 {
    (v * f32::from_bits(SKALA.load(Relaxed))).round() as i32
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    /// Textfarbe je Static-Steuerelement (sonst C_TEXT)
    static FARBEN: RefCell<HashMap<isize, u32>> = RefCell::new(HashMap::new());
}

fn pinsel(p: &AtomicIsize) -> HBRUSH {
    HBRUSH(p.load(Relaxed) as _)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn komma(x: f32, stellen: usize) -> String {
    format!("{x:.stellen$}").replace('.', ",")
}

/// Bänder der Frequenzanzeige (logarithmisch 25 Hz … 20 kHz)
const SPEKTRUM_BAENDER: usize = 72;

fn spektrum_band(i: usize, n: usize) -> (f32, f32) {
    let f = |j: usize| 25.0 * (20_000.0f32 / 25.0).powf(j as f32 / n as f32);
    (f(i), f(i + 1))
}

/// Ortszeit als Name einer Aufnahme: 2026_12_01_1538
fn zeit_name() -> String {
    let t = unsafe { GetLocalTime() };
    format!("{:04}_{:02}_{:02}_{:02}{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute)
}

/// Download-Ordner des Benutzers
fn download_ordner() -> std::path::PathBuf {
    unsafe {
        if let Ok(p) = SHGetKnownFolderPath(&FOLDERID_Downloads, KNOWN_FOLDER_FLAG(0), None) {
            let s = p.to_string().unwrap_or_default();
            windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as _));
            if !s.is_empty() {
                return s.into();
            }
        }
    }
    std::env::var_os("USERPROFILE").map(|p| std::path::PathBuf::from(p).join("Downloads")).unwrap_or_else(|| ".".into())
}

fn freq_text(f: f32) -> String {
    if f >= 1000.0 {
        format!("{} kHz", komma(f / 1000.0, if f >= 10_000.0 { 1 } else { 2 }))
    } else {
        format!("{f:.0} Hz")
    }
}

pub fn meldung(text: &str) {
    unsafe {
        MessageBoxW(HWND::default(), &HSTRING::from(text), w!("Jamcenter"), MB_ICONERROR | MB_OK);
    }
}

fn hinweis(hwnd: HWND, text: &str) {
    unsafe {
        MessageBoxW(hwnd, &HSTRING::from(text), w!("Jamcenter"), MB_ICONINFORMATION | MB_OK);
    }
}

fn frage(hwnd: HWND, text: &str) -> bool {
    unsafe { MessageBoxW(hwnd, &HSTRING::from(text), w!("Jamcenter"), MB_ICONQUESTION | MB_YESNO) == IDYES }
}

fn farbe_setzen(h: HWND, f: u32) {
    let alt = FARBEN.with(|m| m.borrow_mut().insert(h.0 as isize, f));
    if alt != Some(f) {
        unsafe {
            let _ = InvalidateRect(h, None, true);
        }
    }
}

// ---------------------------------------------------------------- Schalter/Knöpfe
fn art(h: HWND) -> isize {
    unsafe { GetWindowLongPtrW(h, GWLP_USERDATA) }
}

fn schalter(h: HWND) -> bool {
    art(h) & K_AN != 0
}

fn schalter_setzen(h: HWND, an: bool) {
    unsafe {
        let a = art(h);
        let neu = if an { a | K_AN } else { a & !K_AN };
        if neu != a {
            SetWindowLongPtrW(h, GWLP_USERDATA, neu);
            let _ = InvalidateRect(h, None, false);
        }
    }
}

fn art_setzen(h: HWND, bit: isize, an: bool) {
    unsafe {
        let a = art(h);
        let neu = if an { a | bit } else { a & !bit };
        if neu != a {
            SetWindowLongPtrW(h, GWLP_USERDATA, neu);
            let _ = InvalidateRect(h, None, false);
        }
    }
}

unsafe fn text_von(h: HWND) -> String {
    let n = GetWindowTextLengthW(h).max(0) as usize;
    let mut buf = vec![0u16; n + 1];
    let r = GetWindowTextW(h, &mut buf).max(0) as usize;
    String::from_utf16_lossy(&buf[..r])
}

unsafe fn runde(hdc: HDC, r: &RECT, radius: i32, fuell: u32, rand: Option<u32>) {
    let b = CreateSolidBrush(COLORREF(fuell));
    let p = match rand {
        Some(f) => CreatePen(PS_SOLID, 1, COLORREF(f)),
        None => CreatePen(PS_SOLID, 1, COLORREF(fuell)),
    };
    let ab = SelectObject(hdc, b);
    let ap = SelectObject(hdc, p);
    let _ = RoundRect(hdc, r.left, r.top, r.right, r.bottom, radius, radius);
    SelectObject(hdc, ab);
    SelectObject(hdc, ap);
    let _ = DeleteObject(b);
    let _ = DeleteObject(p);
}

unsafe fn kreis(hdc: HDC, cx: i32, cy: i32, r: i32, fuell: u32, rand: u32) {
    let b = CreateSolidBrush(COLORREF(fuell));
    let p = CreatePen(PS_SOLID, 1, COLORREF(rand));
    let ab = SelectObject(hdc, b);
    let ap = SelectObject(hdc, p);
    let _ = Ellipse(hdc, cx - r, cy - r, cx + r + 1, cy + r + 1);
    SelectObject(hdc, ab);
    SelectObject(hdc, ap);
    let _ = DeleteObject(b);
    let _ = DeleteObject(p);
}

unsafe fn fuellen(hdc: HDC, r: &RECT, f: u32) {
    let b = CreateSolidBrush(COLORREF(f));
    FillRect(hdc, r, b);
    let _ = DeleteObject(b);
}

unsafe fn text_zeichnen(hdc: HDC, t: &str, r: &RECT, farbe: u32, format: DRAW_TEXT_FORMAT) {
    // leerer Text: gar nicht erst an DrawTextW geben (ein leerer Vec hat keinen gültigen Zeiger)
    if t.is_empty() {
        return;
    }
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, COLORREF(farbe));
    let mut w: Vec<u16> = t.encode_utf16().collect();
    let mut rr = *r;
    DrawTextW(hdc, &mut w, &mut rr, format | DT_NOPREFIX);
}

/// Überabtastung für geglättete Kanten (GDI kann kein Antialiasing)
const SS: i32 = 4;

/// Bereich `r` in 4-facher Auflösung zeichnen (Hintergrund wird übernommen) und mit
/// Halbton-Filter verkleinert zurückkopieren -> runde Formen ohne Treppenstufen.
/// `f` bekommt den Speicher-DC und arbeitet in Koordinaten relativ zu `r` × SS.
unsafe fn glatt(hdc: HDC, r: &RECT, f: impl FnOnce(HDC)) {
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    if w <= 0 || h <= 0 {
        return;
    }
    let mdc = CreateCompatibleDC(hdc);
    let bmp = CreateCompatibleBitmap(hdc, w * SS, h * SS);
    let alt = SelectObject(mdc, bmp);
    SetStretchBltMode(mdc, COLORONCOLOR);
    let _ = StretchBlt(mdc, 0, 0, w * SS, h * SS, hdc, r.left, r.top, w, h, SRCCOPY);
    f(mdc);
    SetStretchBltMode(hdc, HALFTONE);
    let _ = SetBrushOrgEx(hdc, 0, 0, None);
    let _ = StretchBlt(hdc, r.left, r.top, w, h, mdc, 0, 0, w * SS, h * SS, SRCCOPY);
    SelectObject(mdc, alt);
    let _ = DeleteObject(bmp);
    let _ = DeleteDC(mdc);
}

/// Bereich `r` erst unsichtbar zeichnen und dann in einem Stück zeigen (kein Flackern).
/// `f` zeichnet mit denselben Koordinaten wie auf `hdc`.
unsafe fn gepuffert(hdc: HDC, r: &RECT, f: impl FnOnce(HDC)) {
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    if w <= 0 || h <= 0 {
        return;
    }
    let mdc = CreateCompatibleDC(hdc);
    let bmp = CreateCompatibleBitmap(hdc, w, h);
    let alt = SelectObject(mdc, bmp);
    let _ = SetViewportOrgEx(mdc, -r.left, -r.top, None);
    let _ = SetBkMode(mdc, TRANSPARENT);
    f(mdc);
    let _ = SetViewportOrgEx(mdc, 0, 0, None);
    let _ = BitBlt(hdc, r.left, r.top, w, h, mdc, 0, 0, SRCCOPY);
    SelectObject(mdc, alt);
    let _ = DeleteObject(bmp);
    let _ = DeleteDC(mdc);
}

/// Fester Hintergrund eines Fensters (Karten, Kopf) einmal in ein Bild zeichnen – danach wird er nur
/// noch kopiert. So muss beim Auffrischen der Anzeigen nichts Teures neu gezeichnet werden.
unsafe fn hintergrund_anlegen(hwnd: HWND, f: impl FnOnce(HDC)) -> HBITMAP {
    let hdc = GetDC(hwnd);
    let mut c = RECT::default();
    let _ = GetClientRect(hwnd, &mut c);
    let mdc = CreateCompatibleDC(hdc);
    let bmp = CreateCompatibleBitmap(hdc, c.right.max(1), c.bottom.max(1));
    let alt = SelectObject(mdc, bmp);
    let _ = SetBkMode(mdc, TRANSPARENT);
    fuellen(mdc, &c, C_BG);
    f(mdc);
    SelectObject(mdc, alt);
    let _ = DeleteDC(mdc);
    ReleaseDC(hwnd, hdc);
    bmp
}

unsafe fn hintergrund_zeigen(hdc: HDC, bmp: HBITMAP) {
    let mut b = BITMAP::default();
    GetObjectW(HGDIOBJ(bmp.0), std::mem::size_of::<BITMAP>() as i32, Some(&mut b as *mut _ as _));
    let mdc = CreateCompatibleDC(hdc);
    let alt = SelectObject(mdc, bmp);
    let _ = BitBlt(hdc, 0, 0, b.bmWidth, b.bmHeight, mdc, 0, 0, SRCCOPY);
    SelectObject(mdc, alt);
    let _ = DeleteDC(mdc);
}

fn schneidet(a: &RECT, b: &RECT) -> bool {
    a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
}

/// Abgerundetes Rechteck geglättet (Rand optional)
unsafe fn runde_glatt(hdc: HDC, r: &RECT, radius: i32, fuell: u32, rand: Option<u32>) {
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    glatt(hdc, r, |m| {
        let gross = RECT { left: 0, top: 0, right: w * SS, bottom: h * SS };
        if let Some(rf) = rand {
            runde(m, &gross, radius * SS, rf, None);
            let innen = RECT { left: SS, top: SS, right: w * SS - SS, bottom: h * SS - SS };
            runde(m, &innen, (radius - 1).max(1) * SS, fuell, None);
        } else {
            runde(m, &gross, radius * SS, fuell, None);
        }
    });
}

/// Kreis geglättet
unsafe fn kreis_glatt(hdc: HDC, cx: i32, cy: i32, rad: i32, fuell: u32, rand: u32) {
    let r = RECT { left: cx - rad - 1, top: cy - rad - 1, right: cx + rad + 2, bottom: cy + rad + 2 };
    glatt(hdc, &r, |m| {
        let m0 = (rad + 1) * SS;
        let rr = rad * SS + SS / 2;
        kreis(m, m0, m0, rr, rand, rand);
        if rand != fuell {
            kreis(m, m0, m0, rr - (1.5 * SS as f32) as i32, fuell, fuell);
        }
    });
}

/// Knopf bzw. Kippschalter zeichnen (WM_DRAWITEM)
unsafe fn knopf_zeichnen(d: &DRAWITEMSTRUCT) {
    let hdc = d.hDC;
    let r = d.rcItem;
    let a = art(d.hwndItem);
    let gedrueckt = d.itemState.0 & ODS_SELECTED.0 != 0;
    let aus = d.itemState.0 & ODS_DISABLED.0 != 0;
    let grund = if GetParent(d.hwndItem).map(|p| p.0 as isize == H_HAUPT.load(Relaxed) || p.0 as isize == 0).unwrap_or(false) { C_KARTE } else { C_KARTE };
    fuellen(hdc, &r, grund);
    let text = text_von(d.hwndItem);
    let alt_font = SelectObject(hdc, HGDIOBJ(SendMessageW(d.hwndItem, WM_GETFONT, WPARAM(0), LPARAM(0)).0 as _));
    if a & K_SCHALTER != 0 {
        // Kippschalter: Pille links, Text rechts
        let an = a & K_AN != 0;
        let h = r.bottom - r.top;
        let ph = (h - 6).clamp(14, 20);
        let pw = ph * 2;
        let py = r.top + (h - ph) / 2;
        let pille = RECT { left: r.left + 2, top: py, right: r.left + 2 + pw, bottom: py + ph };
        let fuell = if aus {
            C_FELD
        } else if an {
            C_AKZENT
        } else {
            C_RAND
        };
        let knopf = if aus { C_RAND } else { rgb(250, 250, 252) };
        glatt(hdc, &pille, |m| {
            let (pw, ph) = (pw * SS, ph * SS);
            runde(m, &RECT { left: 0, top: 0, right: pw, bottom: ph }, ph, fuell, None);
            let kr = ph / 2 - 3 * SS;
            let kx = if an { pw - ph / 2 } else { ph / 2 };
            kreis(m, kx, ph / 2, kr, knopf, knopf);
        });
        let tr = RECT { left: pille.right + 8, ..r };
        text_zeichnen(hdc, &text, &tr, if aus { C_TEXT2 } else { C_TEXT }, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
    } else {
        let (fuell, schrift, rand) = if aus {
            (C_FELD, C_TEXT2, None)
        } else if a & K_PRIMAER != 0 {
            (if gedrueckt { C_AKZENT_DUNKEL } else { C_AKZENT }, rgb(255, 255, 255), None)
        } else if a & K_GEFAHR != 0 {
            // „Trennen“: dunkel mit rotem Rand, damit es sich vom roten Akzent abhebt
            (if gedrueckt { C_AKZENT_GRUND } else { C_FELD }, C_TEXT, Some(C_AKZENT))
        } else if a & K_GEWAEHLT != 0 {
            (if gedrueckt { C_FELD } else { C_AKZENT_GRUND }, rgb(255, 120, 132), Some(C_AKZENT))
        } else {
            (if gedrueckt { C_RAND } else { C_FELD }, C_TEXT, Some(C_RAND))
        };
        runde_glatt(hdc, &r, radius(), fuell, rand);
        text_zeichnen(hdc, &text, &r, schrift, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
    }
    SelectObject(hdc, alt_font);
}

/// Schieberegler zeichnen (NM_CUSTOMDRAW): komplett selbst und gepuffert – dünne Spur, Akzent bis
/// zum Griff, runder Griff. Der Regler selbst zeichnet nichts (sonst bleiben Reste stehen).
unsafe fn regler_zeichnen(nm: &NMCUSTOMDRAW) -> LRESULT {
    if nm.dwDrawStage != CDDS_PREPAINT {
        return LRESULT(CDRF_SKIPDEFAULT as isize);
    }
    let h = nm.hdr.hwndFrom;
    let senkrecht = GetWindowLongW(h, GWL_STYLE) as u32 & TBS_VERT != 0;
    let aktiv = IsWindowEnabled(h).as_bool();
    let mut c = RECT::default();
    let _ = GetClientRect(h, &mut c);
    let mut griff = RECT::default();
    SendMessageW(h, TBM_GETTHUMBRECT, WPARAM(0), LPARAM(&mut griff as *mut _ as isize));
    let mut kanal = RECT::default();
    SendMessageW(h, TBM_GETCHANNELRECT, WPARAM(0), LPARAM(&mut kanal as *mut _ as isize));
    let akzent = if aktiv { C_AKZENT } else { C_RAND };
    let d = skaliert(2.0).max(2);
    gepuffert(nm.hdc, &c, |m| {
        fuellen(m, &c, C_KARTE);
        let gx = (griff.left + griff.right) / 2;
        let gy = (griff.top + griff.bottom) / 2;
        if senkrecht {
            // TBM_GETCHANNELRECT liefert bei senkrechten Reglern die Werte waagerecht (left/right = oben/unten)
            let (oben, unten) = (kanal.left.min(kanal.right), kanal.left.max(kanal.right));
            fuellen(m, &RECT { left: gx - d, top: oben, right: gx + d, bottom: unten }, C_FELD);
            fuellen(m, &RECT { left: gx - d, top: gy, right: gx + d, bottom: unten }, akzent);
        } else {
            fuellen(m, &RECT { left: kanal.left, top: gy - d, right: kanal.right, bottom: gy + d }, C_FELD);
            fuellen(m, &RECT { left: kanal.left, top: gy - d, right: gx, bottom: gy + d }, akzent);
        }
        let rad = ((griff.right - griff.left).min(griff.bottom - griff.top) / 2 - 1).clamp(5, skaliert(9.0).max(9));
        if aktiv {
            kreis_glatt(m, gx, gy, rad, rgb(250, 250, 252), C_AKZENT);
        } else {
            kreis_glatt(m, gx, gy, rad, C_RAND, C_RAND);
        }
    });
    LRESULT(CDRF_SKIPDEFAULT as isize)
}

/// Schieberegler: Windows erneuert beim Bewegen nur die Stelle des alten und des neuen Griffs.
/// Die farbige Spur dazwischen bliebe dann alt stehen (rote Striche) – deshalb bei jeder
/// Positionsänderung den ganzen Regler neu zeichnen lassen.
unsafe extern "system" fn regler_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _d: usize) -> LRESULT {
    match msg {
        WM_NCDESTROY => {
            let _ = RemoveWindowSubclass(h, Some(regler_proc), 1);
            DefSubclassProc(h, msg, wp, lp)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_LBUTTONUP | WM_KEYDOWN | WM_MOUSEWHEEL | WM_TIMER | WM_ENABLE | WM_SETFOCUS | WM_KILLFOCUS => {
            let vor = DefSubclassProc(h, TBM_GETPOS, WPARAM(0), LPARAM(0)).0;
            let r = DefSubclassProc(h, msg, wp, lp);
            if DefSubclassProc(h, TBM_GETPOS, WPARAM(0), LPARAM(0)).0 != vor || matches!(msg, WM_ENABLE | WM_SETFOCUS | WM_KILLFOCUS) {
                let _ = InvalidateRect(h, None, false);
            }
            r
        }
        x if x == TBM_SETPOS || x == TBM_SETPOSNOTIFY || x == TBM_SETRANGE => {
            let r = DefSubclassProc(h, msg, wp, lp);
            let _ = InvalidateRect(h, None, false);
            r
        }
        _ => DefSubclassProc(h, msg, wp, lp),
    }
}

/// Auswahlliste (Combobox) rund zeichnen: Feld mit gleichem Radius wie die Knöpfe, Text, Pfeil
unsafe extern "system" fn auswahl_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _d: usize) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(h, &mut ps);
            let mut r = RECT::default();
            let _ = GetClientRect(h, &mut r);
            let aktiv = IsWindowEnabled(h).as_bool();
            let editierbar = GetWindowLongW(h, GWL_STYLE) as u32 & 3 == CBS_DROPDOWN as u32;
            let fokus = GetFocus() == h;
            let schrift = HGDIOBJ(SendMessageW(h, WM_GETFONT, WPARAM(0), LPARAM(0)).0 as _);
            let text = if editierbar { String::new() } else { text_von(h) };
            gepuffert(hdc, &r, |m| {
                fuellen(m, &r, C_KARTE);
                runde_glatt(m, &r, radius(), C_FELD, Some(if fokus { C_AKZENT } else { C_RAND }));
                let alt = SelectObject(m, schrift);
                let tr = RECT { left: r.left + skaliert(10.0), right: r.right - skaliert(28.0), ..r };
                text_zeichnen(m, &text, &tr, if aktiv { C_TEXT } else { C_TEXT2 }, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS);
                SelectObject(m, alt);
                // Pfeil nach unten
                let cy = (r.top + r.bottom) / 2;
                let pr = RECT { left: r.right - skaliert(24.0), top: cy - skaliert(5.0), right: r.right - skaliert(10.0), bottom: cy + skaliert(5.0) };
                let (pw, ph) = (pr.right - pr.left, pr.bottom - pr.top);
                let farbe = if aktiv { C_TEXT2 } else { C_RAND };
                glatt(m, &pr, |g| {
                    let pkt = [
                        POINT { x: pw * SS / 5, y: ph * SS * 3 / 10 },
                        POINT { x: pw * SS / 2, y: ph * SS * 7 / 10 },
                        POINT { x: pw * SS * 4 / 5, y: ph * SS * 3 / 10 },
                    ];
                    linienzug(g, &pkt, farbe, (1.8 * SS as f32 * f32::from_bits(SKALA.load(Relaxed))) as i32);
                });
            });
            let _ = EndPaint(h, &ps);
            return LRESULT(0);
        }
        WM_ERASEBKGND => return LRESULT(1),
        WM_SETFOCUS | WM_KILLFOCUS | WM_ENABLE => {
            let r = DefSubclassProc(h, msg, wp, lp);
            let _ = InvalidateRect(h, None, false);
            return r;
        }
        WM_NCDESTROY => {
            let _ = RemoveWindowSubclass(h, Some(auswahl_proc), 1);
        }
        _ => {}
    }
    DefSubclassProc(h, msg, wp, lp)
}

/// Eingabefeld rund: Rand als Nicht-Client-Bereich (gleicher Radius wie Knöpfe), Text senkrecht mittig
unsafe extern "system" fn feld_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _d: usize) -> LRESULT {
    match msg {
        WM_NCCALCSIZE => {
            let r: &mut RECT = if wp.0 != 0 { &mut (*(lp.0 as *mut NCCALCSIZE_PARAMS)).rgrc[0] } else { &mut *(lp.0 as *mut RECT) };
            let hdc = GetDC(h);
            let schrift = HGDIOBJ(SendMessageW(h, WM_GETFONT, WPARAM(0), LPARAM(0)).0 as _);
            let alt = SelectObject(hdc, schrift);
            let mut tm = TEXTMETRICW::default();
            let _ = GetTextMetricsW(hdc, &mut tm);
            SelectObject(hdc, alt);
            ReleaseDC(h, hdc);
            let hoehe = r.bottom - r.top;
            let dy = ((hoehe - tm.tmHeight) / 2).max(1);
            let dx = radius().max(4);
            r.left += dx;
            r.right -= dx;
            r.top += dy;
            r.bottom = (r.bottom - dy).max(r.top + 1);
            return LRESULT(0);
        }
        WM_NCPAINT => {
            let hdc = GetWindowDC(h);
            let mut w = RECT::default();
            let _ = GetWindowRect(h, &mut w);
            let ganz = RECT { left: 0, top: 0, right: w.right - w.left, bottom: w.bottom - w.top };
            let mut c = RECT::default();
            let _ = GetClientRect(h, &mut c);
            let mut pt = POINT::default();
            let _ = ClientToScreen(h, &mut pt);
            let (cx, cy) = (pt.x - w.left, pt.y - w.top);
            let _ = ExcludeClipRect(hdc, cx, cy, cx + c.right, cy + c.bottom);
            let fokus = GetFocus() == h;
            gepuffert(hdc, &ganz, |m| {
                fuellen(m, &ganz, C_KARTE);
                runde_glatt(m, &ganz, radius(), C_FELD, Some(if fokus { C_AKZENT } else { C_RAND }));
            });
            ReleaseDC(h, hdc);
            return LRESULT(0);
        }
        WM_SETFOCUS | WM_KILLFOCUS => {
            let r = DefSubclassProc(h, msg, wp, lp);
            let _ = RedrawWindow(h, None, None, RDW_FRAME | RDW_INVALIDATE);
            return r;
        }
        WM_NCDESTROY => {
            let _ = RemoveWindowSubclass(h, Some(feld_proc), 1);
        }
        _ => {}
    }
    DefSubclassProc(h, msg, wp, lp)
}

/// Segmentierte Pegelanzeige (waagerecht oder senkrecht)
unsafe fn pegel_zeichnen_direkt(hdc: HDC, r: &RECT, wert: f32) {
    let db = 20.0 * wert.max(1e-6).log10();
    let anteil = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
    fuellen(hdc, r, rgb(30, 30, 36));
    let senkrecht = r.bottom - r.top > r.right - r.left;
    let laenge = if senkrecht { r.bottom - r.top } else { r.right - r.left };
    let seg = 4;
    let n = laenge / seg;
    let an = (n as f32 * anteil).round() as i32;
    for i in 0..an {
        let pos = i as f32 / n.max(1) as f32;
        let farbe = if pos > 0.95 {
            C_ROT
        } else if pos > 0.8 {
            C_GELB
        } else {
            C_GRUEN
        };
        let s = if senkrecht {
            RECT { left: r.left, top: r.bottom - (i + 1) * seg + 1, right: r.right, bottom: r.bottom - i * seg }
        } else {
            RECT { left: r.left + i * seg, top: r.top, right: r.left + (i + 1) * seg - 1, bottom: r.bottom }
        };
        fuellen(hdc, &s, farbe);
    }
}

// ---------------------------------------------------------------- Strukturen
#[allow(dead_code)]
struct Steuer {
    raum: HWND,
    passwort: HWND,
    name: HWND,
    server: HWND,
    treiber: HWND,
    panel: HWND,
    eingang: HWND,
    mischpult: HWND,
    verst: HWND,
    verst_wert: HWND,
    stumm: HWND,
    stereo: HWND,
    ausgang: HWND,
    laut: HWND,
    laut_wert: HWND,
    exklusiv: HWND,
    block_label: HWND,
    block: HWND,
    m_an: HWND,
    m_bpm_wert: HWND,
    m_tap: HWND,
    m_bpm: HWND,
    m_takt: HWND,
    m_klang: HWND,
    m_laut: HWND,
    redundanz: HWND,
    schutz: HWND,
    auto: HWND,
    puffer: HWND,
    puffer_wert: HWND,
    verbinden: HWND,
    aufnahme: HWND,
    status: HWND,
    latenz: HWND,
    detail: HWND,
    geraete: HWND,
    quellen_info: HWND,
}

struct Streifen {
    fader: HWND,
    wert: HWND,
    an: HWND,
    pan: HWND,
    pan_wert: HWND,
    pan_mitte: HWND,
    fx: HWND,
}

/// Panorama als Text: „L 40 %“, „Mitte“, „R 100 %“
fn pan_text(pan: f32) -> String {
    let p = (pan * 100.0).round() as i32;
    match p {
        0 => "Mitte".into(),
        p if p < 0 => format!("L {} %", -p),
        p => format!("R {p} %"),
    }
}

struct Mischpult {
    hwnd: HWND,
    namen: Vec<String>,
    streifen: Vec<Streifen>,
    meter: Vec<RECT>,
    anz: Vec<f32>,
    preset: HWND,
    karten: Vec<(RECT, String)>,
    bg: HBITMAP,
}

/// Ein Parameter des Effektfensters (Regler ↔ Wert)
#[derive(Clone, Copy, PartialEq)]
enum FxP {
    Gain(usize),
    Q(usize),
    Schwelle,
    Ratio,
    Attack,
    Release,
    Knee,
    Makeup,
    HallMix,
    HallGroesse,
    HallAbkling,
}

impl FxP {
    fn atom(self, fx: &KanalFx) -> &AtomicF32 {
        match self {
            FxP::Gain(b) => &fx.eq_gain[b],
            FxP::Q(b) => &fx.eq_q[b],
            FxP::Schwelle => &fx.schwelle,
            FxP::Ratio => &fx.ratio,
            FxP::Attack => &fx.attack_ms,
            FxP::Release => &fx.release_ms,
            FxP::Knee => &fx.knee,
            FxP::Makeup => &fx.makeup,
            FxP::HallMix => &fx.hall_mix,
            FxP::HallGroesse => &fx.hall_groesse,
            FxP::HallAbkling => &fx.hall_abkling,
        }
    }

    /// Reglerbereich (Bereiche wie im Songcenter)
    fn bereich(self) -> (i32, i32) {
        match self {
            FxP::Gain(_) => (0, 72),
            FxP::Q(_) => (0, 100),
            FxP::Schwelle => (0, 60),
            FxP::Ratio => (0, 38),
            FxP::Attack => (0, 200),
            FxP::Release => (10, 1000),
            FxP::Knee => (0, 40),
            FxP::Makeup => (0, 48),
            FxP::HallMix => (0, 100),
            FxP::HallGroesse => (2, 60),
            FxP::HallAbkling => (5, 80),
        }
    }

    fn zu_wert(self, p: i32) -> f32 {
        let p = p as f32;
        match self {
            FxP::Gain(_) => 18.0 - p / 2.0,
            FxP::Q(_) => 0.2 * 60f32.powf(p / 100.0),
            FxP::Schwelle => p - 60.0,
            FxP::Ratio => 1.0 + p * 0.5,
            FxP::Attack | FxP::Release | FxP::Knee => p,
            FxP::Makeup => p / 2.0,
            FxP::HallMix => p / 100.0,
            FxP::HallGroesse | FxP::HallAbkling => p / 10.0,
        }
    }

    fn zu_pos(self, v: f32) -> i32 {
        let p = match self {
            FxP::Gain(_) => (18.0 - v) * 2.0,
            FxP::Q(_) => (v / 0.2).max(1.0).ln() / 60f32.ln() * 100.0,
            FxP::Schwelle => v + 60.0,
            FxP::Ratio => (v - 1.0) * 2.0,
            FxP::Attack | FxP::Release | FxP::Knee => v,
            FxP::Makeup => v * 2.0,
            FxP::HallMix => v * 100.0,
            FxP::HallGroesse | FxP::HallAbkling => v * 10.0,
        };
        p.round() as i32
    }

    fn text(self, v: f32) -> String {
        match self {
            FxP::Gain(_) => format!("{:+.1} dB", v).replace('.', ","),
            FxP::Q(_) => format!("Q {}", komma(v, 2)),
            FxP::Schwelle | FxP::Knee => format!("{v:.0} dB"),
            FxP::Ratio => format!("{}:1", komma(v, 1)),
            FxP::Attack | FxP::Release => format!("{v:.0} ms"),
            FxP::Makeup => format!("+{} dB", komma(v, 1)),
            FxP::HallMix => format!("{:.0} %", v * 100.0),
            FxP::HallGroesse => format!("{} s", komma(v, 1)),
            FxP::HallAbkling => komma(v, 1),
        }
    }

    /// zu welchem Abschnitt gehört der Parameter (0 EQ, 1 Kompressor, 2 Hall)
    fn abschnitt(self) -> u8 {
        match self {
            FxP::Gain(_) | FxP::Q(_) => 0,
            FxP::HallMix | FxP::HallGroesse | FxP::HallAbkling => 2,
            _ => 1,
        }
    }
}

/// Effektfenster eines Eingangs (EQ, Kompressor, Hall – Aufbau wie im Songcenter)
struct Effekte {
    hwnd: HWND,
    kanal: usize,
    titel: HWND,
    vorlage: HWND,
    tiefsperre: HWND,
    eq_an: HWND,
    komp_an: HWND,
    hall_an: HWND,
    /// (Regler, Parameter, Wertanzeige)
    regler: Vec<(HWND, FxP, HWND)>,
    freq: [HWND; EQ_BAENDER],
    eq_graph: RECT,
    komp_graph: RECT,
    komp_meter: [RECT; 3],
    komp_bereich: RECT,
    /// Anzeige In / GR (dB) / Out
    anz: [f32; 3],
    ziehen: Option<usize>,
    band: usize,
    karten: Vec<RECT>,
    bg: HBITMAP,
    /// Frequenzanzeige (dB je Band) hinter der EQ-Kurve
    balken: Vec<f32>,
}

/// Linienzug mit eigenem Stift
unsafe fn linienzug(hdc: HDC, pkt: &[POINT], farbe: u32, breite: i32) {
    if pkt.len() < 2 {
        return;
    }
    let stift = CreatePen(PS_SOLID, breite, COLORREF(farbe));
    let alt = SelectObject(hdc, stift);
    let _ = Polyline(hdc, pkt);
    SelectObject(hdc, alt);
    let _ = DeleteObject(stift);
}

/// Position eines EQ-Punktes im Graphen (20 Hz–20 kHz logarithmisch, ±18 dB)
fn eq_xy(g: &RECT, f: f32, db: f32) -> (f32, f32) {
    let (w, h) = ((g.right - g.left) as f32, (g.bottom - g.top) as f32);
    let x = g.left as f32 + w * ((f.max(20.0) / 20.0).ln() / 1000f32.ln());
    let y = g.top as f32 + h * (18.0 - db.clamp(-18.0, 18.0)) / 36.0;
    (x, y)
}

fn eq_von_xy(g: &RECT, x: i32, y: i32) -> (f32, f32) {
    let (w, h) = ((g.right - g.left) as f32, (g.bottom - g.top) as f32);
    let tx = ((x - g.left) as f32 / w).clamp(0.0, 1.0);
    let ty = ((y - g.top) as f32 / h).clamp(0.0, 1.0);
    let f = 20.0 * 1000f32.powf(tx);
    let db = ((18.0 - ty * 36.0) * 2.0).round() / 2.0;
    (f.round(), db)
}

struct Anleitung {
    hwnd: HWND,
}

/// Fenster „Weitere Quellen“
struct QuellenFenster {
    hwnd: HWND,
    eintraege: Vec<(String, HWND)>,
    alt: String,
}

/// Fenster „Aufnahmen“: Liste der Aufnahmen des Raums auf dem Server
struct AufnahmenFenster {
    hwnd: HWND,
    kennung: [u8; 32],
    host: String,
    eintraege: Vec<Eintrag>,
    steuer: Vec<HWND>,
    status: HWND,
    /// Ergebnis des Hintergrund-Abrufs
    liste: Arc<Mutex<Option<Result<Vec<Eintrag>, String>>>>,
    meldung: Arc<Mutex<Option<Result<std::path::PathBuf, String>>>>,
    laedt: bool,
    zuletzt: Instant,
    /// gerade angehörte Aufnahme (Zeile, Spieler)
    spielt: Option<(usize, crate::abspielen::Spieler)>,
    /// Ergebnis des Hochladens (Dateiname oder Fehler)
    hoch: Arc<Mutex<Option<Result<String, String>>>>,
    hoch_laeuft: bool,
}

struct App {
    hwnd: HWND,
    s: f32,
    hinst: HINSTANCE,
    font: HFONT,
    font_fett: HFONT,
    font_gross: HFONT,
    font_klein: HFONT,
    font_logo: HFONT,
    font_titel: HFONT,
    c: Option<Steuer>,
    karten: Vec<(RECT, &'static str)>,
    geteilt: Arc<Geteilt>,
    einst: Einstellungen,
    sitzung: Option<Sitzung>,
    ein_liste: Vec<Geraet>,
    aus_liste: Vec<Geraet>,
    asio_liste: Vec<asio::TreiberInfo>,
    asio_ein: Vec<String>,
    asio_aus: Vec<String>,
    asio_bloecke: Option<asio::Bloecke>,
    asio_panel: Option<asio::Treiber>,
    meter_ein: RECT,
    meter_aus: RECT,
    anz_ein: f32,
    anz_aus: f32,
    texte: HashMap<isize, String>,
    takt: u32,
    rk_alt: u32,
    rk_bis: Option<Instant>,
    m_punkte: RECT,
    m_schlag: i32,
    m_takt_anz: i32,
    m_stand: u32,
    taps: Vec<u64>,
    mp: Option<Mischpult>,
    ef: Option<Effekte>,
    au: Option<AufnahmenFenster>,
    qu: Option<QuellenFenster>,
    /// Spur des Raums: Schlüssel der geladenen/ladenden Datei, Ergebnis des Ladens, Meldung, Länge (s)
    spur_schluessel: String,
    spur_lade: Arc<Mutex<Option<(String, Result<(u32, Vec<i16>), String>)>>>,
    spur_meldung: String,
    spur_dauer: f64,
    /// zuletzt gesehener Zustand (an, Stand), um die Liste bei Änderungen neu zu zeichnen
    spur_zuletzt: (bool, u32),
    /// Aufnahme lief beim letzten Takt (zum Erkennen von „gestoppt“)
    aufn_war_an: bool,
    /// zwischengespeicherter Hintergrund des Hauptfensters
    bg: HBITMAP,
    al: Option<Anleitung>,
    /// Programmsymbol in Kopfgröße (aus der exe, fehlt beim Bauen ohne icon_setzen.ps1)
    logo: HICON,
}

impl App {
    fn p(&self, v: i32) -> i32 {
        (v as f32 * self.s).round() as i32
    }

    fn pr(&self, x: i32, y: i32, w: i32, h: i32) -> RECT {
        RECT { left: self.p(x), top: self.p(y), right: self.p(x + w), bottom: self.p(y + h) }
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn ctl_in(&self, eltern: HWND, klasse: PCWSTR, text: &str, stil: u32, ex: WINDOW_EX_STYLE, r: (i32, i32, i32, i32), id: i32, font: HFONT) -> HWND {
        let h = CreateWindowExW(
            ex,
            klasse,
            &HSTRING::from(text),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(stil),
            self.p(r.0),
            self.p(r.1),
            self.p(r.2),
            self.p(r.3),
            eltern,
            HMENU(id as isize as _),
            self.hinst,
            None,
        )
        .unwrap_or_default();
        SendMessageW(h, WM_SETFONT, WPARAM(font.0 as usize), LPARAM(1));
        h
    }

    unsafe fn label_in(&self, e: HWND, text: &str, r: (i32, i32, i32, i32), farbe: u32, font: HFONT) -> HWND {
        let h = self.ctl_in(e, w!("STATIC"), text, 0x80 /* SS_NOPREFIX */, WINDOW_EX_STYLE(0), r, 0, font);
        FARBEN.with(|m| m.borrow_mut().insert(h.0 as isize, farbe));
        h
    }

    unsafe fn feld_in(&self, e: HWND, text: &str, r: (i32, i32, i32, i32), id: i32, passwort: bool) -> HWND {
        let stil = WS_TABSTOP.0 | ES_AUTOHSCROLL as u32 | if passwort { ES_PASSWORD as u32 } else { 0 };
        let h = self.ctl_in(e, w!("EDIT"), text, stil, WINDOW_EX_STYLE(0), r, id, self.font);
        let _ = SetWindowTheme(h, w!("DarkMode_CFD"), PCWSTR::null());
        let _ = SetWindowSubclass(h, Some(feld_proc), 1, 0);
        // Rand neu berechnen lassen (WM_NCCALCSIZE mit der gesetzten Schrift)
        let _ = SetWindowPos(h, HWND::default(), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED);
        h
    }

    unsafe fn auswahl_in(&self, e: HWND, r: (i32, i32, i32, i32), id: i32, editierbar: bool) -> HWND {
        let art = if editierbar { CBS_DROPDOWN } else { CBS_DROPDOWNLIST } as u32;
        let h = self.ctl_in(e, w!("COMBOBOX"), "", WS_TABSTOP.0 | art | WS_VSCROLL.0 | WS_CLIPCHILDREN.0, WINDOW_EX_STYLE(0), (r.0, r.1, r.2, 320), id, self.font);
        let _ = SetWindowTheme(h, w!("DarkMode_CFD"), PCWSTR::null());
        let _ = SetWindowSubclass(h, Some(auswahl_proc), 1, 0);
        // Die Eingabe eines editierbaren Feldes etwas einrücken, damit sie in die runden Ecken passt
        if editierbar {
            if let Ok(edit) = FindWindowExW(h, HWND::default(), w!("Edit"), PCWSTR::null()) {
                let mut er = RECT::default();
                let _ = GetWindowRect(edit, &mut er);
                let mut pt = POINT { x: er.left, y: er.top };
                let _ = ScreenToClient(h, &mut pt);
                let ein = (skaliert(10.0) - pt.x).max(0);
                let _ = SetWindowPos(edit, HWND::default(), pt.x + ein, pt.y, er.right - er.left - ein, er.bottom - er.top, SWP_NOZORDER);
            }
        }
        h
    }

    unsafe fn knopf_in(&self, e: HWND, text: &str, r: (i32, i32, i32, i32), id: i32, a: isize, font: HFONT) -> HWND {
        let h = self.ctl_in(e, w!("BUTTON"), text, WS_TABSTOP.0 | BS_OWNERDRAW as u32, WINDOW_EX_STYLE(0), r, id, font);
        SetWindowLongPtrW(h, GWLP_USERDATA, a);
        h
    }

    unsafe fn schalter_in(&self, e: HWND, text: &str, r: (i32, i32, i32, i32), id: i32, an: bool) -> HWND {
        self.knopf_in(e, text, r, id, K_SCHALTER | if an { K_AN } else { 0 }, self.font)
    }

    unsafe fn regler_in(&self, e: HWND, r: (i32, i32, i32, i32), id: i32, min: i32, max: i32, pos: i32, senkrecht: bool) -> HWND {
        let stil = WS_TABSTOP.0 | TBS_NOTICKS | if senkrecht { TBS_VERT | TBS_BOTH } else { 0 };
        let h = self.ctl_in(e, TRACKBAR_CLASSW, "", stil, WINDOW_EX_STYLE(0), r, id, self.font);
        SendMessageW(h, TBM_SETRANGE, WPARAM(1), LPARAM(((max as isize) << 16) | (min as isize & 0xFFFF)));
        SendMessageW(h, TBM_SETPOS, WPARAM(1), LPARAM(pos as isize));
        let _ = SetWindowSubclass(h, Some(regler_proc), 1, 0);
        h
    }

    unsafe fn text_setzen(&mut self, h: HWND, t: &str) {
        let k = h.0 as isize;
        if self.texte.get(&k).map(|s| s.as_str()) != Some(t) {
            let _ = SetWindowTextW(h, &HSTRING::from(t));
            self.texte.insert(k, t.to_string());
        }
    }

    // ------------------------------------------------------------ Aufbau Hauptfenster
    unsafe fn aufbauen(&mut self) {
        let h = self.hwnd;
        let e = &self.einst.clone();
        let (f, fk) = (self.font, self.font_klein);
        let grau = C_TEXT2;
        let anl = self.knopf_in(h, "Eigener Server …", (744, 14, 160, 30), ID_ANLEITUNG, 0, f);
        let _ = anl;

        // Karte VERBINDUNG
        self.karten.push((self.pr(16, 64, 440, 176), "VERBINDUNG"));
        self.label_in(h, "Raum", (32, 101, 76, 20), grau, f);
        let raum = self.feld_in(h, &e.raum, (112, 97, 140, 26), ID_RAUM, false);
        self.label_in(h, "Passwort", (264, 101, 66, 20), grau, f);
        let passwort = self.feld_in(h, &e.passwort, (332, 97, 108, 26), ID_PASSWORT, true);
        self.label_in(h, "Dieser Raum", (32, 135, 76, 20), grau, f);
        let name = self.feld_in(h, &e.name, (112, 131, 328, 26), ID_NAME, false);
        self.label_in(h, "Server", (32, 169, 76, 20), grau, f);
        let server = self.feld_in(h, &e.server, (112, 165, 328, 26), ID_SERVER, false);
        self.label_in(h, "leer = voreingestellter Jamcenter-Server · UDP 3478, 443, 47800", (112, 197, 328, 32), grau, fk);

        // Karte AUDIO
        self.karten.push((self.pr(16, 252, 440, 300), "AUDIO"));
        self.label_in(h, "Treiber", (32, 287, 76, 20), grau, f);
        let treiber = self.auswahl_in(h, (112, 283, 212, 0), ID_TREIBER, false);
        let panel = self.knopf_in(h, "ASIO-Panel", (332, 282, 108, 28), ID_PANEL, 0, f);
        self.label_in(h, "Eingang", (32, 321, 76, 20), grau, f);
        let eingang = self.auswahl_in(h, (112, 317, 212, 0), ID_EINGANG, false);
        let mischpult = self.knopf_in(h, "Mischpult", (332, 316, 108, 28), ID_MISCHPULT, K_GEWAEHLT, f);
        self.meter_ein = self.pr(112, 356, 150, 8);
        let verst = self.regler_in(h, (266, 347, 136, 26), ID_VERST, 0, 50, (e.eingang_db + 20.0).round() as i32, false);
        let verst_wert = self.label_in(h, "", (404, 351, 44, 20), C_TEXT, fk);
        let stumm = self.schalter_in(h, "Stumm", (112, 381, 110, 24), ID_STUMM, false);
        let stereo = self.schalter_in(h, "Stereo senden", (232, 381, 200, 24), ID_STEREO, e.stereo);
        // weitere Eingänge: zweites Interface, Windows-Ton (Sequencer, Player …), Loopback
        self.knopf_in(h, "Weitere Quellen …", (112, 413, 150, 28), ID_QUELLEN, 0, f);
        let quellen_info = self.label_in(h, "", (270, 413, 176, 30), grau, fk);
        self.label_in(h, "Ausgang", (32, 463, 76, 20), grau, f);
        let ausgang = self.auswahl_in(h, (112, 459, 328, 0), ID_AUSGANG, false);
        self.meter_aus = self.pr(112, 498, 150, 8);
        let laut = self.regler_in(h, (266, 489, 136, 26), ID_LAUT, 0, 52, (e.lautstaerke_db + 40.0).round() as i32, false);
        let laut_wert = self.label_in(h, "", (404, 493, 44, 20), C_TEXT, fk);
        let exklusiv = self.schalter_in(h, "Exklusiver Gerätezugriff (geringste Latenz)", (112, 524, 330, 24), ID_EXKLUSIV, e.exklusiv);
        let block_label = self.label_in(h, "Block", (32, 528, 76, 20), grau, f);
        let block = self.auswahl_in(h, (112, 524, 328, 0), ID_BLOCK, false);

        // Karte METRONOM
        self.karten.push((self.pr(472, 64, 432, 176), "METRONOM"));
        let m_an = self.schalter_in(h, "an", (488, 95, 64, 26), ID_M_AN, false);
        let m_bpm_wert = self.label_in(h, "100 BPM", (560, 90, 160, 34), C_TEXT, self.font_gross);
        let m_tap = self.knopf_in(h, "TAP", (808, 93, 80, 30), ID_M_TAP, K_GEWAEHLT, self.font_fett);
        let m_bpm = self.regler_in(h, (484, 128, 408, 26), ID_M_BPM, 30, 240, 100, false);
        let m_takt = self.auswahl_in(h, (488, 162, 118, 0), ID_M_TAKT, false);
        let m_klang = self.auswahl_in(h, (614, 162, 150, 0), ID_M_KLANG, false);
        let m_laut = self.regler_in(h, (770, 161, 122, 26), ID_M_LAUT, 0, 46, (e.metronom_db + 40.0).round() as i32, false);
        self.m_punkte = self.pr(488, 202, 260, 20);
        self.label_in(h, "Klick-Lautstärke", (774, 190, 118, 16), C_TEXT2, fk);
        let takte: Vec<String> = TAKTARTEN.iter().map(|t| t.to_string()).collect();
        Self::eintraege(m_takt, &takte, 3);
        let klaenge: Vec<String> = KLANGFORMEN.iter().map(|t| t.to_string()).collect();
        Self::eintraege(m_klang, &klaenge, e.metronom_klang as usize);

        // Karte OPTIONEN
        self.karten.push((self.pr(472, 252, 432, 132), "OPTIONEN"));
        let redundanz = self.schalter_in(h, "Schutz vor Aussetzern (Pakete doppelt, keine Verzögerung)", (488, 282, 404, 24), ID_REDUNDANZ, e.redundanz);
        let schutz = self.schalter_in(h, "Rückkopplungsschutz (senkt bei Pfeifen die Lautstärke)", (488, 312, 404, 24), ID_SCHUTZ, e.schutz);
        let auto = self.schalter_in(h, "Puffer automatisch", (488, 344, 176, 24), ID_AUTO, e.puffer_auto);
        let puffer = self.regler_in(h, (666, 342, 140, 26), ID_PUFFER, 0, 100, e.puffer_ms.round() as i32, false);
        let puffer_wert = self.label_in(h, "", (808, 346, 92, 20), C_TEXT, fk);
        let _ = EnableWindow(puffer, !e.puffer_auto);

        // Karte STATUS
        self.karten.push((self.pr(472, 396, 432, 156), "STATUS"));
        let verbinden = self.knopf_in(h, "Verbinden", (488, 424, 184, 40), ID_VERBINDEN, K_PRIMAER, self.font_fett);
        let aufnahme = self.knopf_in(h, "● Aufnahme", (680, 424, 112, 40), ID_AUFNAHME, 0, self.font_fett);
        // Liste der Aufnahmen jederzeit (auch ohne Verbindung)
        self.knopf_in(h, "Aufnahmen", (800, 424, 88, 40), ID_AUFNAHMEN, 0, f);
        let status = self.label_in(h, "Nicht verbunden.", (488, 466, 262, 40), C_TEXT2, f);
        self.schalter_in(h, "mit Metronom", (758, 468, 140, 24), ID_AUFN_METRONOM, e.aufnahme_metronom);
        let latenz = self.label_in(h, "– ms", (488, 512, 120, 34), C_TEXT2, self.font_gross);
        let detail = self.label_in(h, "", (612, 507, 288, 46), C_TEXT2, fk);
        self.karten.push((self.pr(16, 562, 888, 34), ""));
        let geraete = self.label_in(h, "", (30, 565, 760, 28), C_TEXT2, fk);

        self.c = Some(Steuer {
            raum,
            passwort,
            name,
            server,
            treiber,
            panel,
            eingang,
            mischpult,
            verst,
            verst_wert,
            stumm,
            stereo,
            ausgang,
            laut,
            laut_wert,
            exklusiv,
            block_label,
            block,
            m_an,
            m_bpm_wert,
            m_tap,
            m_bpm,
            m_takt,
            m_klang,
            m_laut,
            redundanz,
            schutz,
            auto,
            puffer,
            puffer_wert,
            verbinden,
            aufnahme,
            status,
            latenz,
            detail,
            geraete,
            quellen_info,
        });
        self.treiber_fuellen();
        // Treiberdaten erst nach dem Anzeigen laden (ASIO-Treiber brauchen dafür teils Sekunden)
        let _ = PostMessageW(self.hwnd, WM_APP + 1, WPARAM(0), LPARAM(0));
        self.werte_anzeigen();
        let _ = SetFocus(if self.einst.raum.is_empty() { raum } else { verbinden });
    }

    unsafe fn eintraege(h: HWND, namen: &[String], sel: usize) {
        SendMessageW(h, CB_RESETCONTENT, WPARAM(0), LPARAM(0));
        for n in namen {
            let w = wide(n);
            SendMessageW(h, CB_ADDSTRING, WPARAM(0), LPARAM(w.as_ptr() as isize));
        }
        SendMessageW(h, CB_SETCURSEL, WPARAM(sel.min(namen.len().saturating_sub(1))), LPARAM(0));
    }

    unsafe fn combo_fuellen(h: HWND, liste: &[Geraet], gewaehlt: &str) {
        let mut namen = vec!["Windows-Standardgerät".to_string()];
        namen.extend(liste.iter().map(|g| g.name.clone()));
        let sel = liste.iter().position(|g| g.id == gewaehlt).map(|i| i + 1).unwrap_or(0);
        Self::eintraege(h, &namen, sel);
    }

    fn asio(&self) -> bool {
        !self.einst.treiber.is_empty()
    }

    unsafe fn treiber_fuellen(&mut self) {
        self.asio_liste = asio::treiber_liste();
        if self.asio() && !self.asio_liste.iter().any(|t| t.name == self.einst.treiber) {
            self.einst.treiber.clear();
        }
        let mut namen = vec!["Windows-Audio (Webcam, eingebaute Geräte)".to_string()];
        namen.extend(self.asio_liste.iter().map(|t| format!("ASIO: {}", t.name)));
        let sel = self.asio_liste.iter().position(|t| t.name == self.einst.treiber).map(|i| i + 1).unwrap_or(0);
        let h = self.c.as_ref().unwrap().treiber;
        Self::eintraege(h, &namen, sel);
        if self.asio_liste.is_empty() {
            let _ = EnableWindow(h, false);
        }
    }

    /// Kanäle und Blockgrößen des gewählten ASIO-Treibers lesen (nicht während er in der Verbindung läuft)
    unsafe fn asio_info_laden(&mut self) {
        if self.sitzung.is_some() {
            return;
        }
        let Some(info) = self.asio_liste.iter().find(|t| t.name == self.einst.treiber).cloned() else { return };
        if self.asio_panel.as_ref().is_some_and(|t| t.name != info.name) {
            self.asio_panel = None;
        }
        let geladen = match self.asio_panel.take() {
            Some(t) => Ok(t),
            None => asio::Treiber::laden(&info, self.hwnd),
        };
        match geladen {
            Ok(t) => {
                let k = t.kanaele();
                self.asio_ein = k.eingaenge;
                self.asio_aus = k.ausgaenge;
                self.asio_bloecke = Some(t.bloecke());
            }
            Err(e) => {
                self.asio_ein.clear();
                self.asio_aus.clear();
                self.asio_bloecke = None;
                hinweis(self.hwnd, &e);
            }
        }
    }

    unsafe fn listen_fuellen(&mut self) {
        let (h_ein, h_aus, h_block) = {
            let c = self.c.as_ref().unwrap();
            (c.eingang, c.ausgang, c.block)
        };
        if !self.asio() {
            self.ein_liste = audio::geraete(true);
            self.aus_liste = audio::geraete(false);
            Self::combo_fuellen(h_ein, &self.ein_liste, &self.einst.eingang);
            let _ = EnableWindow(h_ein, true);
            Self::combo_fuellen(h_aus, &self.aus_liste, &self.einst.ausgang);
            return;
        }
        Self::eintraege(h_ein, &[format!("{} Eingänge – im Mischpult wählen", self.asio_ein.len())], 0);
        let _ = EnableWindow(h_ein, false);
        let aus: Vec<String> = (0..self.asio_aus.len())
            .step_by(2)
            .map(|i| match self.asio_aus.get(i + 1) {
                Some(n2) => format!("Ausgang {}/{}: {} / {}", i + 1, i + 2, self.asio_aus[i], n2),
                None => format!("Ausgang {}: {}", i + 1, self.asio_aus[i]),
            })
            .collect();
        Self::eintraege(h_aus, &aus, self.einst.asio_ausgang / 2);
        let ms = |n: u32| komma(n as f32 * 1000.0 / 48_000.0, 2);
        let mut bl = Vec::new();
        let mut sel = 0;
        if let Some(b) = &self.asio_bloecke {
            bl.push(format!("Treiber-Vorgabe ({} Samples = {} ms)", b.bevorzugt, ms(b.bevorzugt)));
            for (i, &n) in b.moeglich.iter().enumerate() {
                bl.push(format!("{n} Samples = {} ms", ms(n)));
                if n == self.einst.asio_block {
                    sel = i + 1;
                }
            }
        }
        Self::eintraege(h_block, &bl, sel);
    }

    /// Steuerelemente für Windows-Audio bzw. ASIO zeigen
    unsafe fn modus_anwenden(&mut self) {
        let asio = self.asio();
        if asio {
            self.asio_info_laden();
        }
        let c = self.c.as_ref().unwrap();
        let zeigen = |h: HWND, an: bool| {
            let _ = ShowWindow(h, if an { SW_SHOW } else { SW_HIDE });
        };
        zeigen(c.exklusiv, !asio);
        zeigen(c.block_label, asio);
        zeigen(c.block, asio);
        let _ = EnableWindow(c.panel, asio);
        self.listen_fuellen();
    }

    unsafe fn werte_anzeigen(&mut self) {
        let (vw, lw, pw) = {
            let c = self.c.as_ref().unwrap();
            (c.verst_wert, c.laut_wert, c.puffer_wert)
        };
        let (v, l) = (self.einst.eingang_db, self.einst.lautstaerke_db);
        self.text_setzen(vw, &format!("{:+.0} dB", v));
        self.text_setzen(lw, &format!("{:+.0} dB", l));
        let p = if self.einst.puffer_auto {
            let z = self.geteilt.anzeige.puffer_ziel_ms.get();
            if self.sitzung.is_some() && z > 0.0 {
                format!("{} ms", komma(z, 1))
            } else {
                "auto".into()
            }
        } else {
            format!("+{} ms", self.einst.puffer_ms.round())
        };
        self.text_setzen(pw, &p);
    }

    unsafe fn felder_sperren(&self, sperren: bool) {
        let c = self.c.as_ref().unwrap();
        for h in [c.raum, c.passwort, c.name, c.server] {
            let _ = EnableWindow(h, !sperren);
        }
    }

    unsafe fn felder_lesen(&mut self) {
        let c = self.c.as_ref().unwrap();
        self.einst.raum = text_von(c.raum).trim().to_string();
        self.einst.passwort = text_von(c.passwort);
        self.einst.name = text_von(c.name).trim().to_string();
        let s = text_von(c.server).trim().to_string();
        self.einst.server = if s.is_empty() { protokoll::STANDARD_SERVER.to_string() } else { s };
    }

    unsafe fn verbinden(&mut self) {
        self.felder_lesen();
        if self.einst.raum.is_empty() {
            hinweis(self.hwnd, "Bitte einen Raumnamen eingeben – beide Seiten müssen denselben Raum (und dasselbe Passwort) verwenden.");
            return;
        }
        if self.einst.server.trim().is_empty() {
            hinweis(self.hwnd, KEIN_SERVER);
            return;
        }
        if self.einst.name.is_empty() {
            self.einst.name = "Jamcenter".into();
        }
        self.einst.speichern();
        // Ein ASIO-Treiber darf nur einmal geladen sein – die Verbindung lädt ihn selbst
        self.asio_panel = None;
        self.anhoeren_stoppen();
        match sitzung::starten(&self.einst, self.geteilt.clone()) {
            Ok(s) => {
                self.sitzung = Some(s);
                let v = self.c.as_ref().unwrap().verbinden;
                self.text_setzen(v, "Trennen");
                art_setzen(v, K_PRIMAER, false);
                art_setzen(v, K_GEFAHR, true);
                self.felder_sperren(true);
                self.m_stand = u32::MAX;
            }
            Err(e) => meldung(&e),
        }
    }

    unsafe fn trennen(&mut self) {
        self.anhoeren_stoppen();
        if let Some(s) = self.sitzung.take() {
            s.stoppen();
        }
        let v = self.c.as_ref().unwrap().verbinden;
        self.text_setzen(v, "Verbinden");
        art_setzen(v, K_GEFAHR, false);
        art_setzen(v, K_PRIMAER, true);
        self.felder_sperren(false);
        self.anz_ein = 0.0;
        self.anz_aus = 0.0;
        self.m_schlag = -1;
        let _ = InvalidateRect(self.hwnd, None, false);
    }

    unsafe fn neu_starten(&mut self) {
        if self.sitzung.is_some() {
            self.trennen();
            self.verbinden();
        }
    }

    // ------------------------------------------------------------ Metronom
    unsafe fn metronom_anzeigen(&mut self) {
        let m = &self.geteilt.metronom;
        let (an, bpm, takt) = (m.an.load(Relaxed), m.bpm.get().round() as i32, m.takt.load(Relaxed).clamp(1, 7) as usize);
        let (h_an, h_bpm, h_wert, h_takt) = {
            let c = self.c.as_ref().unwrap();
            (c.m_an, c.m_bpm, c.m_bpm_wert, c.m_takt)
        };
        schalter_setzen(h_an, an);
        SendMessageW(h_bpm, TBM_SETPOS, WPARAM(1), LPARAM(bpm as isize));
        SendMessageW(h_takt, CB_SETCURSEL, WPARAM(takt - 1), LPARAM(0));
        self.text_setzen(h_wert, &format!("{bpm} BPM"));
    }

    /// Metronom-Einstellung an den Server schicken (gilt für beide Räume)
    unsafe fn metronom_wunsch(&mut self) {
        let (h_an, h_bpm, h_takt) = {
            let c = self.c.as_ref().unwrap();
            (c.m_an, c.m_bpm, c.m_takt)
        };
        if self.sitzung.is_none() {
            if schalter(h_an) {
                schalter_setzen(h_an, false);
                hinweis(self.hwnd, "Erst verbinden: Das Metronom läuft auf dem Server und gilt für beide Räume.");
            }
            return;
        }
        let an = schalter(h_an);
        let bpm = SendMessageW(h_bpm, TBM_GETPOS, WPARAM(0), LPARAM(0)).0 as f32;
        let takt = (SendMessageW(h_takt, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0.max(0) + 1) as u8;
        *self.geteilt.metronom_wunsch.lock().unwrap() = Some((an, bpm, takt));
    }

    /// Tempo antippen: Mittel der letzten Abstände (bis 2,5 s Pause)
    unsafe fn tap(&mut self) {
        let jetzt = uhr_us();
        if self.taps.last().is_some_and(|&t| jetzt - t > 2_500_000) {
            self.taps.clear();
        }
        self.taps.push(jetzt);
        if self.taps.len() > 6 {
            self.taps.remove(0);
        }
        if self.taps.len() < 2 {
            return;
        }
        let abstand = (self.taps[self.taps.len() - 1] - self.taps[0]) as f64 / (self.taps.len() - 1) as f64;
        let bpm = (60e6 / abstand).round().clamp(30.0, 240.0) as i32;
        let (h_bpm, h_wert) = {
            let c = self.c.as_ref().unwrap();
            (c.m_bpm, c.m_bpm_wert)
        };
        SendMessageW(h_bpm, TBM_SETPOS, WPARAM(1), LPARAM(bpm as isize));
        self.text_setzen(h_wert, &format!("{bpm} BPM"));
        if self.sitzung.is_some() && self.taps.len() >= 3 {
            self.metronom_wunsch();
        }
    }

    // ------------------------------------------------------------ Befehle Hauptfenster
    unsafe fn befehl(&mut self, id: i32, code: u32, h: HWND) {
        let g = self.geteilt.clone();
        match id {
            ID_VERBINDEN | 1 if code == BN_CLICKED => {
                if self.sitzung.is_some() {
                    self.trennen();
                } else {
                    self.verbinden();
                }
            }
            ID_ANLEITUNG if code == BN_CLICKED => self.anleitung_oeffnen(),
            ID_AUFNAHMEN if code == BN_CLICKED => self.aufnahmen_oeffnen(),
            ID_QUELLEN if code == BN_CLICKED => self.quellen_oeffnen(),
            ID_AUFNAHME if code == BN_CLICKED => self.aufnahme_umschalten(),
            ID_AUFN_METRONOM => {
                self.einst.aufnahme_metronom = schalter(h);
                self.einst.speichern();
            }
            ID_STUMM => g.regler.stumm.store(schalter(h), Relaxed),
            ID_STEREO => {
                self.einst.stereo = schalter(h);
                g.regler.stereo.store(self.einst.stereo, Relaxed);
                self.einst.speichern();
                self.mischpult_auffrischen();
            }
            ID_EXKLUSIV => {
                self.einst.exklusiv = schalter(h);
                self.einst.speichern();
                self.neu_starten();
            }
            ID_REDUNDANZ => {
                self.einst.redundanz = schalter(h);
                g.regler.redundanz.store(self.einst.redundanz, Relaxed);
                self.einst.speichern();
            }
            ID_SCHUTZ => {
                self.einst.schutz = schalter(h);
                g.regler.schutz.store(self.einst.schutz, Relaxed);
                self.einst.speichern();
            }
            ID_AUTO => {
                self.einst.puffer_auto = schalter(h);
                g.regler.puffer_auto.store(self.einst.puffer_auto, Relaxed);
                let p = self.c.as_ref().unwrap().puffer;
                let _ = EnableWindow(p, !self.einst.puffer_auto);
                if !self.einst.puffer_auto {
                    if self.sitzung.is_some() {
                        self.einst.puffer_ms = g.jitter_ms.get().ceil().clamp(0.0, 100.0);
                    }
                    g.regler.puffer_ms.set(self.einst.puffer_ms);
                    SendMessageW(p, TBM_SETPOS, WPARAM(1), LPARAM(self.einst.puffer_ms as isize));
                }
                self.einst.speichern();
                self.werte_anzeigen();
            }
            ID_M_AN => self.metronom_wunsch(),
            ID_M_TAP if code == BN_CLICKED => self.tap(),
            ID_M_TAKT if code == CBN_SELCHANGE => self.metronom_wunsch(),
            ID_M_KLANG if code == CBN_SELCHANGE => {
                let sel = SendMessageW(h, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0.max(0) as u32;
                self.einst.metronom_klang = sel;
                g.regler.metronom_klang.store(sel, Relaxed);
                self.einst.speichern();
            }
            ID_MISCHPULT if code == BN_CLICKED => self.mischpult_oeffnen(),
            ID_TREIBER if code == CBN_SELCHANGE => {
                let sel = SendMessageW(h, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
                let neu = if sel <= 0 { String::new() } else { self.asio_liste.get(sel as usize - 1).map(|t| t.name.clone()).unwrap_or_default() };
                if neu != self.einst.treiber {
                    self.einst.treiber = neu;
                    self.einst.asio_eingang = 0;
                    self.einst.asio_ausgang = 0;
                    self.einst.asio_block = 0;
                    self.asio_panel = None;
                    self.mischpult_schliessen();
                    self.einst.kanaele_anwenden(&self.geteilt);
                    let lief = self.sitzung.is_some();
                    if lief {
                        self.trennen();
                    }
                    self.modus_anwenden();
                    self.einst.speichern();
                    if lief {
                        self.verbinden();
                    }
                }
            }
            ID_PANEL if code == BN_CLICKED => {
                if self.sitzung.is_some() {
                    asio::PANEL_OEFFNEN.store(true, Relaxed);
                } else if let Some(info) = self.asio_liste.iter().find(|t| t.name == self.einst.treiber).cloned() {
                    if self.asio_panel.is_none() {
                        match asio::Treiber::laden(&info, self.hwnd) {
                            Ok(t) => self.asio_panel = Some(t),
                            Err(e) => hinweis(self.hwnd, &e),
                        }
                    }
                    if let Some(t) = &self.asio_panel {
                        t.panel();
                    }
                }
            }
            ID_BLOCK => {
                if code == CBN_DROPDOWN {
                    if self.sitzung.is_none() {
                        if let Some(info) = self.asio_liste.iter().find(|t| t.name == self.einst.treiber) {
                            let neu = match &self.asio_panel {
                                Some(t) => Some(t.bloecke()),
                                None => asio::Treiber::laden(info, self.hwnd).ok().map(|t| t.bloecke()),
                            };
                            if neu.is_some() {
                                self.asio_bloecke = neu;
                                self.listen_fuellen();
                            }
                        }
                    }
                } else if code == CBN_SELCHANGE {
                    let sel = SendMessageW(h, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
                    let neu = if sel <= 0 { 0 } else { self.asio_bloecke.as_ref().and_then(|b| b.moeglich.get(sel as usize - 1).copied()).unwrap_or(0) };
                    if neu != self.einst.asio_block {
                        self.einst.asio_block = neu;
                        self.einst.speichern();
                        self.neu_starten();
                    }
                }
            }
            ID_AUSGANG if self.asio() => {
                if code == CBN_SELCHANGE {
                    let sel = SendMessageW(h, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0.max(0) as usize;
                    if self.einst.asio_ausgang != sel * 2 {
                        self.einst.asio_ausgang = sel * 2;
                        self.einst.speichern();
                        self.neu_starten();
                    }
                }
            }
            ID_EINGANG | ID_AUSGANG if !self.asio() => {
                if code == CBN_DROPDOWN {
                    let (liste, gew) = if id == ID_EINGANG {
                        self.ein_liste = audio::geraete(true);
                        (&self.ein_liste, &self.einst.eingang)
                    } else {
                        self.aus_liste = audio::geraete(false);
                        (&self.aus_liste, &self.einst.ausgang)
                    };
                    Self::combo_fuellen(h, liste, gew);
                } else if code == CBN_SELCHANGE {
                    let sel = SendMessageW(h, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
                    let liste = if id == ID_EINGANG { &self.ein_liste } else { &self.aus_liste };
                    let neu = if sel <= 0 { String::new() } else { liste.get(sel as usize - 1).map(|g| g.id.clone()).unwrap_or_default() };
                    let alt = if id == ID_EINGANG { &mut self.einst.eingang } else { &mut self.einst.ausgang };
                    if *alt != neu {
                        *alt = neu;
                        if id == ID_EINGANG {
                            self.mischpult_schliessen();
                            self.einst.kanaele_anwenden(&self.geteilt);
                        }
                        self.einst.speichern();
                        self.neu_starten();
                    }
                }
            }
            _ => {}
        }
    }

    unsafe fn schieber(&mut self, h: HWND, code: u32) {
        let pos = SendMessageW(h, TBM_GETPOS, WPARAM(0), LPARAM(0)).0 as f32;
        let c = self.c.as_ref().unwrap();
        let (verst, laut, puffer, m_bpm, m_laut, m_bpm_wert) = (c.verst, c.laut, c.puffer, c.m_bpm, c.m_laut, c.m_bpm_wert);
        let g = self.geteilt.clone();
        let r = &g.regler;
        if h == m_bpm {
            self.text_setzen(m_bpm_wert, &format!("{} BPM", pos as i32));
            if code == TB_ENDTRACK {
                self.metronom_wunsch();
            }
            return;
        } else if h == m_laut {
            self.einst.metronom_db = pos - 40.0;
            r.metronom_db.set(self.einst.metronom_db);
            return;
        } else if h == verst {
            self.einst.eingang_db = pos - 20.0;
            r.eingang_db.set(self.einst.eingang_db);
        } else if h == laut {
            self.einst.lautstaerke_db = pos - 40.0;
            r.lautstaerke_db.set(self.einst.lautstaerke_db);
        } else if h == puffer {
            self.einst.puffer_ms = pos;
            r.puffer_ms.set(pos);
        }
        self.werte_anzeigen();
    }

    // ------------------------------------------------------------ Takt (40 ms)
    unsafe fn takt(&mut self) {
        self.takt = self.takt.wrapping_add(1);
        self.spur_takt();
        self.anhoeren_takt();
        let g = self.geteilt.clone();
        let a = &g.anzeige;
        self.anz_ein = a.pegel_ein.nehmen().max(self.anz_ein * 0.82);
        self.anz_aus = a.pegel_aus.nehmen().max(self.anz_aus * 0.82);
        let _ = InvalidateRect(self.hwnd, Some(&self.meter_ein), false);
        let _ = InvalidateRect(self.hwnd, Some(&self.meter_aus), false);

        // Schlag-Punkte (Serverzeit)
        let m = &g.metronom;
        let mut schlag = -1;
        let takt = m.takt.load(Relaxed).clamp(1, 7) as i32;
        if self.sitzung.is_some() && m.an.load(Relaxed) && g.uhr_ok.load(Relaxed) {
            let t = uhr_us() as i64 + g.uhr_versatz.load(Relaxed);
            let start = m.start_us.load(Relaxed) as i64;
            if t >= start {
                let dauer = 60e6 / (m.bpm.get() as f64).max(1.0);
                let pos = (t - start) as f64 / dauer;
                if pos.fract() < 0.25 {
                    schlag = ((pos as i64) % takt as i64) as i32;
                }
            }
        }
        if schlag != self.m_schlag || takt != self.m_takt_anz {
            self.m_schlag = schlag;
            self.m_takt_anz = takt;
            let _ = InvalidateRect(self.hwnd, Some(&self.m_punkte), false);
        }
        if let Some(mp) = &mut self.mp {
            for (i, anz) in mp.anz.iter_mut().enumerate() {
                *anz = a.kanal_pegel[i].nehmen().max(*anz * 0.82);
            }
            for r in &mp.meter {
                let _ = InvalidateRect(mp.hwnd, Some(r), false);
            }
        }
        self.aufnahme_anzeigen();
        if let Some(au) = &self.au {
            if !au.laedt && au.eintraege.iter().any(|e| !e.fertig) && au.zuletzt.elapsed() > Duration::from_secs(2) {
                self.aufnahmen_laden();
            }
        }
        if self.ef.is_some() {
            self.spektrum_rechnen();
        }
        if let Some(ef) = &mut self.ef {
            let c = ef.kanal;
            let neu = [a.komp_ein[c].nehmen(), a.komp_gr[c].nehmen(), a.komp_aus[c].nehmen()];
            let alt = ef.anz;
            for i in 0..3 {
                ef.anz[i] = neu[i].max(ef.anz[i] * 0.82);
                if ef.anz[i] < 1e-5 {
                    ef.anz[i] = 0.0;
                }
            }
            if alt != ef.anz {
                let _ = InvalidateRect(ef.hwnd, Some(&ef.komp_graph), false);
                for r in &ef.komp_meter {
                    let _ = InvalidateRect(ef.hwnd, Some(r), false);
                }
            }
        }
        let st = m.stand.load(Relaxed);
        if self.sitzung.is_some() && g.uhr_ok.load(Relaxed) && st != self.m_stand {
            self.m_stand = st;
            self.metronom_anzeigen();
        }
        // Rückkopplungsschutz hat die Lautstärke gesenkt -> Schieber nachziehen
        let l = g.regler.lautstaerke_db.get();
        if (l - self.einst.lautstaerke_db).abs() > 0.01 {
            self.einst.lautstaerke_db = l;
            let h = self.c.as_ref().unwrap().laut;
            SendMessageW(h, TBM_SETPOS, WPARAM(1), LPARAM((l + 40.0).round() as isize));
            self.werte_anzeigen();
        }
        let rk = a.rueckkopplungen.load(Relaxed);
        if rk != self.rk_alt {
            self.rk_alt = rk;
            self.rk_bis = Some(Instant::now() + Duration::from_secs(12));
        }
        if self.takt % 5 != 0 {
            return;
        }
        let c = self.c.as_ref().unwrap();
        let (h_status, h_latenz, h_detail, h_geraete, h_puffer) = (c.status, c.latenz, c.detail, c.geraete, c.puffer);

        let fehler = a.fehler.lock().unwrap().clone();
        let (text, farbe) = if self.sitzung.is_none() {
            ("Nicht verbunden.".to_string(), C_TEXT2)
        } else if let Some(f) = fehler {
            (format!("Problem: {f}"), C_ROT)
        } else if self.rk_bis.is_some_and(|t| Instant::now() < t) {
            ("Rückkopplung erkannt – Lautstärke um 6 dB gesenkt.".to_string(), C_ORANGE)
        } else {
            let s = a.status.lock().unwrap().clone();
            let ok = a.weg.load(Relaxed) > 0 && a.partner_da.load(Relaxed);
            (s, if ok { C_GRUEN } else { C_TEXT2 })
        };
        // Fehlermeldungen sind oft lang: dann bekommt der Text auch den Platz der Latenzanzeige
        let lang = text.starts_with("Problem: ");
        if lang == IsWindowVisible(h_latenz).as_bool() {
            let zeigen = if lang { SW_HIDE } else { SW_SHOW };
            let _ = ShowWindow(h_latenz, zeigen);
            let _ = ShowWindow(h_detail, zeigen);
            let _ = SetWindowPos(h_status, HWND::default(), 0, 0, self.p(262), self.p(if lang { 84 } else { 40 }), SWP_NOMOVE | SWP_NOZORDER);
        }
        farbe_setzen(h_status, farbe);
        self.text_setzen(h_status, &text);

        let weg = a.weg.load(Relaxed);
        let rtt = match weg {
            2 => a.rtt_direkt_ms.get(),
            1 => a.rtt_relay_ms.get(),
            _ => -1.0,
        };
        let (gross, detail, farbe) = if self.sitzung.is_some() && a.partner_da.load(Relaxed) && rtt >= 0.0 {
            let netz = rtt / 2.0;
            let pos = |x: f32| x.max(0.0);
            let her = pos(a.partner_ein_ms.get()) + netz + a.puffer_ist_ms.get() + a.aus_latenz_ms.get();
            let hin = a.ein_latenz_ms.get() + netz + pos(a.partner_puffer_ms.get()) + pos(a.partner_aus_ms.get());
            let mx = her.max(hin);
            let farbe = if mx <= 20.0 {
                C_GRUEN
            } else if mx <= 35.0 {
                C_ORANGE
            } else {
                C_ROT
            };
            (
                format!("{} ms", mx.round()),
                format!(
                    "Ihr hört die anderen nach ≈ {} ms, sie euch nach ≈ {} ms\nNetz {} ms ({}) · Puffer {} ms · Jitter {} ms\nVerlust {} % · Aussetzer {} · {}",
                    her.round(),
                    hin.round(),
                    komma(netz, 1),
                    if weg == 2 { "direkt" } else { "Server" },
                    komma(a.puffer_ist_ms.get(), 1),
                    komma(g.jitter_ms.get(), 1),
                    komma(a.verlust_prozent.get(), 1),
                    a.unterlaeufe.load(Relaxed),
                    if g.regler.stereo.load(Relaxed) { "Stereo" } else { "Mono" }
                ),
                farbe,
            )
        } else {
            ("– ms".to_string(), String::new(), C_TEXT2)
        };
        farbe_setzen(h_latenz, farbe);
        self.text_setzen(h_latenz, &gross);
        self.text_setzen(h_detail, &detail);

        let gtext = if self.sitzung.is_some() {
            let ein = a.ein_info.lock().unwrap().clone();
            let aus = a.aus_info.lock().unwrap().clone();
            format!(
                "Eingang ({} ms): {}\nAusgang ({} ms): {}",
                komma(a.ein_latenz_ms.get(), 1),
                if ein.is_empty() { "…".into() } else { ein },
                komma(a.aus_latenz_ms.get(), 1),
                if aus.is_empty() { "…".into() } else { aus }
            )
        } else {
            "Tipp: Netzwerkkabel statt WLAN, Lautsprecher analog anschließen (nicht über Beamer/HDMI) und nicht neben das Mikrofon stellen.".into()
        };
        self.text_setzen(h_geraete, &gtext);
        if self.einst.puffer_auto {
            SendMessageW(h_puffer, TBM_SETPOS, WPARAM(1), LPARAM(a.puffer_ziel_ms.get().round() as isize));
        }
        self.werte_anzeigen();
    }

    /// Fester Teil des Hauptfensters (Kopf, Karten, Version)
    unsafe fn haupt_hintergrund(&self, hdc: HDC) {
        let alt = SelectObject(hdc, self.font_logo);
        // Logo (Programmsymbol) links, daneben der Schriftzug
        let mit_logo = !self.logo.0.is_null();
        if mit_logo {
            let _ = DrawIconEx(hdc, self.p(18), self.p(10), self.logo, self.p(40), self.p(40), 0, HBRUSH::default(), DI_NORMAL);
        }
        let x0 = if mit_logo { 66 } else { 20 };
        text_zeichnen(hdc, "JAMCENTER", &self.pr(x0, 8, 300, 34), C_AKZENT, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
        SelectObject(hdc, self.font_klein);
        text_zeichnen(hdc, "Live-Musik zwischen zwei Räumen", &self.pr(x0 + 176, 10, 300, 30), C_TEXT2, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
        text_zeichnen(hdc, "by .audiosport", &self.pr(x0 + 2, 40, 300, 16), rgb(110, 110, 122), DT_LEFT | DT_SINGLELINE);
        // Karten
        SelectObject(hdc, self.font_titel);
        for (r, titel) in &self.karten {
            runde_glatt(hdc, r, self.p(14), C_KARTE, Some(rgb(36, 36, 42)));
            let t = RECT { left: r.left + self.p(16), top: r.top + self.p(8), right: r.right, bottom: r.top + self.p(26) };
            text_zeichnen(hdc, titel, &t, C_TEXT2, DT_LEFT | DT_SINGLELINE);
        }
        // Version rechts in der Fußzeile
        SelectObject(hdc, self.font_klein);
        let v = format!("Version {}", env!("CARGO_PKG_VERSION"));
        text_zeichnen(hdc, &v, &self.pr(796, 562, 94, 34), rgb(110, 110, 122), DT_RIGHT | DT_VCENTER | DT_SINGLELINE);
        SelectObject(hdc, alt);
    }

    unsafe fn malen(&mut self) {
        if self.bg.0.is_null() {
            let bg = hintergrund_anlegen(self.hwnd, |m| self.haupt_hintergrund(m));
            self.bg = bg;
        }
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(self.hwnd, &mut ps);
        let (schlag, anzahl, s) = (self.m_schlag, self.m_takt_anz.max(1), self.s);
        let mp = self.m_punkte;
        // alles erst unsichtbar zeichnen und dann in einem Stück zeigen – kein Flackern
        gepuffert(hdc, &ps.rcPaint, |hdc| {
            hintergrund_zeigen(hdc, self.bg);
            pegel_zeichnen_direkt(hdc, &self.meter_ein, self.anz_ein);
            pegel_zeichnen_direkt(hdc, &self.meter_aus, self.anz_aus);
            // Schlag-Punkte
            glatt(hdc, &mp, |m| {
                for i in 0..anzahl {
                    let cx = ((8 + i * 26) as f32 * s) as i32 * SS;
                    let cy = (mp.bottom - mp.top) / 2 * SS;
                    let an = i == schlag;
                    let farbe = if !an {
                        C_FELD
                    } else if i == 0 && anzahl > 1 {
                        C_AKZENT
                    } else {
                        C_TEXT
                    };
                    kreis(m, cx, cy, (7.0 * s) as i32 * SS, if an { farbe } else { C_RAND }, if an { farbe } else { C_RAND });
                    if !an {
                        kreis(m, cx, cy, (6.0 * s) as i32 * SS, farbe, farbe);
                    }
                }
            });
        });
        let _ = EndPaint(self.hwnd, &ps);
    }

    // ------------------------------------------------------------ Mischpult
    unsafe fn mischpult_schliessen(&mut self) {
        if let Some(ef) = self.ef.take() {
            let _ = DestroyWindow(ef.hwnd);
            let _ = DeleteObject(ef.bg);
            self.geteilt.anzeige.spektrum_kanal.store(u32::MAX, Relaxed);
        }
        if let Some(mp) = self.mp.take() {
            let _ = DestroyWindow(mp.hwnd);
            let _ = DeleteObject(mp.bg);
            self.einst.speichern();
        }
    }

    unsafe fn mischpult_geaendert(&mut self) {
        self.geteilt.regler.kanaele_vorgabe.store(false, Relaxed);
        self.einst.kanaele_merken(&self.geteilt);
        self.fx_knoepfe_markieren();
    }

    /// FX-Knöpfe der Kanäle rot, wenn dort ein Effekt an ist
    unsafe fn fx_knoepfe_markieren(&self) {
        let Some(mp) = &self.mp else { return };
        for (c, s) in mp.streifen.iter().enumerate() {
            art_setzen(s.fx, K_GEWAEHLT, self.geteilt.regler.fx[c].aktiv());
        }
    }

    /// Alle Steuerelemente des Mischpults auf die Regler setzen (nach Preset/Stereo-Wechsel)
    unsafe fn mischpult_auffrischen(&mut self) {
        let g = self.geteilt.clone();
        let stereo = g.regler.stereo.load(Relaxed);
        let Some(mp) = &self.mp else { return };
        let mut texte = Vec::new();
        for (c, s) in mp.streifen.iter().enumerate() {
            let db = g.regler.kanal_db[c].get();
            SendMessageW(s.fader, TBM_SETPOS, WPARAM(1), LPARAM((10.0 - db).round() as isize));
            SendMessageW(s.pan, TBM_SETPOS, WPARAM(1), LPARAM((g.regler.kanal_pan[c].get() * 20.0 + 20.0).round() as isize));
            let _ = EnableWindow(s.pan, stereo);
            let _ = EnableWindow(s.pan_mitte, stereo);
            texte.push((s.pan_wert, pan_text(g.regler.kanal_pan[c].get())));
            schalter_setzen(s.an, g.regler.kanal_an[c].load(Relaxed));
            texte.push((s.wert, format!("{:+.0} dB", db)));
        }
        for (h, t) in texte {
            self.text_setzen(h, &t);
        }
        self.fx_knoepfe_markieren();
        self.effekte_anzeigen();
        if let Some(c) = &self.c {
            schalter_setzen(c.stereo, stereo);
        }
    }

    unsafe fn presets_fuellen(&self, h: HWND, wahl: &str) {
        let liste = gemeinsam::presets_laden();
        SendMessageW(h, CB_RESETCONTENT, WPARAM(0), LPARAM(0));
        for (n, _) in &liste {
            let w = wide(n);
            SendMessageW(h, CB_ADDSTRING, WPARAM(0), LPARAM(w.as_ptr() as isize));
        }
        let _ = SetWindowTextW(h, &HSTRING::from(wahl));
    }

    unsafe fn mischpult_oeffnen(&mut self) {
        if let Some(mp) = &self.mp {
            let _ = SetForegroundWindow(mp.hwnd);
            return;
        }
        let g = self.geteilt.clone();
        let mut namen = g.anzeige.kanal_namen.lock().unwrap().clone();
        if namen.is_empty() || self.sitzung.is_none() {
            namen = if self.asio() {
                self.asio_ein.iter().enumerate().map(|(i, n)| if n.is_empty() { format!("Eingang {}", i + 1) } else { n.clone() }).collect()
            } else {
                match audio::kanalzahl(&self.einst.eingang) {
                    1 => vec!["Mono".into()],
                    2 => vec!["Links".into(), "Rechts".into()],
                    k => (1..=k).map(|i| format!("Kanal {i}")).collect(),
                }
            };
            if namen.is_empty() {
                namen = vec!["Mono".into()];
            }
            g.kanaele_geoeffnet(namen.clone(), if self.asio() { Some(self.einst.asio_eingang) } else { None });
            // jetzt mit den Kanälen der Zusatzquellen
            namen = g.anzeige.kanal_namen.lock().unwrap().clone();
        }
        let n = namen.len().min(MAX_KANAELE);
        let spalten = n.min(JE_REIHE) as i32;
        let reihen = n.div_ceil(JE_REIHE) as i32;
        let breite = (16 + spalten * STREIFEN + 16).max(820);
        let hoehe = 64 + reihen * STREIFEN_H + 8;
        let stil = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;
        let mut r = RECT { left: 0, top: 0, right: self.p(breite), bottom: self.p(hoehe) };
        let _ = AdjustWindowRectEx(&mut r, stil, false, WINDOW_EX_STYLE(0));
        let titel = if self.asio() { format!("Jamcenter – Mischpult ({})", self.einst.treiber) } else { "Jamcenter – Mischpult".to_string() };
        let Ok(fenster) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("JamcenterMischpult"),
            &HSTRING::from(titel),
            stil,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            r.right - r.left,
            r.bottom - r.top,
            self.hwnd,
            HMENU::default(),
            self.hinst,
            None,
        ) else {
            return;
        };
        dunkle_titelleiste(fenster);
        let (f, fk) = (self.font, self.font_klein);
        let mut karten = vec![(self.pr(8, 8, breite - 16, 46), String::new())];
        // Kopfzeile: Presets
        self.label_in(fenster, "Preset", (24, 22, 52, 20), C_TEXT2, f);
        let preset = self.auswahl_in(fenster, (80, 18, 220, 0), ID_PRESET, true);
        self.knopf_in(fenster, "Speichern", (308, 17, 96, 28), ID_PRESET_SPEICHERN, K_GEWAEHLT, f);
        self.knopf_in(fenster, "Löschen", (410, 17, 84, 28), ID_PRESET_LOESCHEN, 0, f);
        self.label_in(fenster, "Mischung vor dem Verschicken – ohne zusätzliche Latenz. FX = Equalizer, Kompressor, Hall je Eingang.", (520, 14, breite - 536, 34), C_TEXT2, fk);
        let stereo = g.regler.stereo.load(Relaxed);
        let mut streifen = Vec::new();
        let mut meter = Vec::new();
        for reihe in 0..reihen {
            karten.push((self.pr(8, 64 + reihe * STREIFEN_H, breite - 16, STREIFEN_H - 8), String::new()));
        }
        for (c, name) in namen.iter().take(n).enumerate() {
            let x = 16 + (c % JE_REIHE) as i32 * STREIFEN;
            let y = 72 + (c / JE_REIHE) as i32 * STREIFEN_H;
            self.label_in(fenster, &format!("{}\n{}", c + 1, name), (x, y, STREIFEN - 4, 30), C_TEXT2, fk);
            let pan = self.regler_in(fenster, (x, y + 32, STREIFEN - 6, 24), MP_ID + c as i32 * 8 + 2, 0, 40, (g.regler.kanal_pan[c].get() * 20.0 + 20.0).round() as i32, false);
            let _ = EnableWindow(pan, stereo);
            let pan_wert = self.ctl_in(fenster, w!("STATIC"), &pan_text(g.regler.kanal_pan[c].get()), 0x80, WINDOW_EX_STYLE(0), (x, y + 58, 36, 18), 0, fk);
            FARBEN.with(|m| m.borrow_mut().insert(pan_wert.0 as isize, C_TEXT2));
            let pan_mitte = self.knopf_in(fenster, "C", (x + 36, y + 56, 20, 20), MP_ID + c as i32 * 8 + 4, 0, fk);
            let _ = EnableWindow(pan_mitte, stereo);
            let db = g.regler.kanal_db[c].get();
            let fader = self.regler_in(fenster, (x + 4, y + 80, 28, 130), MP_ID + c as i32 * 8, 0, 50, (10.0 - db).round() as i32, true);
            meter.push(self.pr(x + 38, y + 86, 8, 118));
            let wert = self.label_in(fenster, &format!("{:+.0} dB", db), (x, y + 214, STREIFEN - 6, 18), C_TEXT, fk);
            let an = self.schalter_in(fenster, "", (x + 8, y + 236, STREIFEN - 14, 22), MP_ID + c as i32 * 8 + 1, g.regler.kanal_an[c].load(Relaxed));
            let fx_art = if g.regler.fx[c].aktiv() { K_GEWAEHLT } else { 0 };
            let fx = self.knopf_in(fenster, "FX", (x + 2, y + 260, STREIFEN - 8, 22), MP_ID + c as i32 * 8 + 3, fx_art, fk);
            streifen.push(Streifen { fader, wert, an, pan, pan_wert, pan_mitte, fx });
        }
        let preset_name = self.einst.mix_preset.clone();
        self.presets_fuellen(preset, &preset_name);
        self.mp = Some(Mischpult {
            hwnd: fenster,
            namen: namen.clone(),
            streifen,
            meter,
            anz: vec![0.0; n],
            preset,
            karten,
            bg: HBITMAP::default(),
        });
        let _ = ShowWindow(fenster, SW_SHOW);
    }

    unsafe fn mischpult_nachricht(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        let g = self.geteilt.clone();
        let r = &g.regler;
        match msg {
            WM_VSCROLL | WM_HSCROLL => {
                let h = HWND(lp.0 as _);
                let pos = SendMessageW(h, TBM_GETPOS, WPARAM(0), LPARAM(0)).0 as f32;
                let mp = self.mp.as_ref()?;
                if let Some(k) = mp.streifen.iter().position(|s| s.fader == h) {
                    let db = 10.0 - pos;
                    r.kanal_db[k].set(db);
                    let wert = mp.streifen[k].wert;
                    self.text_setzen(wert, &format!("{:+.0} dB", db));
                } else if let Some(k) = mp.streifen.iter().position(|s| s.pan == h) {
                    let pan = (pos - 20.0) / 20.0;
                    r.kanal_pan[k].set(pan);
                    let wert = mp.streifen[k].pan_wert;
                    self.text_setzen(wert, &pan_text(pan));
                }
                self.mischpult_geaendert();
                Some(LRESULT(0))
            }
            WM_COMMAND => {
                let id = (wp.0 & 0xFFFF) as i32;
                let code = ((wp.0 >> 16) & 0xFFFF) as u32;
                let h = HWND(lp.0 as _);
                if id >= MP_ID {
                    let c = ((id - MP_ID) / 8) as usize;
                    match (id - MP_ID) % 8 {
                        1 => {
                            r.kanal_an[c].store(schalter(h), Relaxed);
                            self.mischpult_geaendert();
                        }
                        3 => self.effekte_oeffnen(c),
                        4 => {
                            // Panorama in die Mitte
                            r.kanal_pan[c].set(0.0);
                            if let Some(s) = self.mp.as_ref().and_then(|m| m.streifen.get(c)) {
                                let (pan, wert) = (s.pan, s.pan_wert);
                                SendMessageW(pan, TBM_SETPOS, WPARAM(1), LPARAM(20));
                                self.text_setzen(wert, "Mitte");
                            }
                            self.mischpult_geaendert();
                        }
                        _ => {}
                    }
                    return Some(LRESULT(0));
                }
                match id {
                    ID_PRESET if code == CBN_SELCHANGE => {
                        let sel = SendMessageW(h, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
                        let liste = gemeinsam::presets_laden();
                        if let Some((name, mix)) = liste.get(sel.max(0) as usize) {
                            let n = self.mp.as_ref()?.streifen.len();
                            self.einst.stereo = gemeinsam::mix_anwenden(&g, mix, n);
                            self.einst.mix_preset = name.clone();
                            self.mischpult_auffrischen();
                            self.mischpult_geaendert();
                            self.einst.speichern();
                        }
                    }
                    ID_PRESET_SPEICHERN if code == BN_CLICKED => {
                        let mp = self.mp.as_ref()?;
                        let (fenster, preset, n) = (mp.hwnd, mp.preset, mp.streifen.len());
                        let name = text_von(preset).trim().to_string();
                        if name.is_empty() {
                            hinweis(fenster, "Bitte oben einen Namen für das Preset eintippen.");
                            return Some(LRESULT(0));
                        }
                        let mut liste = gemeinsam::presets_laden();
                        let mix = gemeinsam::mix_text(&g, n);
                        if let Some(e) = liste.iter_mut().find(|(n, _)| *n == name) {
                            if !frage(fenster, &format!("Preset „{name}“ überschreiben?")) {
                                return Some(LRESULT(0));
                            }
                            e.1 = mix;
                        } else {
                            liste.push((name.clone(), mix));
                        }
                        gemeinsam::presets_speichern(&liste);
                        self.einst.mix_preset = name.clone();
                        self.einst.speichern();
                        self.presets_fuellen(preset, &name);
                    }
                    ID_PRESET_LOESCHEN if code == BN_CLICKED => {
                        let mp = self.mp.as_ref()?;
                        let (fenster, preset) = (mp.hwnd, mp.preset);
                        let name = text_von(preset).trim().to_string();
                        let mut liste = gemeinsam::presets_laden();
                        if !liste.iter().any(|(n, _)| *n == name) {
                            hinweis(fenster, "Bitte zuerst ein gespeichertes Preset auswählen.");
                            return Some(LRESULT(0));
                        }
                        if frage(fenster, &format!("Preset „{name}“ wirklich löschen?")) {
                            liste.retain(|(n, _)| *n != name);
                            gemeinsam::presets_speichern(&liste);
                            self.presets_fuellen(preset, "");
                        }
                    }
                    _ => {}
                }
                Some(LRESULT(0))
            }
            WM_PAINT => {
                let radius_karte = self.p(14);
                let mp = self.mp.as_mut()?;
                if mp.bg.0.is_null() {
                    let karten: Vec<RECT> = mp.karten.iter().map(|k| k.0).collect();
                    mp.bg = hintergrund_anlegen(mp.hwnd, |m| {
                        for r in &karten {
                            runde_glatt(m, r, radius_karte, C_KARTE, Some(rgb(36, 36, 42)));
                        }
                    });
                }
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(mp.hwnd, &mut ps);
                let p = ps.rcPaint;
                gepuffert(hdc, &p, |hdc| {
                    hintergrund_zeigen(hdc, mp.bg);
                    for (c, r) in mp.meter.iter().enumerate() {
                        if schneidet(&p, r) {
                            pegel_zeichnen_direkt(hdc, r, mp.anz[c]);
                        }
                    }
                });
                let _ = EndPaint(mp.hwnd, &ps);
                Some(LRESULT(0))
            }
            WM_CLOSE => {
                self.mischpult_schliessen();
                Some(LRESULT(0))
            }
            _ => None,
        }
    }


    // ------------------------------------------------------------ Effektfenster (je Eingang)
    unsafe fn effekte_oeffnen(&mut self, c: usize) {
        if let Some(ef) = &mut self.ef {
            ef.kanal = c;
            ef.anz = [0.0; 3];
            ef.balken.fill(-120.0);
            self.geteilt.anzeige.spektrum_kanal.store(c as u32, Relaxed);
            let h = ef.hwnd;
            self.effekte_anzeigen();
            let _ = SetForegroundWindow(h);
            return;
        }
        let Some(mp) = &self.mp else { return };
        let besitzer = mp.hwnd;
        let stil = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;
        let (breite, hoehe) = (960, 630);
        let mut r = RECT { left: 0, top: 0, right: self.p(breite), bottom: self.p(hoehe) };
        let _ = AdjustWindowRectEx(&mut r, stil, false, WINDOW_EX_STYLE(0));
        let Ok(fenster) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("JamcenterEffekte"),
            w!("Jamcenter – Effekte"),
            stil,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            r.right - r.left,
            r.bottom - r.top,
            besitzer,
            HMENU::default(),
            self.hinst,
            None,
        ) else {
            return;
        };
        dunkle_titelleiste(fenster);
        let (f, fk) = (self.font, self.font_klein);
        let karten = vec![self.pr(8, 8, 944, 48), self.pr(8, 64, 944, 290), self.pr(8, 362, 560, 260), self.pr(576, 362, 376, 260)];
        // Kopf
        let titel = self.label_in(fenster, "", (24, 22, 330, 20), C_TEXT2, self.font_titel);
        self.label_in(fenster, "Vorlage", (370, 22, 56, 20), C_TEXT2, f);
        let vorlage = self.auswahl_in(fenster, (430, 17, 240, 0), ID_E_VORLAGE, false);
        let vorl: Vec<String> = std::iter::once("Instrument / Stimme wählen …".to_string()).chain(KANAL_VORLAGEN.iter().map(|v| v.name.to_string())).collect();
        Self::eintraege(vorlage, &vorl, 0);
        let tiefsperre = self.schalter_in(fenster, "Trittschallfilter (90 Hz)", (690, 18, 250, 24), ID_E_TIEFSPERRE, false);
        let mut regler = Vec::new();
        // Equalizer: Graph links, je Band ein Streifen
        let eq_an = self.schalter_in(fenster, "Equalizer", (24, 74, 150, 24), ID_E_EQ, false);
        self.label_in(fenster, "Punkte ziehen = Frequenz und Pegel, Mausrad auf einem Punkt = Güte (Q)", (190, 78, 360, 18), C_TEXT2, fk);
        let eq_graph = self.pr(24, 104, 520, 236);
        let mut freq = [HWND::default(); EQ_BAENDER];
        for b in 0..EQ_BAENDER {
            let x = 560 + b as i32 * 76;
            freq[b] = self.ctl_in(fenster, w!("STATIC"), "", 0x80 | 0x01 /* SS_NOPREFIX | SS_CENTER */, WINDOW_EX_STYLE(0), (x, 104, 72, 18), 0, f);
            FARBEN.with(|m| m.borrow_mut().insert(freq[b].0 as isize, EQ_FARBEN[b]));
            let (lo, hi) = FxP::Gain(b).bereich();
            let h = self.regler_in(fenster, (x + 22, 124, 28, 150), 0, lo, hi, 0, true);
            let w = self.ctl_in(fenster, w!("STATIC"), "", 0x81, WINDOW_EX_STYLE(0), (x, 276, 72, 18), 0, fk);
            regler.push((h, FxP::Gain(b), w));
            let (lo, hi) = FxP::Q(b).bereich();
            let h = self.regler_in(fenster, (x, 296, 72, 22), 0, lo, hi, 0, false);
            let w = self.ctl_in(fenster, w!("STATIC"), "", 0x81, WINDOW_EX_STYLE(0), (x, 320, 72, 18), 0, fk);
            regler.push((h, FxP::Q(b), w));
        }
        // Kompressor: Kennlinie, Anzeigen, Regler
        let komp_an = self.schalter_in(fenster, "Kompressor", (24, 372, 150, 24), ID_E_KOMP, false);
        self.label_in(fenster, "Kennlinie Eingang → Ausgang · ohne Vorausschau, keine Zusatzlatenz", (190, 376, 370, 18), C_TEXT2, fk);
        let komp_graph = self.pr(24, 402, 200, 200);
        let komp_meter = [self.pr(236, 402, 12, 186), self.pr(262, 402, 12, 186), self.pr(288, 402, 12, 186)];
        for (i, t) in ["In", "GR", "Out"].iter().enumerate() {
            self.ctl_in(fenster, w!("STATIC"), t, 0x81, WINDOW_EX_STYLE(0), (230 + i as i32 * 26, 590, 24, 16), 0, fk);
        }
        let komp_bereich = RECT { left: komp_graph.left, top: komp_graph.top, right: komp_meter[2].right, bottom: komp_graph.bottom };
        let zeilen = [
            ("Threshold", FxP::Schwelle),
            ("Ratio", FxP::Ratio),
            ("Attack", FxP::Attack),
            ("Release", FxP::Release),
            ("Knee", FxP::Knee),
            ("Make-up", FxP::Makeup),
        ];
        for (i, (t, p)) in zeilen.iter().enumerate() {
            let y = 404 + i as i32 * 32;
            self.label_in(fenster, t, (316, y + 4, 70, 20), C_TEXT2, f);
            let (lo, hi) = p.bereich();
            let h = self.regler_in(fenster, (386, y, 116, 26), 0, lo, hi, 0, false);
            let w = self.label_in(fenster, "", (504, y + 4, 58, 20), C_TEXT, fk);
            regler.push((h, *p, w));
        }
        // Hall
        let hall_an = self.schalter_in(fenster, "Hall", (592, 372, 150, 24), ID_E_HALL, false);
        let zeilen = [("Anteil", FxP::HallMix), ("Größe", FxP::HallGroesse), ("Abklingen", FxP::HallAbkling)];
        for (i, (t, p)) in zeilen.iter().enumerate() {
            let y = 414 + i as i32 * 44;
            self.label_in(fenster, t, (592, y + 4, 80, 20), C_TEXT2, f);
            let (lo, hi) = p.bereich();
            let h = self.regler_in(fenster, (672, y, 200, 26), 0, lo, hi, 0, false);
            let w = self.label_in(fenster, "", (876, y + 4, 64, 20), C_TEXT, fk);
            regler.push((h, *p, w));
        }
        self.label_in(
            fenster,
            "Der Hall kommt zur Fahne dazu – das direkte Signal geht ohne Verzögerung raus.",
            (592, 550, 344, 40),
            C_TEXT2,
            fk,
        );
        self.ef = Some(Effekte {
            hwnd: fenster,
            kanal: c,
            titel,
            vorlage,
            tiefsperre,
            eq_an,
            komp_an,
            hall_an,
            regler,
            freq,
            eq_graph,
            komp_graph,
            komp_meter,
            komp_bereich,
            anz: [0.0; 3],
            ziehen: None,
            band: 2,
            karten,
            bg: HBITMAP::default(),
            balken: vec![-120.0; SPEKTRUM_BAENDER],
        });
        self.geteilt.anzeige.spektrum_kanal.store(c as u32, Relaxed);
        self.effekte_anzeigen();
        let _ = ShowWindow(fenster, SW_SHOW);
    }

    /// Alle Steuerelemente des Effektfensters auf den Kanal setzen
    unsafe fn effekte_anzeigen(&mut self) {
        let g = self.geteilt.clone();
        let Some(ef) = &self.ef else { return };
        let c = ef.kanal;
        let fx = &g.regler.fx[c];
        let name = self.mp.as_ref().and_then(|m| m.namen.get(c).cloned()).unwrap_or_default();
        let mut texte = vec![(ef.titel, format!("EFFEKTE – KANAL {}: {}", c + 1, name.to_uppercase()))];
        schalter_setzen(ef.tiefsperre, fx.tiefsperre.load(Relaxed));
        schalter_setzen(ef.eq_an, fx.eq_an.load(Relaxed));
        schalter_setzen(ef.komp_an, fx.komp_an.load(Relaxed));
        schalter_setzen(ef.hall_an, fx.hall_an.load(Relaxed));
        SendMessageW(ef.vorlage, CB_SETCURSEL, WPARAM(0), LPARAM(0));
        for &(h, p, w) in &ef.regler {
            let v = p.atom(fx).get();
            SendMessageW(h, TBM_SETPOS, WPARAM(1), LPARAM(p.zu_pos(v) as isize));
            texte.push((w, p.text(v)));
            if let FxP::Q(b) = p {
                // Kuhschwanz-Filter (Shelf) haben wie im Browser keine Güte
                let _ = EnableWindow(h, EQ_VORGABE[b].0 == 1);
            }
        }
        for b in 0..EQ_BAENDER {
            texte.push((ef.freq[b], freq_text(fx.eq_freq[b].get())));
        }
        let h = ef.hwnd;
        for (hw, t) in texte {
            self.text_setzen(hw, &t);
        }
        let _ = InvalidateRect(h, None, false);
    }

    unsafe fn effekte_malen(&mut self) {
        let g = self.geteilt.clone();
        let radius_karte = self.p(14);
        if let Some(ef) = &mut self.ef {
            if ef.bg.0.is_null() {
                let karten = ef.karten.clone();
                ef.bg = hintergrund_anlegen(ef.hwnd, |m| {
                    for r in &karten {
                        runde_glatt(m, r, radius_karte, C_KARTE, Some(rgb(36, 36, 42)));
                    }
                });
            }
        }
        let Some(ef) = &self.ef else { return };
        let fx = &g.regler.fx[ef.kanal];
        let mut ps = PAINTSTRUCT::default();
        let hdc_fenster = BeginPaint(ef.hwnd, &mut ps);
        let p = ps.rcPaint;
        // alles erst unsichtbar zeichnen und dann in einem Stück zeigen – kein Flackern
        gepuffert(hdc_fenster, &p, |hdc| {
        hintergrund_zeigen(hdc, ef.bg);
        let alt_font = SelectObject(hdc, self.font_klein);
        if schneidet(&p, &ef.eq_graph) {
            // ---- EQ-Graph
            let gr = ef.eq_graph;
            let (w, h) = (gr.right - gr.left, gr.bottom - gr.top);
            let an = fx.eq_an.load(Relaxed);
            let baender: Vec<(u8, f32, f32, f32)> = (0..EQ_BAENDER).map(|b| (EQ_VORGABE[b].0, fx.eq_freq[b].get(), fx.eq_gain[b].get(), fx.eq_q[b].get())).collect();
            let gewaehlt = ef.band;
            let ss = SS as f32;
            let s = self.s;
            let pp = |v: i32| (v as f32 * s).round() as i32;
            let schrift = self.font_klein;
            {
            fuellen(hdc, &gr, rgb(16, 16, 20));
            // Frequenzanzeige (Signal nach dem EQ): viele kleine Balken, −84 dB unten … 0 dB oben
            let nb = ef.balken.len();
            let x_von = |f: f32| gr.left + (w as f32 * ((f / 20.0).ln() / 1000f32.ln())) as i32;
            for (i, &db) in ef.balken.iter().enumerate() {
                let (f0, f1) = spektrum_band(i, nb);
                let (x0, x1) = (x_von(f0) + 1, x_von(f1) - 1);
                let anteil = ((db + 84.0) / 84.0).clamp(0.0, 1.0);
                if anteil <= 0.0 {
                    continue;
                }
                let y = gr.bottom - (h as f32 * anteil) as i32;
                let r = RECT { left: x0, top: y, right: x1.max(x0 + 1), bottom: gr.bottom };
                fuellen(hdc, &r, if an { rgb(58, 34, 42) } else { rgb(40, 40, 48) });
                fuellen(hdc, &RECT { bottom: (y + 2).min(gr.bottom), ..r }, if an { rgb(150, 64, 80) } else { rgb(90, 90, 100) });
            }
            glatt(hdc, &gr, |m| {
                let lx = |f: f32| (w as f32 * ((f / 20.0).ln() / 1000f32.ln()) * ss) as i32;
                let ly = |db: f32| ((h as f32 * (18.0 - db) / 36.0) * ss) as i32;
                for f in [50.0, 100.0, 200.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0] {
                    linienzug(m, &[POINT { x: lx(f), y: 0 }, POINT { x: lx(f), y: h * SS }], rgb(34, 34, 42), SS);
                }
                for db in [-12.0, -6.0, 0.0, 6.0, 12.0] {
                    let farbe = if db == 0.0 { rgb(60, 60, 70) } else { rgb(34, 34, 42) };
                    linienzug(m, &[POINT { x: 0, y: ly(db) }, POINT { x: w * SS, y: ly(db) }], farbe, SS);
                }
                // Gesamtkurve aus den echten Filterantworten
                let mut pkt = Vec::with_capacity(w as usize + 1);
                for i in (0..=w).step_by(2) {
                    let f = 20.0 * 1000f32.powf(i as f32 / w as f32);
                    let db: f32 = baender.iter().map(|&(t, f0, g0, q)| mischer::band_antwort_db(t, f0, g0, q, f)).sum();
                    pkt.push(POINT { x: i * SS, y: ly(db.clamp(-19.0, 19.0)) });
                }
                linienzug(m, &pkt, if an { C_AKZENT } else { rgb(110, 110, 122) }, 2 * SS);
                for (b, &(_, f0, g0, _)) in baender.iter().enumerate() {
                    let (x, y) = (lx(f0), ly(g0.clamp(-18.0, 18.0)));
                    let farbe = if an { EQ_FARBEN[b] } else { rgb(90, 90, 100) };
                    let rand = if b == gewaehlt { rgb(250, 250, 252) } else { farbe };
                    kreis(m, x, y, 8 * SS, rand, rand);
                    kreis(m, x, y, 6 * SS, farbe, farbe);
                }
            });
            let alt = SelectObject(hdc, schrift);
            for (f, t) in [(100.0, "100"), (1000.0, "1k"), (10000.0, "10k")] {
                let x = gr.left + (w as f32 * ((f / 20.0f32).ln() / 1000f32.ln())) as i32;
                text_zeichnen(hdc, t, &RECT { left: x + 3, top: gr.bottom - pp(16), right: x + pp(40), bottom: gr.bottom }, C_TEXT2, DT_LEFT | DT_SINGLELINE);
            }
            for (db, t) in [(12.0, "+12"), (0.0, "0 dB"), (-12.0, "−12")] {
                let y = gr.top + (h as f32 * (18.0 - db) / 36.0) as i32;
                text_zeichnen(hdc, t, &RECT { left: gr.left + 4, top: y - pp(15), right: gr.left + pp(60), bottom: y }, C_TEXT2, DT_LEFT | DT_SINGLELINE);
            }
            SelectObject(hdc, alt);
            }
        }
        // ---- Kompressor-Kennlinie mit Live-Punkt
        if schneidet(&p, &ef.komp_bereich) {
        let gr = ef.komp_graph;
        let (w, h) = (gr.right - gr.left, gr.bottom - gr.top);
        let an = fx.komp_an.load(Relaxed);
        let (th, ra, kn, mu) = (fx.schwelle.get(), fx.ratio.get(), fx.knee.get(), fx.makeup.get());
        let ein_db = if ef.anz[0] > 0.0 { 20.0 * ef.anz[0].log10() } else { -100.0 };
        {
        fuellen(hdc, &gr, rgb(16, 16, 20));
        glatt(hdc, &gr, |m| {
            let px = |db: f32| (((db + 60.0) / 60.0 * w as f32) * SS as f32) as i32;
            let py = |db: f32| ((h as f32 - (db.clamp(-60.0, 0.0) + 60.0) / 60.0 * h as f32) * SS as f32) as i32;
            for i in 1..6 {
                let d = -60.0 + i as f32 * 10.0;
                linienzug(m, &[POINT { x: px(d), y: 0 }, POINT { x: px(d), y: h * SS }], rgb(34, 34, 42), SS);
                linienzug(m, &[POINT { x: 0, y: py(d) }, POINT { x: w * SS, y: py(d) }], rgb(34, 34, 42), SS);
            }
            linienzug(m, &[POINT { x: 0, y: h * SS }, POINT { x: w * SS, y: 0 }], rgb(60, 60, 70), SS);
            linienzug(m, &[POINT { x: px(th), y: 0 }, POINT { x: px(th), y: h * SS }], C_AKZENT_GRUND, SS);
            let pkt: Vec<POINT> = (0..=120)
                .map(|i| {
                    let e = -60.0 + i as f32 * 0.5;
                    POINT { x: px(e), y: py(mischer::komp_kennlinie(e, th, ra, kn) + mu) }
                })
                .collect();
            linienzug(m, &pkt, if an { C_AKZENT } else { rgb(110, 110, 122) }, 2 * SS);
            if an && ein_db > -60.0 {
                let e = ein_db.min(0.0);
                kreis(m, px(e), py(mischer::komp_kennlinie(e, th, ra, kn) + mu), 5 * SS, rgb(250, 250, 252), rgb(250, 250, 252));
            }
        });
        }
        pegel_zeichnen_direkt(hdc, &ef.komp_meter[0], ef.anz[0]);
        let r = ef.komp_meter[1];
        let anteil = (ef.anz[1] / 24.0).clamp(0.0, 1.0);
        fuellen(hdc, &r, rgb(30, 30, 36));
        fuellen(hdc, &RECT { left: r.left, top: r.top, right: r.right, bottom: r.top + ((r.bottom - r.top) as f32 * anteil) as i32 }, C_ORANGE);
        pegel_zeichnen_direkt(hdc, &ef.komp_meter[2], ef.anz[2]);
        }
        SelectObject(hdc, alt_font);
        });
        let _ = EndPaint(ef.hwnd, &ps);
    }

    /// Nach einer Änderung im Effektfenster: Abschnitt einschalten, Anzeige neu
    unsafe fn effekt_geaendert(&mut self, abschnitt: u8) {
        let g = self.geteilt.clone();
        let Some(ef) = &self.ef else { return };
        let fx = &g.regler.fx[ef.kanal];
        let (schalter_h, flag) = match abschnitt {
            0 => (ef.eq_an, &fx.eq_an),
            1 => (ef.komp_an, &fx.komp_an),
            _ => (ef.hall_an, &fx.hall_an),
        };
        // wer an einem Regler dreht, will den Effekt hören
        flag.store(true, Relaxed);
        schalter_setzen(schalter_h, true);
        let r = if abschnitt == 0 { ef.eq_graph } else { ef.komp_bereich };
        let _ = InvalidateRect(ef.hwnd, Some(&r), false);
    }

    unsafe fn effekte_nachricht(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        let g = self.geteilt.clone();
        let xy = |lp: LPARAM| ((lp.0 & 0xFFFF) as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as i16 as i32);
        let drin = |r: &RECT, x: i32, y: i32| x >= r.left && x < r.right && y >= r.top && y < r.bottom;
        match msg {
            WM_VSCROLL | WM_HSCROLL => {
                let h = HWND(lp.0 as _);
                let ef = self.ef.as_ref()?;
                let &(_, p, w) = ef.regler.iter().find(|r| r.0 == h)?;
                let fx = &g.regler.fx[ef.kanal];
                let pos = SendMessageW(h, TBM_GETPOS, WPARAM(0), LPARAM(0)).0 as i32;
                let v = p.zu_wert(pos);
                p.atom(fx).set(v);
                if let FxP::Gain(b) | FxP::Q(b) = p {
                    if let Some(ef) = &mut self.ef {
                        ef.band = b;
                    }
                }
                self.text_setzen(w, &p.text(v));
                self.effekt_geaendert(p.abschnitt());
                self.mischpult_geaendert();
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN => {
                let (x, y) = xy(lp);
                let grenze = self.p(16) as f32;
                let ef = self.ef.as_mut()?;
                if drin(&ef.eq_graph, x, y) {
                    let fx = &g.regler.fx[ef.kanal];
                    let mut best = None;
                    for b in 0..EQ_BAENDER {
                        let (px, py) = eq_xy(&ef.eq_graph, fx.eq_freq[b].get(), fx.eq_gain[b].get());
                        let d = ((px - x as f32).powi(2) + (py - y as f32).powi(2)).sqrt();
                        if d < grenze && best.is_none_or(|(_, bd)| d < bd) {
                            best = Some((b, d));
                        }
                    }
                    if let Some((b, _)) = best {
                        ef.ziehen = Some(b);
                        ef.band = b;
                        SetCapture(ef.hwnd);
                        let _ = SetFocus(ef.hwnd);
                        let _ = InvalidateRect(ef.hwnd, Some(&ef.eq_graph), false);
                    }
                }
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE => {
                let (x, y) = xy(lp);
                let ef = self.ef.as_ref()?;
                let b = ef.ziehen?;
                let fx = &g.regler.fx[ef.kanal];
                let (f, db) = eq_von_xy(&ef.eq_graph, x, y);
                fx.eq_freq[b].set(f);
                fx.eq_gain[b].set(db);
                let mut texte = vec![(ef.freq[b], freq_text(f))];
                if let Some(&(h, p, w)) = ef.regler.iter().find(|r| r.1 == FxP::Gain(b)) {
                    SendMessageW(h, TBM_SETPOS, WPARAM(1), LPARAM(p.zu_pos(db) as isize));
                    texte.push((w, p.text(db)));
                }
                for (h, t) in texte {
                    self.text_setzen(h, &t);
                }
                self.effekt_geaendert(0);
                Some(LRESULT(0))
            }
            WM_LBUTTONUP | WM_CAPTURECHANGED => {
                let ef = self.ef.as_mut()?;
                if ef.ziehen.take().is_some() {
                    if msg == WM_LBUTTONUP {
                        let _ = ReleaseCapture();
                    }
                    self.mischpult_geaendert();
                }
                Some(LRESULT(0))
            }
            WM_MOUSEWHEEL => {
                let schritte = ((wp.0 >> 16) & 0xFFFF) as i16 as f32 / 120.0;
                let mut pt = POINT { x: (lp.0 & 0xFFFF) as i16 as i32, y: ((lp.0 >> 16) & 0xFFFF) as i16 as i32 };
                let ef = self.ef.as_ref()?;
                let _ = ScreenToClient(ef.hwnd, &mut pt);
                if !drin(&ef.eq_graph, pt.x, pt.y) {
                    return Some(LRESULT(0));
                }
                let fx = &g.regler.fx[ef.kanal];
                // Band unter der Maus, sonst das zuletzt gewählte
                let mut b = ef.band;
                let mut nah = self.p(30) as f32;
                for i in 0..EQ_BAENDER {
                    let (px, py) = eq_xy(&ef.eq_graph, fx.eq_freq[i].get(), fx.eq_gain[i].get());
                    let d = ((px - pt.x as f32).powi(2) + (py - pt.y as f32).powi(2)).sqrt();
                    if d < nah {
                        nah = d;
                        b = i;
                    }
                }
                if EQ_VORGABE[b].0 != 1 {
                    return Some(LRESULT(0));
                }
                let q = (fx.eq_q[b].get() * 1.15f32.powf(schritte)).clamp(0.2, 12.0);
                fx.eq_q[b].set(q);
                let mut texte = Vec::new();
                if let Some(&(h, p, w)) = ef.regler.iter().find(|r| r.1 == FxP::Q(b)) {
                    SendMessageW(h, TBM_SETPOS, WPARAM(1), LPARAM(p.zu_pos(q) as isize));
                    texte.push((w, p.text(q)));
                }
                for (h, t) in texte {
                    self.text_setzen(h, &t);
                }
                if let Some(ef) = &mut self.ef {
                    ef.band = b;
                }
                self.effekt_geaendert(0);
                self.mischpult_geaendert();
                Some(LRESULT(0))
            }
            WM_COMMAND => {
                let id = (wp.0 & 0xFFFF) as i32;
                let code = ((wp.0 >> 16) & 0xFFFF) as u32;
                let h = HWND(lp.0 as _);
                let ef = self.ef.as_ref()?;
                let c = ef.kanal;
                let fx = &g.regler.fx[c];
                match id {
                    ID_E_TIEFSPERRE => fx.tiefsperre.store(schalter(h), Relaxed),
                    ID_E_EQ => fx.eq_an.store(schalter(h), Relaxed),
                    ID_E_KOMP => fx.komp_an.store(schalter(h), Relaxed),
                    ID_E_HALL => fx.hall_an.store(schalter(h), Relaxed),
                    ID_E_VORLAGE if code == CBN_SELCHANGE => {
                        let sel = SendMessageW(h, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0;
                        if sel <= 0 {
                            return Some(LRESULT(0));
                        }
                        mischer::vorlage_anwenden(&g, c, sel as usize - 1);
                        self.effekte_anzeigen();
                        // Vorlagen-Namen stehen lassen, damit man sieht, was gewählt wurde
                        if let Some(ef) = &self.ef {
                            SendMessageW(ef.vorlage, CB_SETCURSEL, WPARAM(sel as usize), LPARAM(0));
                        }
                    }
                    _ => return Some(LRESULT(0)),
                }
                if let Some(ef) = &self.ef {
                    let _ = InvalidateRect(ef.hwnd, None, false);
                }
                self.mischpult_geaendert();
                Some(LRESULT(0))
            }
            WM_PAINT => {
                self.effekte_malen();
                Some(LRESULT(0))
            }
            WM_CLOSE => {
                if let Some(ef) = self.ef.take() {
                    let _ = DestroyWindow(ef.hwnd);
                    let _ = DeleteObject(ef.bg);
            self.geteilt.anzeige.spektrum_kanal.store(u32::MAX, Relaxed);
                }
                self.einst.speichern();
                Some(LRESULT(0))
            }
            _ => None,
        }
    }


    // ------------------------------------------------------------ Aufnahme (läuft auf dem Server)
    /// Ablage-Kennung aus Raumname und Passwort (wie die Verschlüsselung)
    unsafe fn aufnahme_kennung(&mut self) -> Option<[u8; 32]> {
        self.felder_lesen();
        if self.einst.raum.trim().is_empty() {
            hinweis(self.hwnd, "Bitte zuerst einen Raumnamen eintragen – Aufnahmen gehören zum Raum (und seinem Passwort).");
            return None;
        }
        if self.einst.server.trim().is_empty() {
            hinweis(self.hwnd, KEIN_SERVER);
            return None;
        }
        Some(crate::netz::aufnahme_kennung(&self.einst.raum, &self.einst.passwort))
    }

    unsafe fn aufnahme_umschalten(&mut self) {
        if self.sitzung.is_none() {
            hinweis(self.hwnd, "Erst verbinden: Die Aufnahme läuft auf dem Server und nimmt beide Räume zeitrichtig zusammen auf.");
            return;
        }
        let Some(kennung) = self.aufnahme_kennung() else { return };
        let g = self.geteilt.clone();
        let an = !g.aufnahme_an.load(Relaxed);
        // Metronom mit aufnehmen (wenn es läuft): Tempo kommt in den Dateinamen
        let mit_metronom = self.einst.aufnahme_metronom && g.metronom.an.load(Relaxed);
        let mut name = zeit_name();
        if mit_metronom {
            name.push_str(&format!("_{}bpm", g.metronom.bpm.get().round() as i32));
        }
        let klang = mit_metronom.then(|| g.regler.metronom_klang.load(Relaxed) as u8);
        *g.aufnahme_wunsch.lock().unwrap() = Some((an, kennung, name, klang));
    }

    /// Aufnahme-Knopf: rot mit laufender Zeit während der Aufnahme; nach dem Stoppen die Liste zeigen
    unsafe fn aufnahme_anzeigen(&mut self) {
        let g = self.geteilt.clone();
        let an = self.sitzung.is_some() && g.aufnahme_an.load(Relaxed);
        let Some(knopf) = self.c.as_ref().map(|c| c.aufnahme) else { return };
        let text = if an {
            let sek = ((uhr_us() as i64 + g.uhr_versatz.load(Relaxed) - g.aufnahme_start.load(Relaxed) as i64) / 1_000_000).max(0);
            format!("■ {:02}:{:02}", sek / 60, sek % 60)
        } else {
            "● Aufnahme".to_string()
        };
        art_setzen(knopf, K_PRIMAER, an);
        self.text_setzen(knopf, &text);
        if self.aufn_war_an && !an && self.sitzung.is_some() {
            // gerade gestoppt (hier oder im anderen Raum): Liste zeigen, die neue Aufnahme wird umgewandelt
            self.aufnahmen_oeffnen();
        }
        self.aufn_war_an = an;
    }

    unsafe fn aufnahmen_oeffnen(&mut self) {
        if let Some(au) = &self.au {
            let h = au.hwnd;
            let _ = SetForegroundWindow(h);
            self.aufnahmen_laden();
            return;
        }
        let Some(kennung) = self.aufnahme_kennung() else { return };
        let stil = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;
        let mut r = RECT { left: 0, top: 0, right: self.p(AU_BREITE), bottom: self.p(200) };
        let _ = AdjustWindowRectEx(&mut r, stil, false, WINDOW_EX_STYLE(0));
        let titel = format!("Jamcenter – Aufnahmen und Spuren im Raum „{}“", self.einst.raum.trim());
        let Ok(fenster) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("JamcenterAufnahmen"),
            &HSTRING::from(titel),
            stil,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            r.right - r.left,
            r.bottom - r.top,
            self.hwnd,
            HMENU::default(),
            self.hinst,
            None,
        ) else {
            return;
        };
        dunkle_titelleiste(fenster);
        self.label_in(fenster, "AUFNAHMEN UND SPUREN", (24, 22, 240, 20), C_TEXT2, self.font_titel);
        self.knopf_in(fenster, "Spur hochladen …", (390, 16, 150, 30), ID_AU_HOCH, 0, self.font);
        self.knopf_in(fenster, "Aktualisieren", (546, 16, 120, 30), ID_AU_NEU, 0, self.font);
        self.label_in(fenster, "Lautstärke der Spur bei dir", (24, 62, 190, 20), C_TEXT2, self.font);
        self.regler_in(fenster, (214, 58, 200, 26), ID_AU_SPUR_LAUT, 0, 46, (self.einst.spur_db + 40.0).round() as i32, false);
        let status = self.label_in(fenster, "Liste wird geladen …", (24, 96, 642, 20), C_TEXT2, self.font);
        self.au = Some(AufnahmenFenster {
            hwnd: fenster,
            kennung,
            host: aufnahmen::host(&self.einst.server),
            eintraege: Vec::new(),
            steuer: Vec::new(),
            status,
            liste: Arc::new(Mutex::new(None)),
            meldung: Arc::new(Mutex::new(None)),
            laedt: false,
            zuletzt: Instant::now(),
            spielt: None,
            hoch: Arc::new(Mutex::new(None)),
            hoch_laeuft: false,
        });
        let _ = ShowWindow(fenster, SW_SHOW);
        self.aufnahmen_laden();
    }

    /// Liste im Hintergrund vom Server holen (meldet sich mit WM_AU_LISTE)
    unsafe fn aufnahmen_laden(&mut self) {
        let Some(au) = &mut self.au else { return };
        if au.laedt {
            return;
        }
        au.laedt = true;
        au.zuletzt = Instant::now();
        let (host, kennung, ziel, h) = (au.host.clone(), au.kennung, au.liste.clone(), au.hwnd.0 as isize);
        std::thread::spawn(move || {
            *ziel.lock().unwrap() = Some(aufnahmen::liste(&host, &kennung));
            unsafe {
                let _ = PostMessageW(HWND(h as _), WM_AU_LISTE, WPARAM(0), LPARAM(0));
            }
        });
    }

    /// Zeilen der Liste neu aufbauen
    unsafe fn aufnahmen_zeilen(&mut self) {
        let Some(au) = &mut self.au else { return };
        for h in au.steuer.drain(..) {
            FARBEN.with(|m| m.borrow_mut().remove(&(h.0 as isize)));
            let _ = DestroyWindow(h);
        }
        let liste: Vec<Eintrag> = au.eintraege.iter().take(30).cloned().collect();
        let (fenster, status) = (au.hwnd, au.status);
        let mut steuer = Vec::new();
        let (spur_datei, spur_an) = self.spur_jetzt();
        for (i, e) in liste.iter().enumerate() {
            let y = 128 + i as i32 * 40;
            let i = i as i32;
            steuer.push(self.label_in(fenster, &aufnahmen::anzeige_name(&e.datei), (24, y + 8, 200, 20), C_TEXT, self.font_fett));
            if e.fertig {
                let art = if e.datei.ends_with(".wav") { "WAV" } else { "MP3" };
                let info = if aufnahmen::ist_aufnahme(&e.datei) {
                    aufnahmen::zeit_text(aufnahmen::dauer_sek(&e.datei, e.groesse))
                } else {
                    aufnahmen::groesse_text(e.groesse)
                };
                steuer.push(self.label_in(fenster, &format!("{art} · {info}"), (228, y + 10, 88, 20), C_TEXT2, self.font_klein));
                let spielt = self.au.as_ref().is_some_and(|a| a.spielt.as_ref().is_some_and(|s| s.0 == i as usize));
                steuer.push(self.knopf_in(fenster, if spielt { "■" } else { "▶" }, (320, y + 2, 42, 30), AU_ID + 4 * i + 2, if spielt { K_PRIMAER } else { 0 }, self.font));
                let fuer_alle = spur_an && spur_datei == e.datei;
                steuer.push(self.knopf_in(fenster, if fuer_alle { "■ Stopp" } else { "Für alle ▶" }, (368, y + 2, 100, 30), AU_ID + 4 * i + 3, if fuer_alle { K_PRIMAER } else { 0 }, self.font));
                steuer.push(self.knopf_in(fenster, "Download", (474, y + 2, 100, 30), AU_ID + 4 * i, K_GEWAEHLT, self.font));
                steuer.push(self.knopf_in(fenster, "Löschen", (580, y + 2, 86, 30), AU_ID + 4 * i + 1, 0, self.font));
            } else {
                steuer.push(self.label_in(fenster, "wird zu MP3 umgewandelt …", (228, y + 10, 340, 20), C_GELB, self.font));
            }
        }
        let text = if liste.is_empty() {
            "Noch nichts da. Aufnehmen mit „● Aufnahme“ oder eine Spur (z. B. Schlagzeug) hochladen."
        } else {
            "▶ = nur bei dir anhören · „Für alle“ = alle im Raum hören die Spur gleichzeitig und spielen dazu."
        };
        self.text_setzen(status, text);
        let stil = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let mut r = RECT { left: 0, top: 0, right: self.p(AU_BREITE), bottom: self.p(128 + liste.len().max(1) as i32 * 40 + 16) };
        let _ = AdjustWindowRectEx(&mut r, stil, false, WINDOW_EX_STYLE(0));
        let _ = SetWindowPos(fenster, HWND::default(), 0, 0, r.right - r.left, r.bottom - r.top, SWP_NOMOVE | SWP_NOZORDER);
        let _ = InvalidateRect(fenster, None, false);
        if let Some(au) = &mut self.au {
            au.steuer = steuer;
        }
    }

    unsafe fn aufnahmen_nachricht(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_AU_LISTE => {
                let au = self.au.as_mut()?;
                au.laedt = false;
                let erg = au.liste.lock().unwrap().take();
                match erg {
                    Some(Ok(l)) => {
                        // angehörte Aufnahme in der neuen Liste wiederfinden
                        if let Some((i, _)) = &mut au.spielt {
                            let name = au.eintraege.get(*i).map(|e| e.datei.clone()).unwrap_or_default();
                            *i = l.iter().position(|e| e.datei == name).unwrap_or(usize::MAX);
                        }
                        au.eintraege = l;
                        self.aufnahmen_zeilen();
                    }
                    Some(Err(e)) => {
                        let st = au.status;
                        self.text_setzen(st, &format!("Fehler: {e}"));
                    }
                    None => {}
                }
                Some(LRESULT(0))
            }
            WM_AU_FERTIG => {
                let au = self.au.as_ref()?;
                let (fenster, erg) = (au.hwnd, au.meldung.lock().unwrap().take());
                match erg {
                    Some(Ok(pfad)) => {
                        let arg = format!("/select,\"{}\"", pfad.display());
                        ShellExecuteW(fenster, w!("open"), w!("explorer.exe"), &HSTRING::from(arg), PCWSTR::null(), SW_SHOWNORMAL);
                        let st = au.status;
                        self.text_setzen(st, &format!("Gespeichert: {}", pfad.display()));
                    }
                    Some(Err(e)) => hinweis(fenster, &format!("Download fehlgeschlagen: {e}")),
                    None => {}
                }
                Some(LRESULT(0))
            }
            WM_COMMAND => {
                let id = (wp.0 & 0xFFFF) as i32;
                if id == ID_AU_NEU {
                    self.aufnahmen_laden();
                } else if id == ID_AU_HOCH {
                    self.spur_hochladen();
                } else if id >= AU_ID && (id - AU_ID) % 4 == 2 {
                    self.anhoeren(((id - AU_ID) / 4) as usize);
                } else if id >= AU_ID && (id - AU_ID) % 4 == 3 {
                    self.spur_fuer_alle(((id - AU_ID) / 4) as usize);
                } else if id >= AU_ID {
                    let au = self.au.as_ref()?;
                    let i = ((id - AU_ID) / 4) as usize;
                    let e = au.eintraege.get(i)?.clone();
                    let (fenster, host, kennung, st, meldung) = (au.hwnd, au.host.clone(), au.kennung, au.status, au.meldung.clone());
                    if (id - AU_ID) % 4 == 0 {
                        self.text_setzen(st, &format!("Lade {} …", aufnahmen::anzeige_name(&e.datei)));
                        let ziel = download_ordner().join(format!("Jamcenter {}", e.datei));
                        let h = fenster.0 as isize;
                        std::thread::spawn(move || {
                            let erg = aufnahmen::holen(&host, &kennung, &e.datei, &ziel).map(|_| ziel);
                            *meldung.lock().unwrap() = Some(erg);
                            unsafe {
                                let _ = PostMessageW(HWND(h as _), WM_AU_FERTIG, WPARAM(0), LPARAM(0));
                            }
                        });
                    } else if frage(fenster, &format!("Aufnahme vom {} wirklich löschen? Sie ist danach für beide Räume weg.", aufnahmen::anzeige_name(&e.datei))) {
                        self.anhoeren_stoppen();
                        if let Err(f) = aufnahmen::loeschen(&host, &kennung, &e.datei) {
                            hinweis(fenster, &format!("Löschen fehlgeschlagen: {f}"));
                        }
                        self.aufnahmen_laden();
                    }
                }
                Some(LRESULT(0))
            }
            WM_AU_HOCH => {
                let au = self.au.as_mut()?;
                au.hoch_laeuft = false;
                let (fenster, st, erg) = (au.hwnd, au.status, au.hoch.lock().unwrap().take());
                match erg {
                    Some(Ok(name)) => {
                        self.text_setzen(st, &format!("Hochgeladen: {name} – mit „Für alle ▶“ für den ganzen Raum abspielen."));
                        self.aufnahmen_laden();
                    }
                    Some(Err(e)) => hinweis(fenster, &format!("Hochladen fehlgeschlagen: {e}")),
                    None => {}
                }
                Some(LRESULT(0))
            }
            WM_HSCROLL => {
                if lp.0 != 0 && GetDlgCtrlID(HWND(lp.0 as _)) == ID_AU_SPUR_LAUT {
                    let pos = SendMessageW(HWND(lp.0 as _), TBM_GETPOS, WPARAM(0), LPARAM(0)).0 as f32;
                    self.einst.spur_db = pos - 40.0;
                    self.geteilt.regler.spur_db.set(self.einst.spur_db);
                    if (wp.0 & 0xFFFF) as u32 == TB_ENDTRACK {
                        self.einst.speichern();
                    }
                }
                Some(LRESULT(0))
            }
            WM_PAINT => {
                let au = self.au.as_ref()?;
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(au.hwnd, &mut ps);
                fuellen(hdc, &ps.rcPaint, C_KARTE);
                let _ = EndPaint(au.hwnd, &ps);
                Some(LRESULT(0))
            }
            WM_CLOSE => {
                self.anhoeren_stoppen();
                if let Some(au) = self.au.take() {
                    for h in &au.steuer {
                        FARBEN.with(|m| m.borrow_mut().remove(&(h.0 as isize)));
                    }
                    let _ = DestroyWindow(au.hwnd);
                }
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    /// Aktuelle Spur des Raums: (Datei, läuft)
    fn spur_jetzt(&self) -> (String, bool) {
        let g = &self.geteilt;
        let an = self.sitzung.is_some() && g.spur_an.load(Relaxed);
        (g.spur_datei.lock().unwrap().0.clone(), an)
    }

    /// Spur des Raums laden, sobald sie läuft (auch wenn sie im anderen Raum gestartet wurde)
    unsafe fn spur_takt(&mut self) {
        let g = self.geteilt.clone();
        let an = self.sitzung.is_some() && g.spur_an.load(Relaxed);
        if an {
            let (datei, kennung) = g.spur_datei.lock().unwrap().clone();
            let schluessel = gemeinsam::spur_schluessel(&kennung, &datei);
            if schluessel != self.spur_schluessel {
                self.spur_schluessel = schluessel.clone();
                self.spur_meldung = "Spur wird geladen …".into();
                // alte Datei nicht zur neuen Startzeit weiterspielen
                if g.spur_ton.lock().unwrap().as_ref().is_some_and(|t| t.schluessel != schluessel) {
                    *g.spur_ton.lock().unwrap() = None;
                }
                let (host, ziel) = (aufnahmen::host(&self.einst.server), self.spur_lade.clone());
                std::thread::spawn(move || {
                    let erg = crate::abspielen::spur_laden(&host, &kennung, &datei);
                    *ziel.lock().unwrap() = Some((schluessel, erg));
                });
            }
        }
        let fertig = self.spur_lade.lock().unwrap().take();
        if let Some((schluessel, erg)) = fertig {
            if schluessel == self.spur_schluessel {
                match erg {
                    Ok((rate, daten)) => {
                        self.spur_dauer = daten.len() as f64 / 2.0 / rate.max(1) as f64;
                        self.spur_meldung.clear();
                        *g.spur_ton.lock().unwrap() = Some(gemeinsam::SpurTon { schluessel, rate, daten: Arc::new(daten) });
                    }
                    Err(e) => {
                        self.spur_meldung = format!("Spur nicht ladbar: {e}");
                        *g.spur_ton.lock().unwrap() = None;
                    }
                }
            }
        }
        let jetzt = (an, g.spur_stand.load(Relaxed));
        if jetzt != self.spur_zuletzt {
            self.spur_zuletzt = jetzt;
            // nach einem Ladefehler beim nächsten Start erneut versuchen
            if self.spur_meldung.starts_with("Spur nicht ladbar") {
                self.spur_meldung.clear();
                self.spur_schluessel.clear();
            }
            if self.au.is_some() {
                self.aufnahmen_zeilen();
            }
        }
    }

    /// Text zur Spur des Raums (None = keine Spur)
    fn spur_status(&self) -> Option<String> {
        if !self.spur_meldung.is_empty() {
            return Some(self.spur_meldung.clone());
        }
        let (datei, an) = self.spur_jetzt();
        if !an {
            return None;
        }
        let g = &self.geteilt;
        let server = uhr_us() as f64 + g.uhr_versatz.load(Relaxed) as f64;
        let pos = (server - g.spur_start.load(Relaxed) as f64) / 1e6;
        let name = aufnahmen::anzeige_name(&datei);
        Some(if pos < 0.0 {
            format!("♪ Für alle: {name} – beginnt in {:.0} s", -pos.floor())
        } else if pos > self.spur_dauer + 0.5 {
            format!("♪ {name} ist zu Ende – „Für alle ▶“ startet sie noch einmal.")
        } else {
            format!("♪ Für alle: {name} – {} / {}", aufnahmen::zeit_text(pos), aufnahmen::zeit_text(self.spur_dauer))
        })
    }

    /// Spur für den ganzen Raum starten bzw. stoppen
    unsafe fn spur_fuer_alle(&mut self, i: usize) {
        let Some(au) = self.au.as_ref() else { return };
        let Some(e) = au.eintraege.get(i).cloned() else { return };
        let (fenster, kennung) = (au.hwnd, au.kennung);
        if self.sitzung.is_none() {
            hinweis(fenster, "Erst verbinden – die Spur läuft für alle, die im Raum sind.");
            return;
        }
        let (datei, an) = self.spur_jetzt();
        let g = self.geteilt.clone();
        // läuft genau diese Spur (und ist nicht zu Ende): stoppen, sonst (neu) starten
        let server = uhr_us() as f64 + g.uhr_versatz.load(Relaxed) as f64;
        let pos = (server - g.spur_start.load(Relaxed) as f64) / 1e6;
        if an && datei == e.datei && pos <= self.spur_dauer + 0.5 {
            *g.spur_wunsch.lock().unwrap() = Some((false, kennung, e.datei, 0));
        } else {
            self.anhoeren_stoppen();
            *g.spur_wunsch.lock().unwrap() = Some((true, kennung, e.datei, 0));
        }
    }

    /// Datei auswählen und als Spur in die Ablage des Raums hochladen
    unsafe fn spur_hochladen(&mut self) {
        let Some(au) = self.au.as_mut() else { return };
        if au.hoch_laeuft {
            return;
        }
        let mut puffer = [0u16; 1024];
        let filter: Vec<u16> = "Audiodateien (MP3, WAV, FLAC, OGG, M4A)\0*.mp3;*.wav;*.flac;*.ogg;*.m4a\0Alle Dateien\0*.*\0\0".encode_utf16().collect();
        let mut ofn = windows::Win32::UI::Controls::Dialogs::OPENFILENAMEW {
            lStructSize: std::mem::size_of::<windows::Win32::UI::Controls::Dialogs::OPENFILENAMEW>() as u32,
            hwndOwner: au.hwnd,
            lpstrFilter: PCWSTR(filter.as_ptr()),
            lpstrFile: windows::core::PWSTR(puffer.as_mut_ptr()),
            nMaxFile: puffer.len() as u32,
            lpstrTitle: w!("Spur hochladen – alle im Raum können sie dann gemeinsam abspielen"),
            Flags: windows::Win32::UI::Controls::Dialogs::OFN_FILEMUSTEXIST | windows::Win32::UI::Controls::Dialogs::OFN_PATHMUSTEXIST,
            ..Default::default()
        };
        if !windows::Win32::UI::Controls::Dialogs::GetOpenFileNameW(&mut ofn).as_bool() {
            return;
        }
        let laenge = puffer.iter().position(|&c| c == 0).unwrap_or(puffer.len());
        let pfad = std::path::PathBuf::from(String::from_utf16_lossy(&puffer[..laenge]));
        let Some(au) = self.au.as_mut() else { return };
        au.hoch_laeuft = true;
        let (host, kennung, ziel, h, st) = (au.host.clone(), au.kennung, au.hoch.clone(), au.hwnd.0 as isize, au.status);
        let name = pfad.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        self.text_setzen(st, &format!("Lade {name} hoch …"));
        std::thread::spawn(move || {
            let erg = crate::abspielen::hochladen_vorbereiten(&pfad).and_then(|(datei, daten)| aufnahmen::hochladen(&host, &kennung, &datei, &daten).map(|_| datei));
            *ziel.lock().unwrap() = Some(erg);
            unsafe {
                let _ = PostMessageW(HWND(h as _), WM_AU_HOCH, WPARAM(0), LPARAM(0));
            }
        });
    }

    /// Aufnahme direkt vom Server anhören bzw. (gleiche Zeile) anhalten
    unsafe fn anhoeren(&mut self, i: usize) {
        let gleich = self.au.as_ref().is_some_and(|a| a.spielt.as_ref().is_some_and(|s| s.0 == i));
        self.anhoeren_stoppen();
        if gleich {
            self.aufnahmen_zeilen();
            return;
        }
        let Some(au) = self.au.as_ref() else { return };
        let Some(e) = au.eintraege.get(i).cloned() else { return };
        // ohne Verbindung eigener (geteilter) Ausgang, sonst mischt Jamcenters Wiedergabe
        let eigener = if self.sitzung.is_some() { None } else { Some(if self.asio() { String::new() } else { self.einst.ausgang.clone() }) };
        let sp = crate::abspielen::Spieler::starten(au.host.clone(), au.kennung, e.datei.clone(), self.geteilt.clone(), eigener);
        if let Some(au) = self.au.as_mut() {
            au.spielt = Some((i, sp));
        }
        self.aufnahmen_zeilen();
        self.anhoeren_takt();
    }

    unsafe fn anhoeren_stoppen(&mut self) {
        if let Some((_, sp)) = self.au.as_mut().and_then(|a| a.spielt.take()) {
            sp.stoppen();
        }
    }

    /// Zeitanzeige und Ende des Anhörens
    unsafe fn anhoeren_takt(&mut self) {
        let Some(au) = self.au.as_ref() else { return };
        let Some((i, sp)) = &au.spielt else {
            // kein lokales Anhören: Zustand der Spur des Raums zeigen
            if let Some(text) = self.spur_status() {
                let st = au.status;
                if self.takt % 4 == 0 {
                    self.text_setzen(st, &text);
                }
            }
            return;
        };
        let st = au.status;
        let fehler = sp.stand.fehler.lock().unwrap().clone();
        let fertig = sp.stand.fertig.load(Relaxed);
        let e = au.eintraege.get(*i).cloned();
        if fehler.is_some() || fertig || e.is_none() {
            self.anhoeren_stoppen();
            self.aufnahmen_zeilen();
            if let Some(f) = fehler {
                self.text_setzen(st, &format!("Anhören nicht möglich: {f}"));
            }
            return;
        }
        let e = e.unwrap();
        let pos = sp.position();
        let text = if pos <= 0.0 {
            format!("▶ {} – lädt …", aufnahmen::anzeige_name(&e.datei))
        } else {
            format!("▶ {} – {} / {}", aufnahmen::anzeige_name(&e.datei), aufnahmen::zeit_text(pos), aufnahmen::zeit_text(aufnahmen::dauer_sek(&e.datei, e.groesse)))
        };
        // nur bei Änderung setzen (sonst flackert der Text)
        if self.takt % 4 == 0 || pos <= 0.0 {
            self.text_setzen(st, &text);
        }
    }

    /// Frequenzanzeige im Equalizer: letzte 4096 Proben nach dem EQ -> Balken je Band
    unsafe fn spektrum_rechnen(&mut self) {
        let g = self.geteilt.clone();
        let a = &g.anzeige;
        let Some(ef) = &mut self.ef else { return };
        let n = gemeinsam::SPEKTRUM_N;
        let pos = a.spektrum_pos.load(Relaxed) as usize;
        let x: Vec<f32> = (0..n).map(|i| a.spektrum[(pos + i) % n].get()).collect();
        let betrag = crate::analyse::fft_betrag(&x);
        let df = a.spektrum_rate.load(Relaxed).max(8000) as f32 / n as f32;
        let nb = ef.balken.len();
        let mut geaendert = false;
        for i in 0..nb {
            let (f0, f1) = spektrum_band(i, nb);
            let (k0, k1) = ((f0 / df).ceil() as usize, (f1 / df).floor() as usize);
            let v = if k1 >= k0 {
                betrag[k0.min(betrag.len() - 1)..=k1.min(betrag.len() - 1)].iter().fold(0.0f32, |m, &b| m.max(b))
            } else {
                // schmales Band unterhalb der FFT-Auflösung: nächster Wert
                betrag[(((f0 + f1) / 2.0 / df).round() as usize).min(betrag.len() - 1)]
            };
            let db = 20.0 * v.max(1e-6).log10();
            // schnell hoch, langsam runter (wie ein Analyzer)
            let alt = ef.balken[i];
            let neu = if db > alt { db } else { (alt - 1.5).max(db) };
            if (neu - alt).abs() > 0.05 {
                geaendert = true;
            }
            ef.balken[i] = neu;
        }
        if geaendert {
            let _ = InvalidateRect(ef.hwnd, Some(&ef.eq_graph), false);
        }
    }


    // ------------------------------------------------------------ Weitere Quellen
    /// Zusatzquellen aus den Einstellungen übernehmen: Kanalnamen fürs Mischpult, Hinweis im Hauptfenster
    unsafe fn zusatz_anwenden(&mut self) {
        let quellen: Vec<crate::zusatz::Quelle> = crate::zusatz::aus_text(&self.einst.zusatz).iter().filter_map(|k| crate::zusatz::beschreiben(k)).collect();
        *self.geteilt.zusatz_namen.lock().unwrap() = crate::zusatz::kanal_namen(&quellen);
        let text = if quellen.is_empty() {
            "zweites Interface, Windows-Ton …".to_string()
        } else {
            let teile: Vec<String> = quellen.iter().map(|q| if q.kanaele > 2 { format!("{} ({} Kan.)", q.name, q.kanaele) } else { q.name.clone() }).collect();
            format!("+ {} – Kanäle im Mischpult", teile.join(", "))
        };
        if let Some(h) = self.c.as_ref().map(|c| c.quellen_info) {
            self.text_setzen(h, &text);
        }
    }

    unsafe fn quellen_oeffnen(&mut self) {
        if let Some(q) = &self.qu {
            let _ = SetForegroundWindow(q.hwnd);
            return;
        }
        self.felder_lesen();
        let liste = crate::zusatz::auswahl(if self.asio() { "" } else { &self.einst.eingang });
        let gewaehlt = crate::zusatz::aus_text(&self.einst.zusatz);
        let hoehe = 64 + liste.len() as i32 * 32 + 8 + 104 + 48;
        let stil = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_CLIPCHILDREN;
        let mut r = RECT { left: 0, top: 0, right: self.p(620), bottom: self.p(hoehe) };
        let _ = AdjustWindowRectEx(&mut r, stil, false, WINDOW_EX_STYLE(0));
        let Ok(fenster) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("JamcenterQuellen"),
            w!("Jamcenter – weitere Tonquellen"),
            stil,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            r.right - r.left,
            r.bottom - r.top,
            self.hwnd,
            HMENU::default(),
            self.hinst,
            None,
        ) else {
            return;
        };
        dunkle_titelleiste(fenster);
        self.label_in(fenster, "WEITERE TONQUELLEN – werden zusätzlich zum Eingang verschickt", (24, 20, 572, 20), C_TEXT2, self.font_titel);
        self.label_in(fenster, "Jede Quelle bekommt eigene Kanäle im Mischpult (Lautstärke, Panorama, Effekte).", (24, 40, 572, 18), C_TEXT2, self.font_klein);
        let mut eintraege = Vec::new();
        for (i, (k, text)) in liste.iter().enumerate() {
            let h = self.schalter_in(fenster, text, (24, 66 + i as i32 * 32, 572, 26), QU_ID + i as i32, gewaehlt.contains(k));
            eintraege.push((k.clone(), h));
        }
        let y = 66 + liste.len() as i32 * 32 + 8;
        self.label_in(
            fenster,
            "„Windows-Ton“ schickt alles, was andere Programme über Windows abspielen (Sequencer, Browser, Player) – ohne den Ton \
             des anderen Raums und ohne Metronom, also ohne Echo. „Ausgang mithören“ nimmt alles von diesem Ausgang, auch Jamcenter selbst: \
             nicht den Ausgang wählen, auf dem Jamcenter spielt.\n\
             Sequencer mit ASIO-Treiber laufen an Windows vorbei: dort einen Windows-Ausgang (WASAPI/MME) einstellen oder den \
             Loopback-Kanal des Interfaces als weiteren Eingang nehmen. Mit „Exklusiver Gerätezugriff“ kann auf dem Jamcenter-Ausgang \
             kein anderes Programm spielen. Loopback-Quellen kommen etwa 10–20 ms später an als das Mikrofon.",
            (24, y, 572, 96),
            C_TEXT2,
            self.font_klein,
        );
        self.knopf_in(fenster, "Fertig", (476, y + 104, 120, 32), ID_QU_FERTIG, K_PRIMAER, self.font_fett);
        let _ = ShowWindow(fenster, SW_SHOW);
        self.qu = Some(QuellenFenster { hwnd: fenster, eintraege, alt: self.einst.zusatz.clone() });
    }

    /// Fenster schließen; bei Änderung übernehmen (laufende Verbindung startet neu)
    unsafe fn quellen_schliessen(&mut self) {
        let Some(q) = self.qu.take() else { return };
        let neu: Vec<String> = q.eintraege.iter().filter(|(_, h)| schalter(*h)).map(|(k, _)| k.clone()).collect();
        let _ = DestroyWindow(q.hwnd);
        let neu = crate::zusatz::zu_text(&neu);
        if neu == q.alt {
            return;
        }
        self.einst.zusatz = neu;
        self.zusatz_anwenden();
        // eigene Mischung je Quellen-Kombination
        self.geteilt.regler.kanaele_vorgabe.store(false, Relaxed);
        self.einst.kanaele_anwenden(&self.geteilt);
        self.einst.speichern();
        // Mischpult zeigt sonst alte Kanäle
        if self.mp.is_some() {
            self.mischpult_schliessen();
        }
        self.neu_starten();
    }

    unsafe fn quellen_nachricht(&mut self, msg: u32, wp: WPARAM) -> Option<LRESULT> {
        match msg {
            WM_COMMAND => {
                if (wp.0 & 0xFFFF) as i32 == ID_QU_FERTIG {
                    self.quellen_schliessen();
                }
                Some(LRESULT(0))
            }
            WM_PAINT => {
                let q = self.qu.as_ref()?;
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(q.hwnd, &mut ps);
                fuellen(hdc, &ps.rcPaint, C_KARTE);
                let _ = EndPaint(q.hwnd, &ps);
                Some(LRESULT(0))
            }
            WM_CLOSE => {
                self.quellen_schliessen();
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    // ------------------------------------------------------------ Anleitung eigener Server
    unsafe fn anleitung_oeffnen(&mut self) {
        if let Some(a) = &self.al {
            let _ = SetForegroundWindow(a.hwnd);
            return;
        }
        let stil = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU;
        let mut r = RECT { left: 0, top: 0, right: self.p(760), bottom: self.p(600) };
        let _ = AdjustWindowRectEx(&mut r, stil, false, WINDOW_EX_STYLE(0));
        let Ok(fenster) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("JamcenterAnleitung"),
            w!("Jamcenter – eigenen Server einrichten"),
            stil,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            r.right - r.left,
            r.bottom - r.top,
            self.hwnd,
            HMENU::default(),
            self.hinst,
            None,
        ) else {
            return;
        };
        dunkle_titelleiste(fenster);
        let text = self.ctl_in(
            fenster,
            w!("EDIT"),
            &crate::anleitung::text(),
            (ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32 | WS_VSCROLL.0,
            WINDOW_EX_STYLE(0),
            (16, 16, 728, 520),
            0,
            self.font,
        );
        let _ = SetWindowTheme(text, w!("DarkMode_Explorer"), PCWSTR::null());
        let fest = CreateFontW(-((9.5 * GetDpiForSystem().max(96) as f32) / 72.0) as i32, 0, 0, 0, 400, 0, 0, 0, 1, 0, 0, 5, 0, w!("Consolas"));
        SendMessageW(text, WM_SETFONT, WPARAM(fest.0 as usize), LPARAM(1));
        FARBEN.with(|m| m.borrow_mut().insert(text.0 as isize, C_TEXT));
        let leer = crate::anleitung::RELAY_LINUX.is_empty();
        self.knopf_in(fenster, if leer { "Dateien speichern (ohne Server-Programme)" } else { "Dateien speichern …" }, (16, 548, 360, 36), ID_AL_SPEICHERN, K_PRIMAER, self.font_fett);
        self.knopf_in(fenster, "Schließen", (624, 548, 120, 36), ID_AL_SCHLIESSEN, 0, self.font);
        let _ = ShowWindow(fenster, SW_SHOW);
        self.al = Some(Anleitung { hwnd: fenster });
    }

    unsafe fn anleitung_nachricht(&mut self, msg: u32, wp: WPARAM) -> Option<LRESULT> {
        let fenster = self.al.as_ref()?.hwnd;
        match msg {
            WM_COMMAND => {
                match (wp.0 & 0xFFFF) as i32 {
                    ID_AL_SPEICHERN => {
                        let titel = wide("Ordner für die Server-Dateien wählen");
                        let bi = BROWSEINFOW {
                            hwndOwner: fenster,
                            lpszTitle: PCWSTR(titel.as_ptr()),
                            ulFlags: BIF_NEWDIALOGSTYLE | BIF_RETURNONLYFSDIRS,
                            ..Default::default()
                        };
                        let pidl = SHBrowseForFolderW(&bi);
                        if !pidl.is_null() {
                            let mut pfad = [0u16; 260];
                            let ok = SHGetPathFromIDListW(pidl, &mut pfad).as_bool();
                            windows::Win32::System::Com::CoTaskMemFree(Some(pidl as _));
                            if ok {
                                let n = pfad.iter().position(|&c| c == 0).unwrap_or(0);
                                let ordner = String::from_utf16_lossy(&pfad[..n]);
                                match crate::anleitung::speichern(std::path::Path::new(&ordner)) {
                                    Ok(liste) => {
                                        hinweis(fenster, &format!("Gespeichert in {ordner}:\n\n{}", liste.join("\n")));
                                        ShellExecuteW(fenster, w!("open"), &HSTRING::from(ordner), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
                                    }
                                    Err(e) => meldung(&e),
                                }
                            }
                        }
                    }
                    ID_AL_SCHLIESSEN => {
                        let _ = DestroyWindow(fenster);
                        self.al = None;
                    }
                    _ => {}
                }
                Some(LRESULT(0))
            }
            WM_CLOSE => {
                let _ = DestroyWindow(fenster);
                self.al = None;
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    // ------------------------------------------------------------ Nachrichten Hauptfenster
    unsafe fn nachricht(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_COMMAND => {
                let id = (wp.0 & 0xFFFF) as i32;
                let code = ((wp.0 >> 16) & 0xFFFF) as u32;
                self.befehl(id, code, HWND(lp.0 as _));
                Some(LRESULT(0))
            }
            WM_HSCROLL => {
                if lp.0 != 0 {
                    self.schieber(HWND(lp.0 as _), (wp.0 & 0xFFFF) as u32);
                }
                Some(LRESULT(0))
            }
            WM_TIMER => {
                self.takt();
                Some(LRESULT(0))
            }
            x if x == WM_APP + 1 => {
                if self.asio() {
                    let c = self.c.as_ref().unwrap();
                    let lade = vec!["ASIO-Treiber wird gelesen …".to_string()];
                    let (e, a) = (c.eingang, c.ausgang);
                    Self::eintraege(e, &lade, 0);
                    Self::eintraege(a, &lade, 0);
                    let _ = UpdateWindow(self.hwnd);
                }
                self.modus_anwenden();
                Some(LRESULT(0))
            }
            WM_PAINT => {
                self.malen();
                Some(LRESULT(0))
            }
            WM_CLOSE => {
                self.felder_lesen();
                if let Some(s) = self.sitzung.take() {
                    s.stoppen();
                }
                self.asio_panel = None;
                self.mischpult_schliessen();
                self.einst.speichern();
                let _ = DestroyWindow(self.hwnd);
                Some(LRESULT(0))
            }
            _ => None,
        }
    }
}

unsafe fn dunkle_titelleiste(h: HWND) {
    let ja: i32 = 1;
    let _ = DwmSetWindowAttribute(h, DWMWA_USE_IMMERSIVE_DARK_MODE, &ja as *const _ as _, 4);
    let farbe: u32 = C_BG;
    let _ = DwmSetWindowAttribute(h, DWMWA_CAPTION_COLOR, &farbe as *const _ as _, 4);
}

extern "system" fn fensterfunktion(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_DESTROY if hwnd.0 as isize == H_HAUPT.load(Relaxed) => {
                PostQuitMessage(0);
                return LRESULT(0);
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
                let hdc = HDC(wp.0 as _);
                let f = FARBEN.with(|m| m.borrow().get(&lp.0).copied()).unwrap_or(C_TEXT);
                SetTextColor(hdc, COLORREF(f));
                SetBkColor(hdc, COLORREF(C_KARTE));
                return LRESULT(pinsel(&PINSEL_KARTE).0 as isize);
            }
            WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => {
                let hdc = HDC(wp.0 as _);
                SetTextColor(hdc, COLORREF(C_TEXT));
                SetBkColor(hdc, COLORREF(C_FELD));
                return LRESULT(pinsel(&PINSEL_FELD).0 as isize);
            }
            WM_DRAWITEM => {
                let d = &*(lp.0 as *const DRAWITEMSTRUCT);
                knopf_zeichnen(d);
                return LRESULT(1);
            }
            WM_NOTIFY => {
                let kopf = &*(lp.0 as *const NMHDR);
                if kopf.code == NM_CUSTOMDRAW {
                    return regler_zeichnen(&*(lp.0 as *const NMCUSTOMDRAW));
                }
            }
            WM_COMMAND => {
                // Kippschalter umschalten, bevor die Fachlogik ihn liest
                let code = ((wp.0 >> 16) & 0xFFFF) as u32;
                let h = HWND(lp.0 as _);
                if code == BN_CLICKED && lp.0 != 0 && art(h) & K_SCHALTER != 0 {
                    schalter_setzen(h, !schalter(h));
                }
            }
            _ => {}
        }
        let r = APP.with(|a| {
            let Ok(mut b) = a.try_borrow_mut() else { return None };
            let app = b.as_mut()?;
            if hwnd == app.hwnd {
                app.nachricht(msg, wp, lp)
            } else if app.mp.as_ref().is_some_and(|m| m.hwnd == hwnd) {
                app.mischpult_nachricht(msg, wp, lp)
            } else if app.ef.as_ref().is_some_and(|m| m.hwnd == hwnd) {
                app.effekte_nachricht(msg, wp, lp)
            } else if app.qu.as_ref().is_some_and(|m| m.hwnd == hwnd) {
                app.quellen_nachricht(msg, wp)
            } else if app.au.as_ref().is_some_and(|m| m.hwnd == hwnd) {
                app.aufnahmen_nachricht(msg, wp, lp)
            } else if app.al.as_ref().is_some_and(|m| m.hwnd == hwnd) {
                app.anleitung_nachricht(msg, wp)
            } else {
                None
            }
        });
        r.unwrap_or_else(|| DefWindowProcW(hwnd, msg, wp, lp))
    }
}

pub fn ausfuehren() {
    unsafe {
        let _com = audio::Com::sta();
        let icc = INITCOMMONCONTROLSEX { dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32, dwICC: ICC_BAR_CLASSES | ICC_STANDARD_CLASSES };
        let _ = InitCommonControlsEx(&icc);
        PINSEL_BG.store(CreateSolidBrush(COLORREF(C_BG)).0 as isize, Relaxed);
        PINSEL_KARTE.store(CreateSolidBrush(COLORREF(C_KARTE)).0 as isize, Relaxed);
        PINSEL_FELD.store(CreateSolidBrush(COLORREF(C_FELD)).0 as isize, Relaxed);
        let hinst: HINSTANCE = GetModuleHandleW(None).map(|h| h.into()).unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(fensterfunktion),
            hInstance: hinst,
            hIcon: LoadIconW(hinst, PCWSTR(1 as _)).or_else(|_| LoadIconW(HINSTANCE::default(), IDI_APPLICATION)).unwrap_or_default(),
            // kleines Symbol (Titelleiste) in passender Größe statt herunterskaliert
            hIconSm: HICON(
                LoadImageW(hinst, PCWSTR(1 as _), IMAGE_ICON, GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON), LR_DEFAULTCOLOR)
                    .unwrap_or_default()
                    .0,
            ),
            hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
            hbrBackground: pinsel(&PINSEL_BG),
            lpszClassName: w!("JamcenterFenster"),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        RegisterClassExW(&WNDCLASSEXW { lpszClassName: w!("JamcenterMischpult"), ..wc });
        RegisterClassExW(&WNDCLASSEXW { lpszClassName: w!("JamcenterAnleitung"), ..wc });
        RegisterClassExW(&WNDCLASSEXW { lpszClassName: w!("JamcenterEffekte"), ..wc });
        RegisterClassExW(&WNDCLASSEXW { lpszClassName: w!("JamcenterAufnahmen"), ..wc });
        RegisterClassExW(&WNDCLASSEXW { lpszClassName: w!("JamcenterQuellen"), ..wc });
        let dpi = GetDpiForSystem().max(96);
        let s = dpi as f32 / 96.0;
        SKALA.store(s.to_bits(), Relaxed);
        let stil = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;
        let mut r = RECT { left: 0, top: 0, right: (920.0 * s) as i32, bottom: (604.0 * s) as i32 };
        let _ = AdjustWindowRectEx(&mut r, stil, false, WINDOW_EX_STYLE(0));
        let titel = "Jamcenter by .audiosport";
        let hwnd = CreateWindowExW(
            WS_EX_CONTROLPARENT,
            w!("JamcenterFenster"),
            &HSTRING::from(titel),
            stil,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            r.right - r.left,
            r.bottom - r.top,
            HWND::default(),
            HMENU::default(),
            hinst,
            None,
        )
        .expect("Fenster");
        H_HAUPT.store(hwnd.0 as isize, Relaxed);
        dunkle_titelleiste(hwnd);
        let schrift = |pt: f32, gewicht: i32, name: PCWSTR| CreateFontW(-((pt * dpi as f32) / 72.0) as i32, 0, 0, 0, gewicht, 0, 0, 0, 1, 0, 0, 5, 0, name);
        let geteilt = Arc::new(Geteilt::neu());
        let einst = Einstellungen::laden();
        sitzung::regler_setzen(&einst, &geteilt);
        einst.kanaele_anwenden(&geteilt);
        let mut app = App {
            hwnd,
            s,
            hinst,
            font: schrift(9.5, 400, w!("Segoe UI")),
            font_fett: schrift(10.0, 600, w!("Segoe UI Semibold")),
            font_gross: schrift(20.0, 600, w!("Segoe UI Semibold")),
            font_klein: schrift(8.0, 400, w!("Segoe UI")),
            font_logo: schrift(19.0, 900, w!("Segoe UI Black")),
            font_titel: schrift(8.0, 700, w!("Segoe UI")),
            c: None,
            karten: Vec::new(),
            geteilt,
            einst,
            sitzung: None,
            ein_liste: Vec::new(),
            aus_liste: Vec::new(),
            asio_liste: Vec::new(),
            asio_ein: Vec::new(),
            asio_aus: Vec::new(),
            asio_bloecke: None,
            asio_panel: None,
            meter_ein: RECT::default(),
            meter_aus: RECT::default(),
            anz_ein: 0.0,
            anz_aus: 0.0,
            texte: HashMap::new(),
            takt: 0,
            rk_alt: 0,
            rk_bis: None,
            m_punkte: RECT::default(),
            m_schlag: -1,
            m_takt_anz: 4,
            m_stand: u32::MAX,
            taps: Vec::new(),
            mp: None,
            ef: None,
            au: None,
            qu: None,
            spur_schluessel: String::new(),
            spur_lade: Arc::new(Mutex::new(None)),
            spur_meldung: String::new(),
            spur_dauer: 0.0,
            spur_zuletzt: (false, 0),
            aufn_war_an: false,
            bg: HBITMAP::default(),
            al: None,
            logo: HICON(LoadImageW(hinst, PCWSTR(1 as _), IMAGE_ICON, (40.0 * s) as i32, (40.0 * s) as i32, LR_DEFAULTCOLOR).unwrap_or_default().0),
        };
        app.aufbauen();
        app.zusatz_anwenden();
        APP.with(|a| *a.borrow_mut() = Some(app));
        SetTimer(hwnd, 1, 40, None);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, HWND::default(), 0, 0).as_bool() {
            let ziel = GetAncestor(msg.hwnd, GA_ROOT);
            if !IsDialogMessageW(if ziel.0.is_null() { hwnd } else { ziel }, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        APP.with(|a| {
            if let Some(mut app) = a.borrow_mut().take() {
                if let Some(s) = app.sitzung.take() {
                    s.stoppen();
                }
            }
        });
    }
}
