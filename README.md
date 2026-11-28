<p align="center"><img src="assets/icon.png" width="128" alt="MultiAudio"></p>

<h1 align="center">MultiAudio</h1>

<p align="center"><i>L'audio del computer su più uscite insieme, sincronizzate tra loro e col video. Senza driver.</i></p>

> **English:** MultiAudio plays your computer's sound on several outputs at once (Bluetooth, AirPlay, wired speakers, HDMI…), each with its own delay, in sync with each other and with video, DRM-protected video included. No virtual audio driver needed. The UI is in Italian.

| Sistema | Stato |
|---|---|
| macOS 14.2+ | ✅ pronto |
| Linux (PipeWire / PulseAudio) | 🚧 in sviluppo |
| Windows | 🚧 in sviluppo |

## Cosa fa

- Manda l'audio a **più uscite contemporaneamente**, ognuna con il **suo ritardo**.
- Su Mac crea l'uscita **«MultiAudio»** (6 canali, 5.1) e la rende l'uscita di sistema finché è acceso; spento, torna quella di prima. Se l'uscita del video è AirPlay (o Bluetooth senza surround), usa direttamente quella.
- **Video sincronizzato**, anche Netflix, Disney+, Prime: i player ritardano l'immagine da soli.
- **Correzione della deriva di clock** tra i dispositivi.
- **Calibrazione col microfono** (ritardi e volume delle casse) e click di test.
- **Surround**: casse dietro e cassa centrale facoltative, con il 5.1 vero dei film; lo stereo resta com'è (o, a scelta, con un effetto ambiente dietro).
- **Bassi**: per ogni uscita scegli quanti ne suona (tutti, pochi, niente); il resto va alle casse davanti che li suonano tutti.
- **Tasti del volume** su tutte le uscite, con indicatore a schermo.

## Uso

1. Accendi le uscite che vuoi sentire.
2. Accendi MultiAudio con l'interruttore in alto.
3. Premi **🎯 Calibra col microfono**, poi verifica col **🔊 Click di test**: devi sentire un solo click.

Su Mac servono i permessi **Registrazione audio di sistema**, **Microfono** (calibrazione) e **Accessibilità** (tasti del volume): l'app li chiede da sola.

## Compilazione

Serve [Rust](https://rustup.rs).

**macOS** (crea `dist/MultiAudio.app`):

```bash
./packaging/macos/make-app.sh
```

**Linux** (Debian/Ubuntu):

```bash
sudo apt install libasound2-dev libpulse-dev libxkbcommon-dev pulseaudio-utils
```

```bash
cargo build --release
```

**Windows**:

```bash
cargo build --release
```

## Riga di comando

```bash
multiaudio --list          # elenca i dispositivi
multiaudio --headless      # esegue il config salvato senza finestra
multiaudio --cleanup       # dopo un crash: toglie l'uscita «MultiAudio» e rimette quella di prima
```

## Licenza

GPL-3.0: vedi [LICENSE](LICENSE).
