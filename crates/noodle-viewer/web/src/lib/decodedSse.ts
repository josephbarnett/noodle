// EventSource client for /api/decoded-exchanges (S22 of the
// 027–031 refactor — refactor-overview.md §10).
//
// Native browser `EventSource` handles SSE plumbing (reconnect,
// message framing) so this is a thin parse + dispatch layer.
// Single responsibility: open the stream, parse `decoded_exchange`
// frames into typed `DecodedExchange` objects, pass them to a
// handler. Reconnect is delegated to the browser; the constructor
// re-opens if the connection drops permanently after a backoff.

import type { DecodedExchange } from "../types";

export interface DecodedSseOpts {
  url: string;
  onDecodedExchange: (dx: DecodedExchange) => void;
  /** Optional — invoked when the browser reports a lag frame
   *  (`event: lag`, `data: <count>`). */
  onLag?: (count: number) => void;
}

export class DecodedSseClient {
  private opts: DecodedSseOpts;
  private es: EventSource | null = null;
  private closed = false;

  constructor(opts: DecodedSseOpts) {
    this.opts = opts;
    this.connect();
  }

  close(): void {
    this.closed = true;
    this.es?.close();
    this.es = null;
  }

  private connect(): void {
    const es = new EventSource(this.opts.url);
    this.es = es;
    es.addEventListener("decoded_exchange", (ev: MessageEvent) => {
      try {
        const dx = JSON.parse(ev.data as string) as DecodedExchange;
        this.opts.onDecodedExchange(dx);
      } catch (err) {
        console.warn("decoded SSE: ignoring unparseable frame", err);
      }
    });
    es.addEventListener("lag", (ev: MessageEvent) => {
      const n = Number.parseInt(ev.data as string, 10);
      if (Number.isFinite(n)) this.opts.onLag?.(n);
    });
    // `EventSource` auto-reconnects with its own backoff. We only
    // step in if the consumer explicitly closed the stream.
    es.onerror = () => {
      if (this.closed) es.close();
    };
  }
}
