# JamCenter – Übergabe für weitere Agenten

Repo: github.com/mrguybrush/JamCenter (öffentlich), Zweig `main`, lokal `E:\ClaudeWorkdir\JamCenter`.

**Arbeitsweise:** Chat auf Deutsch, kurz. Alles, was Latenz kosten könnte, erst mit dem Nutzer besprechen. Ursache vor Fix.
Keine Doku-Dateien ungefragt. Nach Änderungen committen und pushen.

Werkzeug fürs gemeinsame Musizieren an zwei Orten mit minimaler Latenz, 1:1 (Server lässt 2 je Raum zu).
Rust-Workspace, Toolchain `stable-x86_64-pc-windows-gnu` (`rust-toolchain.toml`; kein Visual Studio – deshalb **windows 0.58**:
ab 0.61 braucht der GNU-Linker dlltool+as). Bauen + Einspielen: `hochladen.ps1` (baut Relay für Linux/musl und Windows ZUERST,
dann die App – sie bettet beide für die Anleitung ein, `app/build.rs`), `-NurApp`/`-NurRelay`; Ziel (SSH, Download-Pfad/-URL) in
`ziel.local.ps1` (nicht in Git). Tests: `cargo test --release`. Kein Server im Code: `protokoll::STANDARD_SERVER` kommt beim Bauen aus `JAMCENTER_SERVER` (`$StandardServer` in ziel.local.ps1), sonst leer
(dann muss man im Feld „Server“ etwas eintragen). Repo ist öffentlich – keine Server-Adressen, Zugänge oder Schulbezüge einchecken.
Einstellungen `%APPDATA%\Jamcenter\` (`einstellungen.txt`, Passwort per DPAPI; alte `Klangbruecke`-Datei wird einmal übernommen;
`presets.txt` = Mix-Presets). Schlüssel-/Raum-Salze heißen bewusst weiter „Klangbruecke-…“ (Kompatibilität) – nicht umbenennen.

- `relay/` = Server: Dienst `jamcenter-relay` (SCHED_FIFO, DynamicUser, löst `klangbruecke-relay` ab), UDP **3478, 443, 47800** (ufw offen).
  Treffpunkt, Weiterleitung, Serveruhr und Metronom je Raum; Adressnachweis per Keks; Log `journalctl -u jamcenter-relay`.
  `server/installieren.sh` + `jamcenter-relay.service` = Einrichtung auf fremden Linux-Servern (über „Eigener Server …“ in der App speicherbar).
- `app/` = Windows-Client (Win32 ohne GPU, schwarzes Design, Akzent Rot #EF3246, „by .audiosport“ in Kopf/Titel/Anleitung, Logo `app/jamcenter.ico` (Quelle `app/logo.png`) setzt `icon_setzen.ps1` per UpdateResource in die exe (kein rc/windres in der GNU-Toolchain); runde Formen per 4-fach-Überabtastung + HALFTONE geglättet (`glatt()`), Auffrischen zeichnet nur Anzeigen; Knöpfe/Kippschalter BS_OWNERDRAW mit Art in GWLP_USERDATA,
  Regler per NM_CUSTOMDRAW, Comboboxen/Edits „DarkMode_CFD“, dunkle Titelleiste per DWM). WASAPI exklusiv → IAudioClient3 → geteilt, RAW,
  MMCSS; **ASIO** ohne SDK über die IASIO-vtable (`asio.rs`, STA-Thread mit eigenem Fenster, alle Eingänge, Ausgangspaar, Blockgröße, Panel,
  Überwachung „keine Rückrufe“). Mit ASIO4ALL auf dem Entwicklungs-PC streamt der Treiber nicht (auch Jamulus nicht) – ASIO-Ton ungeprüft.
- Übertragung PCM 16 Bit **Mono oder Stereo** (Codec 0/1, Stereo-Pakete ≤ 160 Frames wegen MTU), optional Kopie des Vorgängers,
  Ende-zu-Ende ChaCha20-Poly1305 (0,003 ms/Paket). Empfangsring immer Stereo-Paare (`puffer.rs`, CH = 2). P2P per Lochstanzen, Wegwahl nach RTT.
  Jitter-Puffer: Ziel = Block + Paket + gemessener Jitter + 0,5 ms (+1 ms je Aussetzer); PI-geregelter kubischer Resampler; Regler „von Hand“ =
  Reserve über dem Minimum.
- **Mischpult** (`mischer.rs`, Fenster „Mischpult“): je Eingang An, Fader, Panorama (nur Stereo), Knopf „FX“ (rot = Effekt an) öffnet das
  **Effektfenster** (2026-11-27, Vorbild: Songcenter-Effekte auf audiosport.info/songcenter, `multitrack.js`): Trittschallfilter 90 Hz,
  5-Band-EQ (Lowshelf/3 Glocken/Highshelf wie Web Audio, Graph mit ziehbaren Punkten, Mausrad = Q), Kompressor mit Songcenter-Kennlinie
  (Threshold/Ratio/Attack/Release/Knee/Make-up, Kennlinie + In/GR/Out, OHNE Vorausschau), Hall je Kanal als FDN (Anteil/Größe/Abklingen,
  Nachhallzeit = Größe·(1−0,001^(1/Abklingen)), trocken = 1−Anteil) – alles ohne Zusatzlatenz, gemischt VOR dem Verschicken. Modell
  `KanalFx` in gemeinsam.rs, 21 `KANAL_VORLAGEN` (Stimmen, Chor, Klavier, Gitarren, Bass, Schlagzeug/Bassdrum/Snare/Cajón, Streicher, Holz, Blech …).
  Mix-Text „stereo~0~n~an=..,db=..,pan=..,lc=..,eq=..,b0=f/g/q,…,k=..,th=..,…,h=..,hm=..,hg=..,hd=..;…“ (`mix_text`/`mix_anwenden`, alte Formate
  werden gelesen): Presets sind unabhängig vom Gerät, überzählige Kanäle werden verworfen, fehlende bleiben aus; je Gerät zusätzlich in `kanaele` gemerkt.
- **Metronom**: Server = Uhr/Zustand je Raum (`T_METRONOM`, Wechsel am nächsten Taktanfang), Uhrabgleich wie NTP; Klick erzeugt jede App
  selbst (`KlickGeber` mit kleinem Synth: Klick, Glocke, Holzblock, Rimshot, Kuhglocke, Bassdrum/Snare, Hi-Hat), sodass er in beiden Räumen
  zur selben Serverzeit erklingt (lokal Ø < 0,1 ms). Tap-Tempo, Klang und Lautstärke nur lokal.
- Kommandozeile: `--diagnose`, `--test <raum> <name> [server[:port]] [sek] [passwort]` (+ `KB_METRONOM=120,3`, `KB_NUR_KLICK=1`, `KB_STEREO=1`).
- **Aufnahme** (1.3.0, 2026-11-27): Knopf „● Aufnahme“ (Statuskarte) startet/stoppt für den ganzen Raum (`T_AUFNAHME`, Name = Ortszeit
  `2026_12_01_1538` der startenden App, Stand trägt `Aufnahme`). Jede App schickt währenddessen ihre Sendemischung zusätzlich im Klartext an den
  Server (`T_AUFNAHME_TON`, Serverzeit über Probenzähler, `Sender::aufnahme_senden`), der Relay legt die Spuren in `relay/src/aufnahme.rs` zeitrichtig
  übereinander (Lücken = Stille), mischt nach dem Stopp zu 48-kHz-Stereo und wandelt mit `lame` (über `chrt -o 0 nice`) in MP3. Alles Schreiben/Mischen
  in Threads mit SCHED_OTHER (`normale_prioritaet()`), die UDP-Threads reichen nur per `try_send` weiter. Ablage `/var/lib/jamcenter/aufnahmen/<Kennung>/`
  (StateDirectory, Kennung = SHA-256 „Jamcenter-Aufnahme|“ + Schlüssel aus Raum+Passwort), Löschen nach 60 Tagen. Liste/Download/Löschen per TCP 47800
  (`JAMCENTER 1 LISTE|HOLE|LOESCHEN`), App: Fenster „Aufnahmen …“ (`app/src/aufnahmen.rs`), Download in den Ordner „Downloads“. Test: `KB_AUFNAHME=1 jamcenter --test …`.
  Schalter „mit Metronom“ (1.3.1, Einstellung `aufnahme_metronom`): der Server erzeugt die Klicks selbst aus dem Metronom-Verlauf (`metronom_schlaege()`,
  Klang = `protokoll::klang`, gemeinsam mit der App), Tempo im Namen `…_120bpm`. Relay protokolliert je Spur Pakete/vor Beginn/zu spät/Lücken.
  1.5.0 (2026-11-27): Relay hängt fortlaufende Pakete (seq+1) IMMER lückenlos an (bis ±400 ms Abweichung) – vorher wurden ab 30 ms
  Abweichung Pakete verworfen (beim Nutzer 20–28 % „zu spät“ = Knistern, Ursache der Zeitstempel-Schwankung nicht nachgestellt). Takt/Versatz je Spur
  per Ausgleichsgerade über Stichproben (`taktausgleich()`, Steigung nur bis ±2 %), im Log „Takt/Versatz“. **Anhören** ohne Download: ▶ je Zeile im
  Aufnahmen-Fenster (`app/src/abspielen.rs`, TCP-Strom `aufnahmen::strom()`, Dekoder symphonia mp3/wav, Ring `Geteilt::vorhoeren`, gemischt in
  `Wiedergabe::fuellen` bzw. ohne Verbindung eigener geteilter Ausgang; Lautstärke = Ausgangsregler, nur lokal). Prüfen: `jamcenter --anhoeren <server> <raum> <datei>`,
  `--takt <Eingangs-ID> [1=exklusiv] [sek]` (gelieferte Frames/s), `--test` mit `KB_EINGANG=<id>` (+`KB_EXKLUSIV=1`) = echtes Mikrofon.
- **Spur für den ganzen Raum** (1.6.0, 2026-11-27): Audiodatei aus der Ablage des Raums (hochladen per „Spur hochladen …“ im Aufnahmen-Fenster,
  TCP `JAMCENTER 1 HOCH <kennung> <datei> <länge>` + Daten, max. 300 MB; MP3 bleibt, WAV/FLAC/OGG/M4A wandelt die App in 16-Bit-WAV,
  `abspielen::hochladen_vorbereiten()`). „Für alle ▶“ = `T_SPUR`-Wunsch, Server setzt `Spur {an, kennung, datei, start_us = jetzt + 2 s, stand}`
  im `Stand` (gleicher Wunsch innerhalb 3,5 s = Wiederholung, später = Neustart). Jede App lädt die Datei ganz (`abspielen::spur_laden()`, PCM16 im
  Speicher, `Geteilt::spur_ton`) und spielt sie in `Wiedergabe::fuellen` über `SpurGeber` so, dass Position 0 zur Serverzeit `start_us` aus dem
  Lautsprecher kommt (wie das Metronom; Nachlade-Verspätete steigen an der richtigen Stelle ein, Gleichlauf ±0,2 % Tempo). Nur lokal, nicht
  verschickt, kein Puffer im Echtzeitweg. Lautstärke `spur_db` (Regler im Aufnahmen-Fenster). Läuft die Spur während einer Aufnahme, mischt der
  Server sie zeitrichtig mit −3 dB hinein (`spuren_vorbereiten()`, MP3 per `lame --decode`). Prüfen: `jamcenter --hochladen <server> <raum> <datei>`.
- **Zweite Soundkarte** (1.6.0): „Weiterer Eingang“ öffnet wie der Haupteingang exklusiv (folgt dem Exklusiv-Schalter), Ring-Ziel = eine Quell-Periode
  + ein Block des Haupteingangs + 2 ms (gemessen Webcam 3 ms: Ø ~5 ms Vorrat), `--quelle` zeigt den Vorrat.
- Metronom (1.3.0): Server ändert bei gleichem Wunsch nichts mehr (sonst Neustart = Lücke bis zum nächsten Takt); `KlickGeber` merkt die Serverzeit des
  letzten Schlags, holt bis 40 ms verpasste Schläge nach, glättet Uhrabgleich-Sprünge und spielt das alte Raster bis zum Beginn des neuen. Bassdrum/Snare je
  Taktart (`bassdrum()`). Equalizer zeigt eine Frequenzanzeige (FFT 4096 nach dem EQ, `spektrum_rechnen()`, Ring `anzeige.spektrum`).
- Oberfläche (1.2.0, 2026-11-27): einheitlicher Eckenradius `radius()` (8 px × DPI) für Knöpfe, Eingabefelder (`feld_proc`: Rand im Nicht-Client-Bereich) und Auswahllisten (`auswahl_proc`, Subclass); Regler und Anzeigen zeichnen gepuffert (`gepuffert()`), Fenster mit WS_CLIPCHILDREN. Puffer voreingestellt fest 2 ms (alte Einstellungen einmalig umgestellt, Kennung `puffer_vorgabe`). Anleitung/Oberfläche ohne Schulbezug.
- **Weitere Tonquellen** (1.4.0, 2026-11-27, `app/src/zusatz.rs`, Knopf „Weitere Quellen …“ in der Audio-Karte, Einstellung `zusatz` = „system;aus:<id>;ein:<id>“):
  `system` = Prozess-Loopback ohne Jamcenter (`audio::oeffnen_system_ohne_mich()`, ActivateAudioInterfaceAsync + eigener COM-Rückruf per `#[implement]`,
  windows-Feature `implement` + Crate windows-core; kennt kein GetCurrentPadding/Mischformat), `aus:` = Loopback eines Ausgangs, `ein:` = weiteres Aufnahmegerät.
  Je Quelle eigener Thread + Ring, `zusatz::Leser` gleicht den Takt per PI-geregeltem Hermite-Resampler an (Ziel 2 Perioden + 3 ms), der Sender mischt sie in
  `senden_aus_mischer()` als Kanäle hinter dem Haupteingang (`anzeige.haupt_kanaele`, Namen/Panorama `zusatz_namen`, Mischung je Kombination über `kanal_schluessel`).
  Prüfen: `jamcenter --quelle liste|system|aus:<id>|ein:<id> [sek]` (Ausgabe nur mit umgeleiteter Standardausgabe).
- Stolperstein (2026-11-27): `DrawTextW` nie mit leerem Text aufrufen (leerer Vec = ungültiger Zeiger → seltener Absturz 0xC000041D in USER32).
- Gemessen: Trust-Webcam exklusiv 3 ms Periode; Heimanschluss (Kabel) ↔ Server ~28 ms RTT; 1 ms gesamt physikalisch unmöglich
  (dem Nutzer erklärt), mit Webcam ≈ 19 ms + Netz. Nächste Ideen: Opus-Option für schwache Leitungen, Effekte auf dem Empfangsweg.
