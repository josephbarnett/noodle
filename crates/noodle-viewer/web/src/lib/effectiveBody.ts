// Pick the "attribution-debugging view" of an Exchange's body —
// the side that has noodle-touched data visible.
//
// Wire model (see `types.ts`):
//   - `body`     = bytes noodle received (pre-mutation)
//   - `body_out` = bytes noodle forwarded (post-mutation), present
//                  only when distinct from `body`.
//
// Symmetric "what noodle did" rule, applied per direction:
//
//   Request  → body_out   (post-injection — directive lives HERE,
//                          not in what the client sent)
//   Response → body_in    (pre-strip — the model's marker lives
//                          HERE, before we deleted it for the client)
//
// Both directions surface the side that carries the proxy's
// interesting bytes. For the request side this means "what we
// injected"; for the response side this means "what the model
// emitted before we stripped." The other view (request body_in =
// client's original, response body_out = client-received) is the
// "transparent passthrough" view and is available via the raw
// `body` / `body_out` fields on the Exchange when needed (e.g.
// the row detail panel renders both side-by-side).
//
// Returns `undefined` when both fields are absent.

import type { Exchange } from "../types";

export function effectiveBody(ex: Exchange | undefined): unknown {
  if (!ex) return undefined;
  if (ex.direction === "request") {
    return ex.body_out ?? ex.body;
  }
  // Response: prefer the pre-mutation view so attribution markers
  // are visible. body is always present; body_out exists only when
  // noodle stripped something.
  return ex.body ?? ex.body_out;
}

/** True when noodle modified the bytes on this direction. */
export function wasMutated(ex: Exchange | undefined): boolean {
  return ex?.body_out !== undefined;
}
