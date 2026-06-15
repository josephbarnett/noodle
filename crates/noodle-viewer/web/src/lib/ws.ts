// WebSocket client with simple reconnect.
// Single responsibility: keep a connection open and pass parsed
// messages to a handler. Reconnect on close after a short backoff.

import type { ServerMsg } from "../types";

export interface WsClientOpts {
  url: string;
  onMessage: (msg: ServerMsg) => void;
  onConnected?: () => void;
  onDisconnected?: () => void;
}

export class WsClient {
  private opts: WsClientOpts;
  private ws: WebSocket | null = null;
  private closed = false;
  private backoff = 250;

  constructor(opts: WsClientOpts) {
    this.opts = opts;
    this.connect();
  }

  close(): void {
    this.closed = true;
    this.ws?.close();
  }

  private connect(): void {
    const ws = new WebSocket(this.opts.url);
    this.ws = ws;
    ws.onopen = () => {
      this.backoff = 250;
      this.opts.onConnected?.();
    };
    ws.onmessage = (e) => {
      try {
        const msg = JSON.parse(e.data as string) as ServerMsg;
        this.opts.onMessage(msg);
      } catch (err) {
        console.warn("ws: ignoring unparseable message", err);
      }
    };
    ws.onerror = () => {
      // onclose will follow; let it handle reconnect.
    };
    ws.onclose = () => {
      this.opts.onDisconnected?.();
      if (!this.closed) {
        setTimeout(() => this.connect(), this.backoff);
        this.backoff = Math.min(this.backoff * 2, 5000);
      }
    };
  }
}
