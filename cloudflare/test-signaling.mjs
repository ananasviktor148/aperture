// Signalling regression harness: run `npm run dev` in another terminal, then
// `npm test`. Mirrors the message contract of src/bin/relay.rs.

const BASE = process.env.RELAY_URL || "http://127.0.0.1:8787";
const ROOM = "testroom01";
const WS = BASE.replace(/^http/, "ws");

const open = [];
let failures = 0;
let checks = 0;

function ok(label, condition, detail) {
  checks += 1;
  if (condition) {
    console.log(`  ok    ${label}`);
    return true;
  }
  failures += 1;
  console.log(`  FAIL  ${label}${detail === undefined ? "" : ` — got ${JSON.stringify(detail)}`}`);
  return false;
}

function connect(role, room = ROOM) {
  return new Promise((resolve, reject) => {
    const socket = new WebSocket(`${WS}/ws/${room}?role=${role}`);
    open.push(socket);
    const inbox = [];
    const waiters = [];
    let closed = null;

    socket.addEventListener("message", (event) => {
      const data = String(event.data);
      let parsed;
      try {
        parsed = JSON.parse(data);
      } catch {
        parsed = data === "aperture-keepalive-ack" ? { type: "ack" } : { type: "raw", data };
      }
      const waiter = waiters.shift();
      if (waiter) waiter(parsed);
      else inbox.push(parsed);
    });
    socket.addEventListener("close", (event) => {
      closed = { type: "closed", code: event.code, reason: event.reason };
      const waiter = waiters.shift();
      if (waiter) waiter(closed);
      else inbox.push(closed);
    });
    socket.addEventListener("error", (event) => reject(new Error(`ws error: ${event.message || "unknown"}`)));
    socket.addEventListener("open", () =>
      resolve({
        socket,
        send(object) {
          socket.send(JSON.stringify(object));
        },
        sendRaw(text) {
          socket.send(text);
        },
        // Waits for the next frame; rejects if none arrives in time.
        receive(timeout = 4000) {
          if (inbox.length) return Promise.resolve(inbox.shift());
          if (closed) return Promise.resolve(closed);
          return new Promise((resolveWait, rejectWait) => {
            const timer = setTimeout(() => {
              const index = waiters.indexOf(onFrame);
              if (index >= 0) waiters.splice(index, 1);
              rejectWait(new Error("no message"));
            }, timeout);
            function onFrame(value) {
              clearTimeout(timer);
              resolveWait(value);
            }
            waiters.push(onFrame);
          });
        },
        async quiet(timeout = 900) {
          try {
            return { leaked: await this.receive(timeout) };
          } catch {
            return { leaked: null };
          }
        },
      })
    );
  });
}

async function http(path) {
  const response = await fetch(`${BASE}${path}`);
  return {
    status: response.status,
    body: await response.text(),
    cache: response.headers.get("cache-control") || "",
    type: response.headers.get("content-type") || "",
  };
}

