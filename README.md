# Aperture

Private, low-latency remote desktop for Windows. The host is a single Rust
binary; the viewer is any modern browser — including a phone. No account, no
cloud relay, no install on the client.

Measured on a 2560×1440 display over Wi-Fi/LAN: **12–14 ms capture-to-display**
with the NVIDIA hardware encoder, ~55 fps, ~6 Mb/s.

## Why it is fast

```
Windows Graphics Capture ──BGRA──▶ NV12 downscale (rayon)
      ──▶ H.264 hardware MFT (Media Foundation) ──▶ WebSocket over TLS
      ──▶ browser WebCodecs VideoDecoder ──▶ canvas (desynchronized)
```

* Capture runs free-threaded with the cursor embedded; unchanged frames are
  skipped so the GPU encoder idles instead of burning cycles.
* The H.264 MFT runs in low-latency mode: CBR, Main profile, 1 s GOP, no
  B-frames, and input is only pushed when the async encoder asks for it.
* Frames travel through a single-slot "latest frame wins" channel, so the
  encoder never queues stale pictures. Slow clients get a keyframe instead of a
  backlog.
* The viewer decodes in hardware (`optimizeForLatency`) and measures true
  glass-to-glass latency from the capture timestamp in each frame header.

## Requirements

* Windows 10 1809+ or Windows 11 (Graphics Capture API).
* Rust stable (edition 2024). `cargo build --release` — no external toolchain.
* Optional hardware H.264 (NVIDIA/Intel/AMD). Without it the Microsoft software
  encoder is used automatically.
* Client: Chrome/Edge 94+, Safari 16.4+, Firefox 130+ — anything with WebCodecs.
  Phones must use HTTPS.

## Build and run

```powershell
cargo build --release
.\target\release\aperture.exe
```

The agent prints a desktop link, a phone link (LAN IP), a QR code, and the
active encoder. Open the desktop link locally, or scan the QR with your phone.

### Flags

| Flag | Default | Purpose |
| --- | --- | --- |
| `--port` | `8443` | HTTPS listening port |
| `--bind` | `0.0.0.0` | Bind address |
| `--fps` | `60` | Capture/encode frame-rate cap |
| `--bitrate` | `16000000` | Target bitrate (bits/s) |
| `--max-width`, `--max-height` | `2560`, `1440` | Encoded size cap, never upscales |
| `--monitor` | `0` | Monitor index to stream at startup |
| `--http` | off | Plain HTTP (localhost only — phones need HTTPS) |
| `--token` | stored | Fixed access token instead of `%LOCALAPPDATA%\aperture\token.txt` |
| `--no-hardware` | off | Force the software encoder |
| `--no-qr` | off | Skip the QR code |
| `--relay` | off | Public URL of an `aperture-relay` — enables the friend (WebRTC) link |

## Sharing with friends (WebRTC, no VPN)

Friends open one link, the page loads in their browser, and a peer connection
is negotiated straight between their browser and your PC. They install nothing.
A tiny public **relay** handles only the introduction (SDP/ICE signaling) and
serves the viewer page; the video itself never passes through it.

### Option A — Cloudflare Workers (free, no server to babysit)

`cloudflare/` is that same relay as a Worker: a stateless entry point serving the
viewer plus one Durable Object per room holding the agent socket and the guest
sockets. Cloudflare gives you TLS, a `*.workers.dev` hostname and anycast for
free, and the object hibernates between signaling messages, so an idle room
costs no compute.

```powershell
cd cloudflare
npm install
npm run deploy        # first run opens a browser — sign in and click Allow
```

`npm run deploy` copies `viewer/` into `cloudflare/public/` (Cloudflare serves
static files out of a plain directory) and uploads it. Take the URL it prints and
start the agent with it:

```powershell
.\target\release\aperture.exe --relay https://aperture-relay.<your-account>.workers.dev
```

The banner now prints a **share** link and a second QR code:
`https://…workers.dev/r/<room>?t=<token>`. The room id comes from your token, so
the link survives restarts. Send it — a friend taps it, Safari/Chrome opens, and
they connect by themselves. Several friends can share one link at a time.

| command | what it does |
| --- | --- |
| `npm run dev` | run the identical relay locally on `http://127.0.0.1:8787` |
| `npm test` | 27-check signaling contract test against a running relay |
| `npm run check` | re-sync assets, regenerate `worker-configuration.d.ts`, type-check |
| `npm run deploy` | publish — re-run after any change in `viewer/` or `cloudflare/src` |

Free-plan numbers that matter: 100 000 Durable Object requests a day (one room
uses a few hundred) and hibernated sockets are not billed for duration. Pushing a
new deploy disconnects live sessions; both sides reconnect on their own.

The signaling socket sends a one-word keepalive every 25 s, which Cloudflare
answers by itself — that is what keeps a quiet peer connection alive.

TURN (optional, `wrangler secret put TURN_CREDENTIAL` plus `TURN_URL` and
`TURN_USERNAME` in `wrangler.toml`) is only worth setting up when a friend sits
behind carrier-grade NAT, or your own ISP is CGNAT:

```powershell
.\target\release\aperture.exe --relay https://…workers.dev --turn "turn:your-host:3478?transport=udp,USER,PASS"
```

### Option B — your own VPS

