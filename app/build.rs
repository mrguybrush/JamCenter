use embed_manifest::manifest::DpiAwareness;
use embed_manifest::{embed_manifest, new_manifest};
use std::path::PathBuf;

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        // Common Controls 6 (moderne Optik), systemweite DPI-Skalierung, UTF-8
        embed_manifest(new_manifest("Jamcenter").dpi_awareness(DpiAwareness::System)).expect("Manifest");
    }
    // Server-Programme für die Anleitung einbetten (vorher bauen: hochladen.ps1 bzw.
    // cargo build --release -p jamcenter-relay [--target x86_64-unknown-linux-musl]).
    // Fehlen sie, enthält die App eine leere Datei und speichert sie nicht mit.
    let wurzel = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("..");
    let aus = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    for (quelle, ziel) in [
        ("target/x86_64-unknown-linux-musl/release/jamcenter-relay", "relay-linux"),
        ("target/release/jamcenter-relay.exe", "relay-windows.exe"),
    ] {
        let q = wurzel.join(quelle);
        println!("cargo:rerun-if-changed={}", q.display());
        let daten = std::fs::read(&q).unwrap_or_default();
        std::fs::write(aus.join(ziel), daten).expect("OUT_DIR");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
