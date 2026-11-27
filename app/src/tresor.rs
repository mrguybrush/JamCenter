//! Raumpasswort in den Einstellungen mit Windows-DPAPI schützen (nur dieses Benutzerkonto kann es lesen).

use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

pub fn verpacken(klartext: &str) -> String {
    if klartext.is_empty() {
        return String::new();
    }
    unsafe {
        let ein = CRYPT_INTEGER_BLOB { cbData: klartext.len() as u32, pbData: klartext.as_ptr() as *mut u8 };
        let mut aus = CRYPT_INTEGER_BLOB::default();
        if CryptProtectData(&ein, windows::core::PCWSTR::null(), None, None, None, 0, &mut aus).is_err() {
            return String::new();
        }
        let s = hex(std::slice::from_raw_parts(aus.pbData, aus.cbData as usize));
        let _ = LocalFree(HLOCAL(aus.pbData as _));
        format!("dpapi:{s}")
    }
}

pub fn entpacken(text: &str) -> Option<String> {
    let daten = unhex(text.strip_prefix("dpapi:")?)?;
    unsafe {
        let ein = CRYPT_INTEGER_BLOB { cbData: daten.len() as u32, pbData: daten.as_ptr() as *mut u8 };
        let mut aus = CRYPT_INTEGER_BLOB::default();
        CryptUnprotectData(&ein, None, None, None, None, 0, &mut aus).ok()?;
        let s = String::from_utf8_lossy(std::slice::from_raw_parts(aus.pbData, aus.cbData as usize)).into_owned();
        let _ = LocalFree(HLOCAL(aus.pbData as _));
        Some(s)
    }
}