Use this if you already run a box with a domain. The Rust relay speaks plain
HTTP; finish TLS with Caddy in front:

```toml
# Caddyfile
relay.example.com {
    reverse_proxy localhost:9000
}
```

```bash
./relay --port 9000 --bind 127.0.0.1
./target/release/aperture.exe --relay https://relay.example.com
```

Locally, to try it on one machine before hosting anything:

```powershell
.\target\release\relay.exe --port 9000
.\target\release\aperture.exe --relay http://localhost:9000
# open the printed share link in any browser
```

Notes:

* The token is not sent to the relay — it travels inside the encrypted
  peer connection. Anyone who knows a share link can *attempt* to connect;
  only the token holder gets in. Treat the link like a password.
* Change your token (`--token …` once, then remove the flag) if a link leaks;
  the room id changes with it.
* The relay sees: room ids, guest ids, timing, and each peer's public IP via
  ICE — that is inherent to any WebRTC rendezvous.
* Both peers must have a route to each other or to a TURN server. With STUN
  alone most home NATs (cone/full-cone) connect fine; behind carrier-grade
  NAT (some mobile networks) you need the TURN option above.

## Connecting from a phone

1. Put the phone on the same network as the PC, or join both to a VPN such as
   Tailscale (recommended if you leave the house).
2. Scan the QR code, or open the printed `phone` link.
3. Accept the self-signed certificate — Chrome: *Advanced → Proceed*; Safari:
   *Show Details → visit this website*. The certificate carries the PC's LAN IP
   as a subject alternative name and is regenerated when the IP changes.
4. Tap the fullscreen button for the full experience; landscape is recommended.

If the page does not load, allow the binary through Windows Firewall on private
networks:

```powershell
netsh advfirewall firewall add rule name="Aperture" dir=in action=allow program="%CD%\target\release\aperture.exe" enable=yes profile=private
```

The token lives in the URL (`?t=…`) and is remembered in the tab. If you open
`/` without it, the gate accepts either the raw token or a full link pasted from
the agent console.

## Controls

* **DIRECT** — touch position maps to the screen; tap clicks, drag drags,
  long-press (520 ms) right-clicks.
* **TRACKPAD** — relative pointer motion from finger movement, scaled to the
  stream resolution; better for precision on small screens.
* Two fingers on the canvas scroll. The wheel/hold modifiers bar (Ctrl, Alt,
  Shift, Win) stays latched until tapped again.
* The keyboard button opens a key grid plus F1–F12; the hidden input relays your
  phone's own keyboard, including IME composition.
* Settings (sliders icon): bitrate, monitor selection, force keyframe, HUD
  toggle, fullscreen, disconnect.
* Desktop shortcuts: `F11` fullscreen, `Esc` closes open panels. Everything else
  is forwarded to the remote PC, except shortcuts the browser reserves for
  itself (Ctrl+W/T/N, Cmd+…, etc.).

The HUD shows **LAT** (capture→display), **RTT** (ping), **FPS**, and **RATE**.

## Security model

* One 128-bit token gates every route and the WebSocket upgrade; comparisons are
  constant-time. Rotate it by deleting `%LOCALAPPDATA%\aperture\token.txt` and
  restarting.
* Transport is TLS with a locally generated self-signed certificate and key
  (cached next to the token). The connection is encrypted but not
  CA-authenticated — verify the fingerprint out of band if that matters to you.
* Do **not** port-forward this to the public internet. Scope it to your LAN or a
  private VPN overlay.
* Nothing else is exposed: no clipboard, audio, file transfer, or shell. Input
  injection is limited to mouse, wheel, keys, and text on the machine running
  the agent.

## Limitations

* Windows host only. The viewer requires WebCodecs, so older browsers are out.
* One monitor at a time; switch in settings (the stream restarts momentarily).
* UAC prompts and the lock/sign-in screen live on the Secure Desktop, which
  cannot be captured or driven — approve elevation at the PC.
* No audio, clipboard, file transfer, or session recording.
* Browser-reserved keyboard shortcuts cannot be forwarded.
* Software encoding works but costs CPU and adds latency; use a GPU if you have
  one.

## Project layout

```
src/main.rs        CLI, banner, listener
src/agent.rs       supervisor: capture→convert→encode pipeline, monitor switching
src/capture.rs     Windows Graphics Capture session
src/convert.rs     BGRA→NV12 bilinear downscale (rayon)
src/encoder/       Media Foundation H.264 wrapper + pipeline
src/display.rs     monitor enumeration / geometry
src/input.rs       SendInput injection (absolute, relative, wheel, keys, text)
src/protocol.rs    frame header + JSON control messages
src/server.rs      axum routes, token gate, WebSocket fan-out
src/webrtc.rs      DataChannel hosting for the friend (relay) link
src/tls.rs         token and self-signed certificate management
src/bin/relay.rs   public signaling relay + viewer hosting
cloudflare/        the same relay as a Cloudflare Worker + Durable Object
viewer/            embedded HTML/CSS/JS viewer (WebCodecs)
examples/probe.rs  standalone encoder enumeration probe
```

## Tests

```powershell
cargo test
```

covers the frame header encoding, token comparison, NV12 aspect fitting, and
Annex B handling. `cargo run --example probe` lists available H.264 encoders.
