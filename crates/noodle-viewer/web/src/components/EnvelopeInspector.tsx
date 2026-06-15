// S22 (refactor-overview.md §10): collapsible inspector panel for
// the typed `envelope.*` block on a [`DecodedExchange`]. Shows
// `agent_app`, `machine`, `collector_app`, `subscription` (each
// individually optional).
//
// The on-disk shape — and therefore this view — mirrors ADR 029
// §2.4 / S6 / S7. Inner fields are surfaced as a labeled
// key/value list; absent inner fields collapse out entirely so
// the panel only shows what the proxy observed.

import { useState } from "react";
import type { DecodedEnvelope } from "../types";

interface Props {
  envelope: DecodedEnvelope | null | undefined;
  /** Initial collapsed/expanded state. Defaults to collapsed. */
  defaultOpen?: boolean;
}

export function EnvelopeInspector({ envelope, defaultOpen = false }: Props) {
  const [open, setOpen] = useState(defaultOpen);
  if (!envelope || isEmpty(envelope)) return null;

  return (
    <section className={`envelope-inspector${open ? " open" : ""}`}>
      <button
        type="button"
        className="envelope-head"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
      >
        <span className="envelope-chev" aria-hidden="true">
          {open ? "▾" : "▸"}
        </span>
        <span className="envelope-title">envelope</span>
        <span className="envelope-summary">{summary(envelope)}</span>
      </button>
      {open && (
        <div className="envelope-body">
          {envelope.agent_app && (
            <Group title="agent_app">
              <KV label="name" value={envelope.agent_app.name} />
              <KV label="version" value={envelope.agent_app.version ?? "—"} />
              <KV label="source" value={envelope.agent_app.source} />
              {envelope.agent_app.build_hash && (
                <KV label="build_hash" value={envelope.agent_app.build_hash} />
              )}
              {envelope.agent_app.build_date && (
                <KV label="build_date" value={envelope.agent_app.build_date} />
              )}
            </Group>
          )}
          {envelope.machine && (
            <Group title="machine">
              <KV label="os_family" value={envelope.machine.os_family} />
              <KV label="architecture" value={envelope.machine.architecture} />
              {envelope.machine.hostname && (
                <KV label="hostname" value={envelope.machine.hostname} />
              )}
              {envelope.machine.os_version && (
                <KV label="os_version" value={envelope.machine.os_version} />
              )}
              {envelope.machine.locale && (
                <KV label="locale" value={envelope.machine.locale} />
              )}
              {envelope.machine.timezone && (
                <KV label="timezone" value={envelope.machine.timezone} />
              )}
            </Group>
          )}
          {envelope.collector_app && (
            <Group title="collector_app">
              <KV label="name" value={envelope.collector_app.name} />
              <KV label="version" value={envelope.collector_app.version} />
              <KV label="build_hash" value={envelope.collector_app.build_hash} />
              <KV label="build_date" value={envelope.collector_app.build_date} />
              {envelope.collector_app.features.length > 0 && (
                <KV
                  label="features"
                  value={envelope.collector_app.features.join(", ")}
                />
              )}
            </Group>
          )}
          {envelope.subscription && (
            <Group title="subscription">
              {envelope.subscription.api_key && (
                <>
                  <KV label="api_key.prefix" value={envelope.subscription.api_key.prefix} />
                  <KV label="api_key.kind" value={envelope.subscription.api_key.kind} />
                  <KV label="api_key.source" value={envelope.subscription.api_key.source} />
                </>
              )}
              {envelope.subscription.organization?.organization_id && (
                <KV
                  label="organization_id"
                  value={envelope.subscription.organization.organization_id}
                />
              )}
              {envelope.subscription.tier?.tier && (
                <KV label="tier" value={envelope.subscription.tier.tier} />
              )}
            </Group>
          )}
        </div>
      )}
    </section>
  );
}

function Group({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="envelope-group">
      <div className="envelope-group-title">{title}</div>
      <table className="envelope-table">
        <tbody>{children}</tbody>
      </table>
    </div>
  );
}

function KV({ label, value }: { label: string; value: string }) {
  return (
    <tr>
      <th>{label}</th>
      <td>{value}</td>
    </tr>
  );
}

function isEmpty(env: DecodedEnvelope): boolean {
  return (
    !env.agent_app && !env.machine && !env.collector_app && !env.subscription
  );
}

function summary(env: DecodedEnvelope): string {
  const parts: string[] = [];
  if (env.agent_app) parts.push(env.agent_app.name);
  if (env.machine) parts.push(`${env.machine.os_family}/${env.machine.architecture}`);
  if (env.collector_app) parts.push(env.collector_app.name);
  return parts.join(" · ");
}
