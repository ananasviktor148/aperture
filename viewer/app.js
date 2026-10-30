// Aperture viewer: WebSocket / WebRTC transport, WebCodecs H.264, input relay.
(() => {
  "use strict";

  const $ = (id) => document.getElementById(id);
  const canvas = $("video");
  const ctx = canvas.getContext("2d", { alpha: false, desynchronized: true });
  const stage = $("stage");
  const gate = $("gate");
  const boot = $("boot");
  const bootMsg = $("boot-msg");
  const banner = $("banner");
  const bannerText = $("banner-text");
  const softInput = $("soft-input");
  const quality = $("quality");
  const qualityVal = $("quality-val");
  const monitorSelect = $("monitor");
  const modsBar = $("mods");
  const keysheet = $("keysheet");
  const settings = $("settings");

  const TOKEN_KEY = "aperture.token";
  const supportsWebCodecs = typeof window.VideoDecoder === "function";

  const S = {
    token: "",
    transport: null,
    room: "",
    open: false,
    manual: false,
    reconnectTimer: 0,
    attempts: 0,

    decoder: null,
    configured: false,
    avcC: null,
    codec: "",
    decoderW: 0,
    decoderH: 0,
    streamW: 0,
    streamH: 0,
    gotKeyframe: false,
    decodeEpoch: 0,

    offset: null,
    bestRtt: Infinity,
    rtt: 0,
    lastLat: 0,
    gotFrame: false,

    serverFps: null,
    serverKbps: null,
    meta: null,

    mode: "direct",
    mods: new Set(),
    held: new Set(),
    buttons: new Set(),
    pendingKeys: new Set(),
    pendingButtons: new Set(),

    movePending: null,
    relPending: { x: 0, y: 0 },
    wheelPending: { x: 0, y: 0 },
    raf: 0,

    lastKeyReq: 0,
    pingTimer: 0,
    statsTimer: 0,
    skipStreak: 0,
    autoBitrate: 0,
    autoCeiling: 0,
    lastBitrateShift: 0,
    manualHold: 0,

    longPress: 0,
    primary: null,
  };

  const pointers = new Map();
  let leftDown = false;
  let scroll = null;
  let downAt = 0;

  // ── helpers ────────────────────────────────────────────────────────

  const now = () => performance.now();

  function send(msg) {
    if (S.transport) S.transport.send(JSON.stringify(msg));
  }

  function input(ev) {
    send({ type: "input", ...ev });
  }

  function clamp(v, lo, hi) {
    return v < lo ? lo : v > hi ? hi : v;
  }

  function isSafelyEqual(a, b) {
    if (!a || !b || a.length !== b.length) return false;
    for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
    return true;
  }

  function tokenFrom(raw) {
    raw = (raw || "").trim();
    if (!raw) return "";
    try {
      const parsed = new URL(raw);
      const t = parsed.searchParams.get("t");
      if (t) return t.trim();
    } catch {
      /* not a URL */
    }
    const match = raw.match(/[?&]t=([^&\s]+)/);
    if (match) return decodeURIComponent(match[1]);
    return raw;
  }

  // ── gate / connection ──────────────────────────────────────────────

  function setGateMessage(text, error) {
    const el = $("gate-msg");
    el.textContent = text;
    el.classList.toggle("err", Boolean(error));
  }

  function showGate() {
    gate.hidden = false;
    stage.hidden = true;
    closeSheets();
    $("token").value = "";
    setGateMessage(
      S.token
        ? "A stored token exists. Press CONNECT to reuse it, or paste a new one."
        : "The link contains your token. You can also paste it here.",
      false
    );
  }

  function showStage() {
    gate.hidden = true;
    stage.hidden = false;
    requestAnimationFrame(() => window.scrollTo(0, 0));
  }

  function setBoot(text) {
    if (S.gotFrame) return;
    boot.classList.remove("done");
    bootMsg.textContent = text;
  }

  function hideBoot() {
    boot.classList.add("done");
  }

  function showBanner(text, retry) {
    bannerText.textContent = text;
    banner.hidden = false;
    $("banner-retry").hidden = !retry;
  }

  function hideBanner() {
    banner.hidden = true;
  }

  const DEFAULT_STUN = "stun:stun.l.google.com:19302";
  // Hosted relays (Cloudflare) close silent WebSockets; the relay answers this
  // frame for us, so the signalling link survives an idle peer connection.
  const SIGNAL_KEEPALIVE = "aperture-keepalive";

  function wsUrl() {
    const scheme = location.protocol === "https:" ? "wss" : "ws";
    return `${scheme}://${location.host}/ws?t=${encodeURIComponent(S.token)}`;
  }

  function signalingUrl() {
    const scheme = location.protocol === "https:" ? "wss" : "ws";
    return `${scheme}://${location.host}/ws/${encodeURIComponent(S.room)}?role=guest`;
  }

  // ── transports ─────────────────────────────────────────────────────
  // A transport sends JSON text and surfaces: open, message (string or
  // ArrayBuffer), close. The viewer speaks the same aperture protocol over
  // both; only the plumbing differs.

  function onTransportOpen() {
    S.open = true;
    S.attempts = 0;
    hideBanner();
    setBoot("AWAITING STREAM");
    for (const code of S.pendingKeys) send({ type: "input", kind: "key", code, down: false });
    S.pendingKeys.clear();
    for (const name of S.pendingButtons) {
      send({ type: "input", kind: "button", button: name, down: false });
    }
    S.pendingButtons.clear();
    startPing();
    requestKeyframe();
  }

  function onTransportMessage(data) {
    if (typeof data === "string") {
      handleServerMessage(data);
    } else {
      handleFrame(data);
    }
  }

  function onTransportClose() {
    S.open = false;
    stopPing();
    forgetLocalInput();
    if (S.manual) return;
    scheduleReconnect();
  }

  function teardownTransport() {
    const transport = S.transport;
    S.transport = null;
    if (transport) {
      try {
        transport.close();
      } catch {
        /* ignore */
      }
    }
  }

  function createWsTransport() {
    const ws = new WebSocket(wsUrl());
    ws.binaryType = "arraybuffer";
    const transport = {
      send(text) {
        if (ws.readyState === WebSocket.OPEN) ws.send(text);
      },
      close() {
        try {
          ws.close(1000, "client disconnect");
        } catch {
          /* ignore */
        }
      },
    };
    ws.onopen = () => {
      if (S.transport === transport) onTransportOpen();
    };
    ws.onmessage = (event) => {
      if (S.transport === transport) onTransportMessage(event.data);
    };
    ws.onclose = () => {
      if (S.transport === transport) onTransportClose();
    };
    ws.onerror = () => {
      /* close follows */
    };
    return transport;
  }

  // Guest side of the friend link: the relay page carries the room; we
  // negotiate a DataChannel straight to the agent and run the usual
  // protocol over it.
  function createRtcTransport() {
    let pc = null;
    let dc = null;
    let sig = null;
    let keepalive = 0;
    let started = false;
    let closed = false;

    const transport = {
      send(text) {
        if (dc && dc.readyState === "open") dc.send(text);
      },
      close() {
        closed = true;
        clearInterval(keepalive);
        for (const obj of [dc, pc, sig]) {
          if (obj) {
            try {
              obj.close();
            } catch {
              /* ignore */
            }
          }
        }
      },
    };

    function startPeer() {
      if (started) return;
      started = true;
      if (pc) {
        try {
          pc.close();
        } catch {
          /* ignore */
        }
        pc = null;
        dc = null;
      }
      setBoot("NEGOTIATING PEER CONNECTION");
      pc = new RTCPeerConnection({
        iceServers: window.APERTURE_ICE || [{ urls: [DEFAULT_STUN] }],
      });
      pc.onicecandidate = (event) => {
        if (event.candidate && sig && sig.readyState === WebSocket.OPEN) {
          sig.send(JSON.stringify({ type: "ice", candidate: event.candidate.toJSON() }));
        }
      };
      pc.onconnectionstatechange = () => {
        if (pc.connectionState === "failed") transport.close();
      };
      dc = pc.createDataChannel("aperture", { ordered: true });
      dc.binaryType = "arraybuffer";
      dc.onopen = () => {
        if (S.transport !== transport) return;
        transport.send(JSON.stringify({ type: "auth", token: S.token }));
        onTransportOpen();
      };
      dc.onmessage = (event) => {
        if (S.transport === transport) onTransportMessage(event.data);
      };
      dc.onclose = () => {
        if (!closed && S.transport === transport) onTransportClose();
      };
      pc.setLocalDescription(pc.createOffer())
        .then(() => {
          if (sig && sig.readyState === WebSocket.OPEN) {
            sig.send(JSON.stringify({ type: "offer", sdp: pc.localDescription.sdp }));
          }
        })
        .catch(() => transport.close());
    }

    sig = new WebSocket(signalingUrl());
    keepalive = setInterval(() => {
      if (sig && sig.readyState === WebSocket.OPEN) sig.send(SIGNAL_KEEPALIVE);
    }, 25000);
    sig.onmessage = (event) => {
      let msg;
      try {
        msg = JSON.parse(event.data);
      } catch {
        return;
      }
      if (S.transport !== transport) return;
      if (msg.type === "joined") {
        if (msg.host) {
          startPeer();
        } else {
          setBoot("WAITING FOR HOST TO COME ONLINE");
        }
      } else if (msg.type === "host-joined") {
        started = false;
        startPeer();
      } else if (msg.type === "answer" && pc) {
        pc.setRemoteDescription({ type: "answer", sdp: msg.sdp }).catch(() => {});
      } else if (msg.type === "ice" && pc && msg.candidate) {
        pc.addIceCandidate(msg.candidate).catch(() => {});
      }
    };
    sig.onclose = () => {
      if (!closed && S.transport === transport) onTransportClose();
    };
    sig.onerror = () => {
      /* close follows */
    };

    return transport;
  }

  function connect() {
    clearTimeout(S.reconnectTimer);
    S.manual = false;
    hideBanner();
    teardownTransport();

    if (!S.token) {
      showGate();
      return;
    }
    if (!supportsWebCodecs) {
      setBoot("WEBCODECS NOT AVAILABLE");
      showBanner("This browser has no WebCodecs. Use Chrome, Edge, or Safari 16.4+.", false);
      return;
    }

    setBoot(S.attempts ? "RE-ESTABLISHING LINK" : "NEGOTIATING LINK");

    try {
      S.transport = S.room ? createRtcTransport() : createWsTransport();
    } catch {
      scheduleReconnect();
    }
  }

  function scheduleReconnect() {
    if (S.manual) return;
    S.attempts += 1;
    const delay = Math.min(6000, 600 * Math.pow(1.6, Math.min(S.attempts, 6)));
    if (S.gotFrame) {
      showBanner("LINK LOST — RECONNECTING", true);
    } else {
      setBoot("LINK UNAVAILABLE");
      showBanner("Could not reach the agent. Check the token and the certificate.", true);
    }
    clearTimeout(S.reconnectTimer);
    S.reconnectTimer = setTimeout(connect, delay);
  }

  function disconnect() {
    S.manual = true;
    releaseRemote();
    stopPing();
    resetDecoder();
    hideBanner();
    teardownTransport();
    S.open = false;
    S.gotFrame = false;
    boot.classList.remove("done");
    showGate();
  }

  // ── server messages ────────────────────────────────────────────────

  function handleServerMessage(text) {
    let msg;
    try {
      msg = JSON.parse(text);
    } catch {
      return;
    }

    switch (msg.type) {
      case "hello":
        applyHello(msg);
        break;
      case "stats":
        S.serverFps = msg.fps;
        S.serverKbps = msg.kbps;
        break;
      case "pong":
        applyPong(msg);
        break;
      case "notice":
        showBanner(msg.message || "notice", false);
        break;
      default:
        break;
    }
  }

  // Grid percentage max-height is unreliable on replaced elements, so the
  // letterbox size is computed here to keep the canvas rect equal to the image.
  function fitCanvas() {
    const holder = canvas.parentElement;
    if (!holder) return;
    const style = getComputedStyle(holder);
    const availW =
      holder.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight);
    const availH =
      holder.clientHeight - parseFloat(style.paddingTop) - parseFloat(style.paddingBottom);
    if (availW < 4 || availH < 4) return;
    const ratio = (canvas.width || 16) / (canvas.height || 9);
    let width = availW;
    let height = width / ratio;
    if (height > availH) {
      height = availH;
      width = height * ratio;
    }
    canvas.style.width = `${Math.max(4, Math.floor(width))}px`;
    canvas.style.height = `${Math.max(4, Math.floor(height))}px`;
  }

  function applyHello(msg) {
    const previous = S.meta;
    S.meta = msg;

    const dimsChanged =
      !previous ||
      previous.width !== msg.width ||
      previous.height !== msg.height ||
      previous.monitor !== msg.monitor;

    S.streamW = msg.width;
    S.streamH = msg.height;

    if (!S.autoCeiling) {
      S.autoCeiling = msg.bitrate;
      S.autoBitrate = msg.bitrate;
    }

    if (canvas.width !== msg.width || canvas.height !== msg.height) {
      canvas.width = Math.max(2, msg.width);
      canvas.height = Math.max(2, msg.height);
    }
    fitCanvas();

    $("hud-res").textContent = `${msg.width}×${msg.height}`;

    const monitor = (msg.monitors || []).find((m) => m.index === msg.monitor);
    $("src-line").textContent = monitor
      ? `${monitor.name} · ${msg.desktop_width}×${msg.desktop_height} · ${msg.width}×${msg.height} · ${msg.encoder}`
      : `${msg.width}×${msg.height} · ${msg.encoder}`;

    buildMonitorOptions(msg);

    if (document.activeElement !== quality) {
      const mbps = clamp(Math.round(msg.bitrate / 1_000_000), 2, 60);
      quality.value = String(mbps);
      paintQuality(mbps);
    }

    const fineprint = $("fineprint");
    fineprint.textContent =
      `${msg.encoder}\n` +
      `${msg.fps} fps cap · ${(msg.bitrate / 1_000_000).toFixed(1)} Mb/s target\n` +
      `Browser shortcuts (Ctrl+W, Ctrl+T) stay local. No audio or clipboard relay.\n` +
      `Token-gated H.264 over TLS — treat this link like a password.`;

    if (dimsChanged && S.configured) {
      resetDecoder();
      requestKeyframe();
    }
    if (dimsChanged) {
      hideBanner();
      S.decodeEpoch = 0;
    }
  }

  function buildMonitorOptions(msg) {
    const monitors = msg.monitors || [];
    if (monitorSelect.options.length === monitors.length) {
      monitorSelect.value = String(msg.monitor);
      return;
    }
    monitorSelect.replaceChildren();
    for (const m of monitors) {
      const option = document.createElement("option");
      option.value = String(m.index);
      option.textContent = `${m.index} · ${m.name} · ${m.width}×${m.height}${m.primary ? " · primary" : ""}`;
      monitorSelect.append(option);
    }
    monitorSelect.value = String(msg.monitor);
  }

  function applyPong(msg) {
    const t = now();
    const rtt = Math.max(0, t - msg.ts);
    S.rtt = S.rtt ? S.rtt * 0.8 + rtt * 0.2 : rtt;

    const serverMs = msg.server_us / 1000;
    const offset = serverMs - (msg.ts + rtt / 2);

    if (S.offset === null || rtt <= S.bestRtt + 0.4) {
      S.bestRtt = Math.min(S.bestRtt, rtt);
      S.offset = offset;
      if (rtt > 2000) S.bestRtt = Infinity;
    }
  }

  function startPing() {
    stopPing();
    const tick = () => send({ type: "ping", ts: Math.round(now()) });
    S.autoBitrate = S.autoCeiling;
    S.lastBitrateShift = 0;
    tick();
    S.pingTimer = setInterval(tick, 500);
  }

  function stopPing() {
    clearInterval(S.pingTimer);
    S.pingTimer = 0;
    S.rtt = 0;
    S.bestRtt = Infinity;
    S.offset = null;
  }

  // ── video decode ───────────────────────────────────────────────────

  function splitAnnexB(data) {
    const nals = [];
    let start = -1;
    let i = 0;
    while (i + 3 <= data.length) {
      if (data[i] === 0 && data[i + 1] === 0 && data[i + 2] === 1) {
        if (start >= 0) nals.push(data.subarray(start, i));
        i += 3;
        start = i;
        continue;
      }
      if (
        data[i] === 0 &&
        data[i + 1] === 0 &&
        data[i + 2] === 0 &&
        i + 3 < data.length &&
        data[i + 3] === 1
      ) {
        if (start >= 0) nals.push(data.subarray(start, i));
        i += 4;
        start = i;
        continue;
      }
      i += 1;
    }
    if (start >= 0 && start < data.length) nals.push(data.subarray(start));
    return nals;
  }

  function toAvcc(nals) {
    let size = 0;
    for (const nal of nals) size += 4 + nal.length;
    const out = new Uint8Array(size);
    const view = new DataView(out.buffer);
    let offset = 0;
    for (const nal of nals) {
      view.setUint32(offset, nal.length);
      offset += 4;
      out.set(nal, offset);
      offset += nal.length;
    }
    return out;
  }

  function codecString(sps) {
    const hex = (n) => n.toString(16).padStart(2, "0");
    return `avc1.${hex(sps[1])}${hex(sps[2])}${hex(sps[3])}`;
  }

  function buildAvcC(spsList, ppsList) {
    let size = 5 + 1 + 2 + 1;
    for (const sps of spsList) size += 2 + sps.length;
    for (const pps of ppsList) size += 2 + pps.length;

    const out = new Uint8Array(size);
    let o = 0;
    out[o++] = 1;
    out[o++] = spsList[0][1];
    out[o++] = spsList[0][2];
    out[o++] = spsList[0][3];
    out[o++] = 0xff;
    out[o++] = 0xe0 | (spsList.length & 0x1f);
    for (const sps of spsList) {
      out[o++] = (sps.length >> 8) & 0xff;
      out[o++] = sps.length & 0xff;
      out.set(sps, o);
      o += sps.length;
    }
    out[o++] = ppsList.length & 0xff;
    for (const pps of ppsList) {
      out[o++] = (pps.length >> 8) & 0xff;
      out[o++] = pps.length & 0xff;
      out.set(pps, o);
      o += pps.length;
    }
    return out;
  }

  function resetDecoder() {
    const decoder = S.decoder;
    S.decoder = null;
    S.configured = false;
    S.avcC = null;
    S.codec = "";
    S.decoderW = 0;
    S.decoderH = 0;
    S.gotKeyframe = false;
    if (decoder) {
      try {
        decoder.close();
      } catch {
        /* already closed */
      }
    }
  }

  function configureDecoder(description, codec) {
    resetDecoder();
    try {
      const decoder = new VideoDecoder({
        output: onVideoFrame,
        error: onDecoderError,
      });
      decoder.configure({
        codec,
        description,
        optimizeForLatency: true,
        hardwareAcceleration: "no-preference",
      });
      S.decoder = decoder;
      S.configured = true;
      S.codec = codec;
      S.avcC = description.slice();
      S.decoderW = S.streamW;
      S.decoderH = S.streamH;
      return true;
    } catch (err) {
      console.warn("decoder configure failed", err);
      S.decoder = null;
      S.configured = false;
      return false;
    }
  }

  function onDecoderError(err) {
    console.warn("decoder error", err);
    resetDecoder();
    requestKeyframe(true);
    if (now() - (S.decodeEpoch || 0) > 4000) {
      S.decodeEpoch = now();
      showBanner("VIDEO RESYNC — REQUESTING KEYFRAME", false);
      setTimeout(() => {
        if (!banner.hidden) hideBanner();
      }, 2200);
    }
  }

  function onVideoFrame(frame) {
    try {
      const w = frame.displayWidth || canvas.width;
      const h = frame.displayHeight || canvas.height;
      if (canvas.width !== w || canvas.height !== h) {
        canvas.width = w;
        canvas.height = h;
        fitCanvas();
      }
      ctx.drawImage(frame, 0, 0, canvas.width, canvas.height);
      if (S.offset !== null) {
        const captureMs = frame.timestamp / 1000;
        const age = now() + S.offset - captureMs;
        if (Number.isFinite(age)) S.lastLat = Math.max(0, age);
      }
      if (!S.gotFrame) {
        S.gotFrame = true;
        hideBoot();
        hideBanner();
      }
    } finally {
      frame.close();
    }
  }

  // WebRTC DataChannels cap one message near 64 KiB, so the agent fragments
  // large frames: magic 1 starts (and continues) a frame, magic 2 continues.
  let contHeader = null;
  let contParts = [];

  function handleFrame(buffer) {
    if (buffer.byteLength <= 8) return;
    const bytes = new Uint8Array(buffer);
    const magic = bytes[0];

    if (magic === 1 && (bytes[1] & 2) === 2) {
      contHeader = bytes.subarray(0, 8);
      contParts = [bytes.subarray(8)];
      return;
    }
    if (magic === 2) {
      if (!contHeader) return; // lost sync — wait for the next IDR
      contParts.push(bytes.subarray(8));
      if ((bytes[1] & 2) === 2) return;
      const header = contHeader;
      const parts = contParts;
      contHeader = null;
      contParts = [];
      let total = 8;
      for (const p of parts) total += p.length;
      const full = new Uint8Array(total);
      full.set(header, 0);
      let off = 8;
      for (const p of parts) {
        full.set(p, off);
        off += p.length;
      }
      deliverFrame(full);
      return;
    }
    if (magic !== 1) return;
    // A fresh frame abandons any half-reassembled one.
    contHeader = null;
    contParts = [];
    deliverFrame(bytes);
  }

  function deliverFrame(bytes) {
    const keyframe = (bytes[1] & 1) === 1;
    const captureMs = bytes[4] | (bytes[5] << 8) | (bytes[6] << 16) | (bytes[7] << 24);
    const payload = bytes.subarray(8);
    const nals = splitAnnexB(payload);
    if (!nals.length) return;

    const sps = [];
    const pps = [];
    for (const nal of nals) {
      if (!nal.length) continue;
      const type = nal[0] & 0x1f;
      if (type === 7) sps.push(nal);
      else if (type === 8) pps.push(nal);
    }

    if (sps.length && pps.length) {
      const description = buildAvcC(sps, pps);
      const codec = codecString(sps[0]);
      const dimsChanged = S.decoderW !== S.streamW || S.decoderH !== S.streamH;
      if (
        !S.configured ||
        dimsChanged ||
        codec !== S.codec ||
        !isSafelyEqual(description, S.avcC)
      ) {
        if (!configureDecoder(description, codec)) {
          requestKeyframe();
          return;
        }
      }
    }

    if (!S.configured) {
      requestKeyframe();
      return;
    }
    if (!keyframe && !S.gotKeyframe) {
      requestKeyframe();
      return;
    }

    const decoder = S.decoder;
    if (!decoder || decoder.state !== "configured") {
      requestKeyframe();
      return;
    }

    // Freshness over completeness: queued or already-stale delta frames only
    // add latency, so skip them and resync at the next IDR.
    const ageMs = S.offset === null ? 0 : now() - ((captureMs >>> 0) + S.offset);
    if (!keyframe && (decoder.decodeQueueSize > 2 || ageMs > 160)) {
      S.skipStreak += 1;
      if (S.skipStreak >= 10) {
        S.skipStreak = 0;
        requestKeyframe();
      }
      return;
    }

    if (decoder.decodeQueueSize > 24) {
      resetDecoder();
      requestKeyframe(true);
      return;
    }

    const chunk = new EncodedVideoChunk({
      type: keyframe ? "key" : "delta",
      timestamp: (captureMs >>> 0) * 1000,
      data: toAvcc(nals),
    });

    try {
      decoder.decode(chunk);
      S.skipStreak = 0;
      if (keyframe) S.gotKeyframe = true;
    } catch (err) {
      console.warn("decode failed", err);
      resetDecoder();
      requestKeyframe();
    }
  }

  function requestKeyframe(force) {
    const t = now();
    if (!force && t - S.lastKeyReq < 350) return;
    S.lastKeyReq = t;
    send({ type: "keyframe" });
  }

  // ── telemetry HUD ──────────────────────────────────────────────────

  function paintHud() {
    const lat = S.offset === null ? null : Math.round(S.lastLat);
    $("m-lat").textContent = lat === null ? "—" : String(clamp(lat, 0, 9999));
    $("m-rtt").textContent = S.rtt ? String(Math.round(S.rtt)) : "—";
    $("m-fps").textContent = S.serverFps === null ? "—" : String(Math.round(S.serverFps));
    $("m-rate").textContent = S.serverKbps === null ? "—" : (S.serverKbps / 1000).toFixed(1);
    S.statsTimer = 0;
  }

  setInterval(() => {
    if (!S.statsTimer) {
      S.statsTimer = 1;
      requestAnimationFrame(paintHud);
    }
  }, 400);

  // Congestion control: track the decoder backlog and RTT, and walk the
  // bitrate down before frames pile up (and back up when the link is clean).
  setInterval(() => {
    if (!S.open || !S.autoCeiling || now() < S.manualHold) return;
    const q = S.decoder ? S.decoder.decodeQueueSize : 0;
    let target = S.autoBitrate;
    if (q > 6 || S.rtt > 140) target *= 0.7;
    else if (q > 3) target *= 0.88;
    else if (q < 2 && S.rtt > 0 && S.rtt < 60) target *= 1.15;
    target = clamp(Math.round(target), 2_000_000, S.autoCeiling);
    if (target !== S.autoBitrate && now() - S.lastBitrateShift > 2000) {
      S.autoBitrate = target;
      S.lastBitrateShift = now();
      send({ type: "bitrate", bitrate: target });
    }
  }, 1000);

  // ── pointer / touch input ──────────────────────────────────────────

  function norm(clientX, clientY) {
    const rect = canvas.getBoundingClientRect();
    return {
      x: clamp((clientX - rect.left) / Math.max(1, rect.width), 0, 1),
      y: clamp((clientY - rect.top) / Math.max(1, rect.height), 0, 1),
    };
  }

  function queueMove(clientX, clientY) {
    S.movePending = norm(clientX, clientY);
    scheduleFlush();
  }

  function queueRel(dx, dy) {
    S.relPending.x += dx;
    S.relPending.y += dy;
    scheduleFlush();
  }

  function scheduleFlush() {
    if (S.raf) return;
    S.raf = requestAnimationFrame(flushInput);
  }

  function flushInput() {
    S.raf = 0;
    if (S.movePending) {
      input({ kind: "move", x: S.movePending.x, y: S.movePending.y });
      S.movePending = null;
    }
    const rel = S.relPending;
    if (rel.x || rel.y) {
      input({ kind: "moveRel", dx: rel.x, dy: rel.y });
      S.relPending = { x: 0, y: 0 };
    }
    const wheel = S.wheelPending;
    if (wheel.x || wheel.y) {
      const dx = Math.trunc(wheel.x);
      const dy = Math.trunc(wheel.y);
      wheel.x -= dx;
      wheel.y -= dy;
      if (dx || dy) input({ kind: "wheel", dx, dy });
    }
  }

  function queueWheel(ticksX, ticksY) {
    S.wheelPending.x += ticksX;
    S.wheelPending.y += ticksY;
    scheduleFlush();
  }

  function pressButton(name, down) {
    if (down) {
      if (S.buttons.has(name)) return;
      S.buttons.add(name);
    } else {
      if (!S.buttons.has(name)) return;
      S.buttons.delete(name);
    }
    input({ kind: "button", button: name, down });
  }

  function releaseButtons() {
    for (const name of [...S.buttons]) pressButton(name, false);
  }

  function buttonName(index) {
    return index === 0 ? "left" : index === 1 ? "middle" : index === 2 ? "right" : null;
  }

  function relativeFactor(pointerType) {
    if (pointerType !== "touch") return 1.15;
    const rect = canvas.getBoundingClientRect();
    const scale = S.streamW && rect.width ? S.streamW / rect.width : 1;
    return clamp(scale, 0.9, 3.4);
  }

  function startLongPress(pointer) {
    clearTimeout(S.longPress);
    S.longPress = setTimeout(() => {
      if (S.mode !== "direct" || pointer.moved || pointers.size !== 1) return;
      pointer.longPressed = true;
      pressButton("left", false);
      leftDown = false;
      pressButton("right", false);
      pressButton("right", true);
      setTimeout(() => pressButton("right", false), 40);
      if (navigator.vibrate) navigator.vibrate(8);
    }, 520);
  }

  function beginScroll() {
    clearTimeout(S.longPress);
    if (leftDown) {
      pressButton("left", false);
      leftDown = false;
    }
    scroll = { cx: 0, cy: 0 };
    recomputeCentroid();
  }

  function recomputeCentroid() {
    if (!scroll) return;
    let x = 0;
    let y = 0;
    for (const p of pointers.values()) {
      x += p.x;
      y += p.y;
    }
    const n = pointers.size || 1;
    scroll.cx = x / n;
    scroll.cy = y / n;
  }

  function endScroll() {
    scroll = null;
    for (const p of pointers.values()) {
      p.startX = p.x;
      p.startY = p.y;
      p.moved = false;
      p.longPressed = true;
    }
  }

  canvas.addEventListener(
    "pointerdown",
    (e) => {
      if (e.button > 2 && e.pointerType !== "touch") return;
      e.preventDefault();
      try {
        canvas.setPointerCapture(e.pointerId);
      } catch {
        /* ignore */
      }

      const p = {
        type: e.pointerType,
        x: e.clientX,
        y: e.clientY,
        startX: e.clientX,
        startY: e.clientY,
        moved: false,
        longPressed: false,
      };
      pointers.set(e.pointerId, p);

      if (e.pointerType === "touch") {
        if (pointers.size === 2) {
          beginScroll();
          return;
        }
        if (pointers.size > 2) return;
        S.primary = e.pointerId;
        downAt = Date.now();
        if (S.mode === "direct") queueMove(e.clientX, e.clientY);
        startLongPress(p);
        return;
      }

      S.primary = e.pointerId;
      const name = buttonName(e.button);
      if (!name) return;
      if (S.mode === "direct") queueMove(e.clientX, e.clientY);
      pressButton(name, true);
    },
    { passive: false }
  );

  canvas.addEventListener(
    "pointermove",
    (e) => {
      const p = pointers.get(e.pointerId);
      if (!p) {
        if (e.pointerType === "mouse" && S.mode === "direct") {
          queueMove(e.clientX, e.clientY);
        }
        return;
      }
      e.preventDefault();

      const dx = e.clientX - p.x;
      const dy = e.clientY - p.y;
      p.x = e.clientX;
      p.y = e.clientY;

      if (Math.hypot(p.x - p.startX, p.y - p.startY) > 6) p.moved = true;

      if (scroll && pointers.size >= 2) {
        const previousX = scroll.cx;
        const previousY = scroll.cy;
        recomputeCentroid();
        if (previousX || previousY) {
          queueWheel(-(scroll.cx - previousX) / 26, (scroll.cy - previousY) / 26);
        }
        return;
      }

      if (p.moved) clearTimeout(S.longPress);

      if (S.mode === "direct") {
        queueMove(e.clientX, e.clientY);
        if (p.type === "touch" && p.moved && !leftDown && !p.longPressed) {
          pressButton("left", true);
          leftDown = true;
        }
      } else {
        const k = relativeFactor(p.type);
        if (dx || dy) queueRel(dx * k, dy * k);
        if (p.type === "touch" && p.moved && !leftDown && !p.longPressed) {
          pressButton("left", true);
          leftDown = true;
        }
      }
    },
    { passive: false }
  );

  function finishPointer(e, cancelled) {
    const p = pointers.get(e.pointerId);
    if (!p) return;
    pointers.delete(e.pointerId);
    clearTimeout(S.longPress);

    if (scroll) {
      if (pointers.size < 2) endScroll();
      return;
    }

    if (p.type === "touch") {
      if (S.primary === e.pointerId) S.primary = null;
      if (leftDown) {
        pressButton("left", false);
        leftDown = false;
      } else if (!p.longPressed && !cancelled) {
        const quick = Date.now() - downAt < 600;
        if (quick) {
          pressButton("left", true);
          setTimeout(() => pressButton("left", false), 24);
        }
      }
      return;
    }

    S.primary = null;
    const name = buttonName(e.button);
    if (name) pressButton(name, false);
  }

  canvas.addEventListener(
    "pointerup",
    (e) => {
      e.preventDefault();
      finishPointer(e, false);
    },
    { passive: false }
  );

  canvas.addEventListener(
    "pointercancel",
    (e) => {
      finishPointer(e, true);
    },
    { passive: false }
  );

  canvas.addEventListener("contextmenu", (e) => e.preventDefault());

  canvas.addEventListener(
    "wheel",
    (e) => {
      e.preventDefault();
      const unit = e.deltaMode === 1 ? 18 : e.deltaMode === 2 ? 420 : 1;
      queueWheel((e.deltaX * unit) / 100, (-e.deltaY * unit) / 100);
    },
    { passive: false }
  );

  // ── keyboard ───────────────────────────────────────────────────────

  function remoteKey(code, down) {
    if (!code) return;
    if (down) S.held.add(code);
    else S.held.delete(code);
    input({ kind: "key", code, down });
  }

  function tapKey(code) {
    if (!code) return;
    input({ kind: "key", code, down: true });
    setTimeout(() => input({ kind: "key", code, down: false }), 32);
  }

  function releaseRemote() {
    for (const code of [...S.held]) input({ kind: "key", code, down: false });
    S.held.clear();
    for (const code of [...S.mods]) input({ kind: "key", code, down: false });
    S.mods.clear();
    paintMods();
    releaseButtons();
  }

  function forgetLocalInput() {
    for (const code of S.held) S.pendingKeys.add(code);
    for (const code of S.mods) S.pendingKeys.add(code);
    for (const name of S.buttons) S.pendingButtons.add(name);
    S.held.clear();
    S.buttons.clear();
    S.mods.clear();
    paintMods();
    leftDown = false;
    pointers.clear();
    scroll = null;
  }

  window.addEventListener("keydown", (e) => {
    if (stage.hidden) return;
    if (e.target === softInput) return;
    if (e.target instanceof HTMLInputElement || e.target instanceof HTMLSelectElement) return;
    if (!S.open) return;

    if (e.key === "Escape" && (!keysheet.hidden || !settings.hidden || !modsBar.hidden)) {
      closeSheets();
      e.preventDefault();
      return;
    }

    if (e.key === "F11") {
      toggleFullscreen();
      e.preventDefault();
      return;
    }

    const code = e.code;
    if (!code) return;
    remoteKey(code, true);
    e.preventDefault();
  });

  window.addEventListener("keyup", (e) => {
    if (stage.hidden) return;
    if (e.target === softInput) return;
    if (e.target instanceof HTMLInputElement || e.target instanceof HTMLSelectElement) return;
    if (!e.code) return;
    remoteKey(e.code, false);
  });

  window.addEventListener("blur", () => {
    for (const code of [...S.held]) input({ kind: "key", code, down: false });
    S.held.clear();
    releaseButtons();
  });

  document.addEventListener("visibilitychange", () => {
    if (document.hidden) {
      for (const code of [...S.held]) input({ kind: "key", code, down: false });
      S.held.clear();
      releaseButtons();
    } else if (S.open) {
      send({ type: "ping", ts: Math.round(now()) });
    }
  });

  window.addEventListener("pagehide", () => {
    releaseRemote();
  });

  softInput.addEventListener("keydown", (e) => {
    if (e.key === "Backspace" || e.key === "Enter" || e.key === "Tab" || e.key.startsWith("Arrow")) {
      e.preventDefault();
      tapKey(e.code);
    }
  });

  softInput.addEventListener("input", (e) => {
    if (e.isComposing) return;
    const value = softInput.value;
    if (!value) return;
    input({ kind: "text", text: value });
    softInput.value = "";
  });

  softInput.addEventListener("compositionend", () => {
    const value = softInput.value;
    if (!value) return;
    input({ kind: "text", text: value });
    softInput.value = "";
  });

  softInput.addEventListener("paste", (e) => {
    const text = e.clipboardData?.getData("text");
    if (text) {
      e.preventDefault();
      input({ kind: "text", text });
    }
  });

  // ── UI wiring ──────────────────────────────────────────────────────

  function paintQuality(mbps) {
    qualityVal.textContent = `${mbps} Mb/s`;
    const min = Number(quality.min) || 2;
    const max = Number(quality.max) || 60;
    quality.style.setProperty("--pct", `${((mbps - min) / (max - min)) * 100}%`);
  }

  function setMode(mode) {
    S.mode = mode;
    for (const button of document.querySelectorAll("[data-mode]")) {
      button.classList.toggle("on", button.dataset.mode === mode);
    }
    releaseButtons();
    leftDown = false;
    clearTimeout(S.longPress);
  }

  function paintMods() {
    for (const button of document.querySelectorAll("[data-mod]")) {
      button.classList.toggle("on", S.mods.has(button.dataset.mod));
    }
    $("btn-mods").classList.toggle("on", !modsBar.hidden || S.mods.size > 0);
  }

  function toggleMod(code) {
    const active = S.mods.has(code);
    if (active) {
      S.mods.delete(code);
      input({ kind: "key", code, down: false });
    } else {
      S.mods.add(code);
      input({ kind: "key", code, down: true });
    }
    paintMods();
  }

  function closeSheets() {
    keysheet.hidden = true;
    settings.hidden = true;
    paintMods();
  }

  function toggleSheet(sheet) {
    const show = sheet.hidden;
    closeSheets();
    if (show) sheet.hidden = false;
    paintMods();
  }

  async function toggleFullscreen() {
    try {
      if (!document.fullscreenElement) {
        await document.documentElement.requestFullscreen();
      } else {
        await document.exitFullscreen();
      }
    } catch {
      /* unsupported */
    }
  }

  const KEYS = [
    ["ESC", "Escape"],
    ["TAB", "Tab"],
    ["ENTER", "Enter"],
    ["BKSP", "Backspace"],
    ["DEL", "Delete"],
    ["INS", "Insert"],
    ["HOME", "Home"],
    ["END", "End"],
    ["PGUP", "PageUp"],
    ["PGDN", "PageDown"],
    ["↑", "ArrowUp"],
    ["↓", "ArrowDown"],
    ["←", "ArrowLeft"],
    ["→", "ArrowRight"],
    ["CTRL", "ControlLeft"],
    ["ALT", "AltLeft"],
    ["SHIFT", "ShiftLeft"],
    ["WIN", "MetaLeft"],
    ["CAPS", "CapsLock"],
    ["MENU", "ContextMenu"],
    ["PRTSC", "PrintScreen"],
    ["NUMLK", "NumLock"],
  ];
  for (let i = 1; i <= 12; i++) KEYS.push([`F${i}`, `F${i}`]);

  function buildKeygrid() {
    const grid = $("keygrid");
    const fragment = document.createDocumentFragment();
    for (const [label, code] of KEYS) {
      const button = document.createElement("button");
      button.type = "button";
      button.textContent = label;
      button.addEventListener("click", (e) => {
        e.preventDefault();
        tapKey(code);
      });
      fragment.append(button);
    }
    grid.append(fragment);
  }

  function bindUi() {
    $("gate-form").addEventListener("submit", (e) => {
      e.preventDefault();
      const raw = $("token").value.trim();
      const token = tokenFrom(raw) || S.token;
      if (!token) {
        setGateMessage("Paste the access token or the full link printed by the agent.", true);
        return;
      }
      S.token = token;
      try {
        sessionStorage.setItem(TOKEN_KEY, token);
      } catch {
        /* storage disabled */
      }
      S.attempts = 0;
      showStage();
      hiddenBootReset();
      connect();
    });

    $("banner-retry").addEventListener("click", () => {
      S.attempts = 0;
      connect();
    });

    for (const button of document.querySelectorAll("[data-mode]")) {
      button.addEventListener("click", () => setMode(button.dataset.mode));
    }

    for (const button of document.querySelectorAll("[data-mod]")) {
      button.addEventListener("click", () => toggleMod(button.dataset.mod));
    }

    $("btn-mods").addEventListener("click", () => {
      modsBar.hidden = !modsBar.hidden;
      paintMods();
    });

    $("btn-keyboard").addEventListener("click", () => {
      softInput.focus({ preventScroll: true });
      if (document.activeElement !== softInput) {
        softInput.value = "";
      }
    });

    $("btn-keys").addEventListener("click", () => toggleSheet(keysheet));
    $("btn-settings").addEventListener("click", () => toggleSheet(settings));
    $("btn-fullscreen").addEventListener("click", toggleFullscreen);
    $("btn-power").addEventListener("click", disconnect);
    $("quit-btn").addEventListener("click", disconnect);

    for (const button of document.querySelectorAll("[data-close]")) {
      button.addEventListener("click", closeSheets);
    }

    quality.addEventListener("input", () => {
      const mbps = Number(quality.value);
      paintQuality(mbps);
      S.autoCeiling = mbps * 1_000_000;
      S.autoBitrate = S.autoCeiling;
      S.manualHold = now() + 15000;
      clearTimeout(quality._timer);
      quality._timer = setTimeout(() => {
        send({ type: "bitrate", bitrate: mbps * 1_000_000 });
      }, 260);
    });

    monitorSelect.addEventListener("change", () => {
      send({ type: "monitor", index: Number(monitorSelect.value) });
      requestKeyframe(true);
    });

    $("kf-btn").addEventListener("click", () => requestKeyframe(true));

    $("hud-btn").addEventListener("click", () => {
      S.hud = !S.hud;
      stage.classList.toggle("hud-off", !S.hud);
    });

    window.addEventListener("resize", fitCanvas);

    document.addEventListener("fullscreenchange", () => {
      $("btn-fullscreen").classList.toggle("on", Boolean(document.fullscreenElement));
      setTimeout(fitCanvas, 60);
    });
  }

  function hiddenBootReset() {
    S.gotFrame = false;
    boot.classList.remove("done");
    setBoot("NEGOTIATING LINK");
  }

  // ── boot ───────────────────────────────────────────────────────────

  function init() {
    bindUi();
    buildKeygrid();
    modsBar.hidden = true;
    paintQuality(Number(quality.value));

    if (!supportsWebCodecs) {
      setGateMessage(
        "This browser lacks WebCodecs. Use Chrome, Edge, or Safari 16.4 or newer.",
        true
      );
      $("connect-btn").disabled = true;
      $("connect-btn").style.opacity = "0.4";
    }

    // Friend links are served by the relay at /r/<room>; the room is also
    // injected as window.APERTURE_ROOM for good measure.
    const roomMatch = location.pathname.match(/\/r\/([A-Za-z0-9_-]+)/);
    S.room = (roomMatch && roomMatch[1]) || window.APERTURE_ROOM || "";

    let paramToken = "";
    try {
      paramToken = tokenFrom(new URLSearchParams(location.search).get("t") || "");
      if (paramToken) sessionStorage.setItem(TOKEN_KEY, paramToken);
    } catch {
      /* ignore */
    }
    if (!paramToken) {
      try {
        paramToken = sessionStorage.getItem(TOKEN_KEY) || "";
      } catch {
        paramToken = "";
      }
    }

    if (paramToken) {
      S.token = paramToken;
      showStage();
      hiddenBootReset();
      connect();
    } else {
      showGate();
    }
  }

  init();
})();
