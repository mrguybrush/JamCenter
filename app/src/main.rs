//! Jamcenter – Ton zwischen zwei Räumen mit möglichst geringer Latenz.
#![windows_subsystem = "windows"]

mod abspielen;
mod analyse;
mod aufnahmen;
mod anleitung;
mod asio;
mod audio;
mod gemeinsam;
mod gui;
mod mischer;
mod netz;
mod puffer;
mod sitzung;
mod test;
mod tresor;
mod zusatz;

fn main() {
    std::panic::set_hook(Box::new(|info| {
        let text = format!("Unerwarteter Fehler:\n{info}");
        eprintln!("{text}");
        gui::meldung(&text);
    }));
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("--diagnose") => {
            if std::env::var("KB_NUR_ASIO").is_err() {
                let t = audio::diagnose();
                println!("{t}");
            }
            let g = std::sync::Arc::new(gemeinsam::Geteilt::neu());
            g.anzeige_zuruecksetzen();
            println!("{}", asio::diagnose(g));
        }
        Some("--test") => test::laufen(&args[2..]),
        // --quelle <system|aus:ID|ein:ID|liste> [sek]: Zusatzquelle prüfen (Pegel je Sekunde)
        Some("--quelle") => zusatz::probe(args.get(2).map(|s| s.as_str()).unwrap_or("system"), args.get(3).and_then(|s| s.parse().ok()).unwrap_or(5)),
        // --takt <Eingangs-ID> [exklusiv 0/1] [sek]: tatsächlich gelieferte Frames je Sekunde messen
        // --anhoeren <server> <raum> <datei> [passwort]: Aufnahme vom Server abspielen (Pegel je Sekunde)
        Some("--anhoeren") => abspielen::probe(&args[2..]),
        // --hochladen <server> <raum> <datei> [passwort]: Spur hochladen
        Some("--hochladen") => abspielen::hochladen_probe(&args[2..]),
        Some("--takt") => audio::takt_messen(args.get(2).map(|s| s.as_str()).unwrap_or(""), args.get(3).is_some_and(|s| s == "1"), args.get(4).and_then(|s| s.parse().ok()).unwrap_or(5)),
        _ => gui::ausfuehren(),
    }
}
