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

### Linux

Come su Mac, acceso rende **«MultiAudio»** l'uscita di sistema e suona tutte le uscite, ognuna con posizione e ritardo. Perché i player ritardino il video, l'uscita deve dichiarare la sua latenza: un'uscita virtuale di PipeWire non può farlo, la porta di una scheda audio sì. Per questo «MultiAudio» è la scheda loopback del kernel (`snd-aloop`), da preparare una volta:

```bash
echo snd-aloop | sudo tee /etc/modules-load.d/snd-aloop.conf
echo "options snd-aloop pcm_substreams=1" | sudo tee /etc/modprobe.d/snd-aloop.conf
sudo modprobe snd-aloop
```

e la regola di WirePlumber che le dà il nome che MultiAudio cerca, in `~/.config/wireplumber/wireplumber.conf.d/60-multiaudio-loopback.conf`:

```
monitor.alsa.rules = [
  { matches = [ { device.name = "~alsa_card.platform-snd_aloop.*" } ]
    actions = { update-props = { device.description = "MultiAudio" } } }
  { matches = [ { node.name = "~alsa_output.platform-snd_aloop.*" } ]
    actions = { update-props = { node.name = "multiaudio_51", node.description = "MultiAudio",
                                 priority.session = 1, priority.driver = 1,
                                 session.suspend-timeout-seconds = 0 } } }
  { matches = [ { node.name = "~alsa_input.platform-snd_aloop.*" } ]
    actions = { update-props = { node.disabled = true } } }
]
```

poi `systemctl --user restart wireplumber`. Senza, «MultiAudio» c'è lo stesso ma non dichiara la latenza: il video può andare avanti all'audio. La latenza delle casse Bluetooth è quella che dichiarano a BlueZ; le uscite col filo partono da circa 20 ms: la calibrazione col microfono misura quella vera.

Su Linux MultiAudio sta anche nella **barra di sistema** (icona con menu: acceso/spento, uscite, apri, avvio all'accesso, esci); chiudere la finestra non lo spegne.

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
