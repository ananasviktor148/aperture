// One Durable Object per room: holds the agent's host socket plus the guest
// sockets, and relays SDP/ICE signalling between them. Video never passes
// through here — it flows peer-to-peer over the DataChannel.
//
// Connection state lives in each socket's attachment, not in memory, so the
// object can hibernate between messages and cost no duration while idle.

import { DurableObject } from "cloudflare:workers";
import {
  KEEPALIVE,
  KEEPALIVE_ACK,
  randomId,
  roomFrom,
  validRoom,
  type Env,
} from "./shared";

type Role = "host" | "guest";

interface Attachment {
  role: Role;
  id: string;
  /** Set when a newer agent takes the room over, so the close stays quiet. */
  retired?: boolean;
}

interface Peers {
  host: WebSocket | null;
  guests: Map<string, WebSocket>;
}

export class Room extends DurableObject<Env> {
  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    // The edge answers this pair itself, so an idle link stays open without
    // ever waking the object.
    this.ctx.setWebSocketAutoResponse(
      new WebSocketRequestResponsePair(KEEPALIVE, KEEPALIVE_ACK)
    );
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const room = roomFrom(url.pathname, "/ws/");
    if (!validRoom(room)) return plain("unknown room", 404);

    const upgrade = url.searchParams.get("role") === "host" ? "host" : "guest";
    const peers = this.peers();

    // A crashed agent must not lock the room until the edge notices the dead
    // socket: the newest host wins.
    if (upgrade === "host" && peers.host) {
      const stale = peers.host;
      stale.serializeAttachment({ role: "host", id: "", retired: true } satisfies Attachment);
      this.closeSocket(stale, 1000, "replaced");
      peers.host = null;
    }

    const [client, server] = Object.values(new WebSocketPair());
    const id = upgrade === "host" ? "" : randomId();
    server.serializeAttachment({ role: upgrade, id } satisfies Attachment);
    this.ctx.acceptWebSocket(server);

    if (upgrade === "host") {
      for (const guest of peers.guests.values()) this.send(guest, { type: "host-joined" });
    } else {
      this.send(server, { type: "joined", id, host: Boolean(peers.host) });
      if (peers.host) this.send(peers.host, { type: "guest-joined", id });
    }

    return new Response(null, { status: 101, webSocket: client });
  }

  async webSocketMessage(ws: WebSocket, message: string | ArrayBuffer): Promise<void> {
    if (typeof message !== "string") return;

    const mine = this.attachment(ws);
    if (!mine || mine.retired) return;

    let msg: Record<string, unknown>;
    try {
      const parsed: unknown = JSON.parse(message);
      if (!parsed || typeof parsed !== "object") return;
      msg = parsed as Record<string, unknown>;
    } catch {
      return;
    }

    const peers = this.peers();
    if (mine.role === "host") {
      msg.from = "host";
      const to = typeof msg.to === "string" ? msg.to : "";
      if (to) {
        const guest = peers.guests.get(to);
        if (guest) this.send(guest, msg);
        return;
      }
      for (const guest of peers.guests.values()) this.send(guest, msg);
      return;
    }

    msg.from = mine.id;
    if (peers.host) this.send(peers.host, msg);
  }

  async webSocketClose(ws: WebSocket, code: number, _reason: string, _wasClean: boolean): Promise<void> {
    const mine = this.attachment(ws);
    this.closeSocket(ws, code);
    if (!mine || mine.retired) return;

    const peers = this.peers();
    if (mine.role === "host") {
      for (const guest of peers.guests.values()) {
        if (guest !== ws) this.send(guest, { type: "host-left" });
      }
    } else if (peers.host && peers.host !== ws) {
      this.send(peers.host, { type: "guest-left", id: mine.id });
    }
  }

  async webSocketError(ws: WebSocket, _errorKnown: unknown): Promise<void> {
    await this.webSocketClose(ws, 1006, "", false);
  }

  private peers(): Peers {
    let host: WebSocket | null = null;
    const guests = new Map<string, WebSocket>();

    for (const ws of this.ctx.getWebSockets()) {
      const attachment = this.attachment(ws);
      if (!attachment) {
        this.closeSocket(ws, 1011, "lost connection state");
        continue;
      }
      if (attachment.retired) continue;
      if (attachment.role === "host") host = ws;
      else guests.set(attachment.id, ws);
    }

    return { host, guests };
  }

  private attachment(ws: WebSocket): Attachment | null {
    try {
      return ws.deserializeAttachment() as Attachment | null;
    } catch {
      return null;
    }
  }

  private send(ws: WebSocket, message: unknown): void {
    try {
      ws.send(JSON.stringify(message));
    } catch {
      // Peer already gone; the close handler cleans up.
    }
  }

  private closeSocket(ws: WebSocket, code: number, reason?: string): void {
    try {
      ws.close(code, reason);
    } catch {
      // Already closed.
    }
  }
}

function plain(body: string, status: number): Response {
  return new Response(body, {
    status,
    headers: { "content-type": "text/plain; charset=utf-8", "cache-control": "no-store" },
  });
}
