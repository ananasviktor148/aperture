export const KEEPALIVE = "aperture-keepalive";
export const KEEPALIVE_ACK = "aperture-keepalive-ack";

export const DEFAULT_STUN = "stun:stun.l.google.com:19302";
export const INDEX_MARKER = "<!--APERTURE_ICE-->";

export interface IceServer {
  urls: string[];
  username?: string;
  credential?: string;
}

// Structural so neither file has to import the other's class.
export interface RoomNamespace {
  idFromName(name: string): DurableObjectId;
  get(id: DurableObjectId): DurableObjectStub;
}

export interface Env {
  ROOM: RoomNamespace;
  ASSETS: Fetcher;
  /** Comma-separated STUN URLs. */
  STUN_URLS?: string;
  /** relay:host:3478?transport=udp — only needed for friends behind symmetric NAT. */
  TURN_URL?: string;
  TURN_USERNAME?: string;
  TURN_CREDENTIAL?: string;
}

export function iceServers(env: Env): IceServer[] {
  const stun = (env.STUN_URLS || DEFAULT_STUN)
    .split(",")
    .map((url) => url.trim())
    .filter(Boolean);

  const servers: IceServer[] = stun.length ? [{ urls: stun }] : [];
  if (env.TURN_URL) {
    servers.push({
      urls: [env.TURN_URL],
      username: env.TURN_USERNAME || "",
      credential: env.TURN_CREDENTIAL || "",
    });
  }
  return servers;
}

export function validRoom(room: string): boolean {
  return room.length >= 4 && room.length <= 64 && /^[A-Za-z0-9_-]+$/.test(room);
}

/** `/ws/<room>` or `/r/<room>` -> `<room>` ("" when the path has no segment). */
export function roomFrom(pathname: string, prefix: string): string {
  if (!pathname.startsWith(prefix)) return "";
  const rest = pathname.slice(prefix.length).replace(/\/+$/, "");
  if (!rest || rest.includes("/")) return "";
  try {
    return decodeURIComponent(rest);
  } catch {
    return "";
  }
}

export function randomId(): string {
  const alphabet = "abcdefghijklmnopqrstuvwxyz0123456789";
  const bytes = new Uint8Array(8);
  crypto.getRandomValues(bytes);
  let id = "";
  for (const byte of bytes) id += alphabet[byte % alphabet.length];
  return id;
}