async function main() {
  console.log(`\nrelay smoke test against ${BASE}\n`);

  const health = await http("/health");
  ok("/health returns ok", health.status === 200 && health.body === "ok", health.status);

  const index = await http(`/r/${ROOM}?t=sometoken`);
  ok(
    "/r/:room injects room + ICE servers",
    index.status === 200 &&
      index.body.includes(`window.APERTURE_ROOM="${ROOM}"`) &&
      index.body.includes("stun:stun.l.google.com:19302") &&
      !index.body.includes("<!--APERTURE_ICE-->"),
    index.body.slice(0, 160)
  );
  ok("/r/:room is not cacheable", /no-store/.test(index.cache), index.cache);
  ok("viewer assets are served", (await http("/app.js")).body.includes("createRtcTransport"));
  ok("/r/:room rejects an invalid room", (await http("/r/no!")).status === 404);
  ok("/ws rejects an invalid room", await rejectsHandshake("bad!"));

  console.log("\nmessaging\n");

  const host = await connect("host");
  const guest = await connect("guest");

  const joined = await guest.receive();
  ok(
    "guest gets joined with a host present",
    joined.type === "joined" && joined.host === true && /^[a-z0-9]{8}$/.test(joined.id || ""),
    joined
  );
  const guestId = joined.id;

  const guestJoined = await host.receive();
  ok("host is told about the guest", guestJoined.type === "guest-joined" && guestJoined.id === guestId, guestJoined);

  guest.send({ type: "offer", sdp: "v=0-offer" });
  const offer = await host.receive();
  ok("offer is relayed and stamped with from", offer.type === "offer" && offer.from === guestId, offer);

  host.send({ type: "answer", sdp: "v=0-answer", to: guestId });
  const answer = await guest.receive();
  ok(
    "directed answer reaches only that guest",
    answer.type === "answer" && answer.from === "host" && answer.sdp === "v=0-answer",
    answer
  );

  host.send({ type: "ice", candidate: { candidate: "a" } });
  const ice = await guest.receive();
  ok("undirected host message broadcasts", ice.type === "ice" && ice.from === "host", ice);

  guest.sendRaw("aperture-keepalive");
  ok("relay answers the keepalive itself", (await guest.receive()).type === "ack");
  ok("keepalive never reaches the host", (await host.quiet()).leaked === null);

  const late = await connect("guest");
  const lateJoined = await late.receive();
  ok("second guest is accepted", lateJoined.type === "joined" && lateJoined.host === true, lateJoined);
  ok("second guest id differs", lateJoined.id !== guestId, lateJoined.id);

  host.send({ type: "answer", sdp: "for-both", to: lateJoined.id });
  ok("host can address a specific guest", (await late.receive()).sdp === "for-both");
  ok("the addressed guest is the only receiver", (await guest.quiet()).leaked === null);

  const lateNotice = await host.receive();
  ok("host learns of the second guest", lateNotice.type === "guest-joined" && lateNotice.id === lateJoined.id, lateNotice);

  console.log("\ndepartures\n");

  guest.socket.close(1000, "bye");
  const left = await host.receive();
  ok("host is told the guest left", left.type === "guest-left" && left.id === guestId, left);

  late.socket.close(1000, "bye");
  const leftAgain = await host.receive();
  ok("each guest departure is announced", leftAgain.type === "guest-left" && leftAgain.id === lateJoined.id, leftAgain);

  host.socket.close(1000, "bye");
  const orphaned = await (await connect("guest")).receive();
  ok("guest with no host waits", orphaned.type === "joined" && orphaned.host === false, orphaned);

  console.log("\nhost takeover\n");

  const first = await connect("host");
  const second = await connect("host");
  const waiting = await connect("guest");
  const waitingJoined = await waiting.receive();
  ok("guest sees the surviving host", waitingJoined.host === true, waitingJoined);

  ok("stale host stops receiving traffic", (await first.quiet(1200)).leaked === null);
  const newcomer = await second.receive();
  ok("new host is told about the guest", newcomer.type === "guest-joined", newcomer);

  first.send({ type: "ice", candidate: { candidate: "stale" } });
  ok("retired host cannot reach the guest", (await waiting.quiet()).leaked === null);

  second.send({ type: "ice", candidate: { candidate: "fresh" } });
  const fresh = await waiting.receive();
  ok("new host relays to guests", fresh.type === "ice" && fresh.from === "host", fresh);

  second.socket.close(1000, "bye");
  const gone = await waiting.receive();
  ok("host departure is announced", gone.type === "host-left", gone);

  console.log(`\n${checks - failures}/${checks} checks passed`);

  for (const socket of open) {
    try {
      socket.close();
    } catch {
      /* already gone */
    }
  }
  // Half-closed test sockets would otherwise keep the event loop busy.
  await new Promise((resolve) => setTimeout(resolve, 250));
  process.exitCode = failures ? 1 : 0;
}

// A rejected handshake surfaces as an error/close without ever opening.
function rejectsHandshake(room) {
  return new Promise((resolve) => {
    const socket = new WebSocket(`${WS}/ws/${room}?role=guest`);
    socket.addEventListener("open", () => resolve(false));
    socket.addEventListener("error", () => resolve(true));
    socket.addEventListener("close", () => resolve(true));
  });
}

main().catch((error) => {
  console.error("\nharness error:", error.message);
  process.exitCode = 1;
});
