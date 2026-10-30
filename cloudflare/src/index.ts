// aperture relay on Cloudflare: serves the viewer, and routes each room's
// signalling to its Durable Object. Drop-in replacement for the self-hosted
// `relay` binary — same URLs, same JSON messages.

import { Room } from "./room";
import {
  INDEX_MARKER,
  iceServers,
  roomFrom,
  validRoom,
  type Env,
} from "./shared";

export { Room };

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);

    if (url.pathname === "/health") {
      return page("ok", { "content-type": "text/plain; charset=utf-8" });
    }

    if (url.pathname.startsWith("/ws/")) {
      const room = roomFrom(url.pathname, "/ws/");
      if (!validRoom(room)) return page("unknown room", { status: 404 });
      return env.ROOM.get(env.ROOM.idFromName(room)).fetch(request);
    }

    if (url.pathname.startsWith("/r/")) {
      const room = roomFrom(url.pathname, "/r/");
      if (!validRoom(room)) return page("unknown room", { status: 404 });
      return viewer(env, url, room);
    }

    if (url.pathname === "/") {
      return page(landing(url.origin), { "content-type": "text/html; charset=utf-8" });
    }

    return env.ASSETS.fetch(request);
  },
} satisfies ExportedHandler<Env>;

async function viewer(env: Env, url: URL, room: string): Promise<Response> {
  const asset = await env.ASSETS.fetch(new Request(new URL("/index.html", url)));
  if (!asset.ok) return page("viewer assets missing — run `npm run sync`", { status: 500 });

  const html = await asset.text();
  const script =
    `<script>window.APERTURE_ICE=${JSON.stringify(iceServers(env))};` +
    `window.APERTURE_ROOM=${JSON.stringify(room)};</script>`;

  return page(html.replace(INDEX_MARKER, () => script));
}

function page(
  body: string,
  init: { "content-type"?: string; status?: number } = {}
): Response {
  return new Response(body, {
    status: init.status ?? 200,
    headers: {
      "content-type": init["content-type"] ?? "text/html; charset=utf-8",
      "cache-control": "no-store",
    },
  });
}

function landing(origin: string): string {
  return `<!doctype html><meta charset=utf-8>
<meta name=viewport content="width=device-width,initial-scale=1">
<title>Aperture relay</title>
<style>body{background:#0a0c0a;color:#e6eadf;font:15px/1.6 ui-monospace,Menlo,Consolas,monospace;
margin:0;padding:14vh 24px;max-width:38rem}h1{font-size:20px;letter-spacing:.24em;color:#c8f751}
a{color:#c8f751}code{color:#e6eadf}</style>
<h1>APERTURE RELAY</h1>
<p>Run the agent with <code>--relay ${origin}</code> and it prints a share link
like <code>${origin}/r/&lt;room&gt;?t=&lt;token&gt;</code>.</p>
<p>Send that link to a friend: the page opens in the browser, connects peer-to-peer,
and the token never reaches this relay.</p>`;
}
