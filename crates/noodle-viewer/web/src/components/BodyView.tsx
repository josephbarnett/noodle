// Body rendering. Single responsibility: take an unknown payload from
// an Exchange and render it in a readable form.
//
// - Object → pretty-printed JSON (indented).
// - String that looks like SSE (`event:` / `data:` lines) → SSE-aware
//   monospace block.
// - Other string → monospace block.
// - null / undefined → "—".

import { useState } from "react";

interface Props {
  body: unknown;
  /** Optional label for the copy button title (e.g. "request body"). */
  label?: string;
}

export function BodyView({ body, label }: Props) {
  if (body === null || body === undefined) {
    return <div className="body-empty">—</div>;
  }
  const copyText = bodyToText(body);
  return (
    <div className="body-view">
      {copyText !== null && <CopyButton text={copyText} label={label} />}
      {renderBody(body)}
    </div>
  );
}

function renderBody(body: unknown) {
  if (typeof body === "string") {
    if (looksLikeSse(body)) {
      return <SseBody raw={body} />;
    }
    return <pre className="body-pre">{body}</pre>;
  }
  // Object / array.
  return <pre className="body-pre">{JSON.stringify(body, null, 2)}</pre>;
}

/** Materialize the body to a string the user can paste anywhere. */
function bodyToText(body: unknown): string | null {
  if (body === null || body === undefined) return null;
  if (typeof body === "string") return body;
  return JSON.stringify(body, null, 2);
}

function CopyButton({ text, label }: { text: string; label?: string }) {
  const [copied, setCopied] = useState(false);
  const onClick = async () => {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1200);
    } catch {
      // Clipboard API can fail on insecure contexts / older browsers.
      // Fallback: select-and-copy a hidden textarea.
      const ta = document.createElement("textarea");
      ta.value = text;
      ta.style.position = "fixed";
      ta.style.opacity = "0";
      document.body.appendChild(ta);
      ta.select();
      try {
        document.execCommand("copy");
        setCopied(true);
        window.setTimeout(() => setCopied(false), 1200);
      } finally {
        document.body.removeChild(ta);
      }
    }
  };
  return (
    <button
      className={`body-copy-btn${copied ? " copied" : ""}`}
      type="button"
      onClick={onClick}
      title={label ? `Copy ${label}` : "Copy body"}
    >
      {copied ? "✓ Copied" : "⧉ Copy"}
    </button>
  );
}

function looksLikeSse(s: string): boolean {
  return /(^|\n)(event:|data:)/.test(s.slice(0, 1024));
}

function SseBody({ raw }: { raw: string }) {
  const frames = raw.split(/\n\n/).filter((s) => s.trim().length > 0);
  return (
    <div className="sse-frames">
      {frames.map((f, i) => (
        <pre key={i} className="sse-frame">
          {f}
        </pre>
      ))}
    </div>
  );
}
