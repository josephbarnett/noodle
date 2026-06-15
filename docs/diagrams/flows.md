# noodle flow diagrams

Five mermaid diagrams that together describe how noodle works. Drawio files
in this directory cover the same material with more visual detail; this
file is the LLM-friendly text version.

## 1. Hexagonal component view

The whole architecture in one picture. Domain core in the middle, four
driven ports on the boundary, driving adapter at the top, driven adapters
on the outside.

```mermaid
flowchart TB
    subgraph Driving["Driving Adapter (noodle-proxy)"]
        Rama["rama service stack<br/>HTTP CONNECT · TLS MITM · h1+h2"]
    end

    subgraph Core["Domain Core (noodle-core)"]
        Engine["InspectionEngine<br/>+ NormalizedEvent · Session · TurnId"]
    end

    subgraph Ports["Driven Ports (traits in noodle-core)"]
        P1["LlmAdapter"]
        P2["TagPolicy"]
        P3["SessionStore"]
        P4["AuditSink"]
    end

    subgraph DrivenAdapters["Driven Adapters (noodle-adapters)"]
        A1["OpenAiAdapter<br/>AnthropicAdapter<br/>WsAdapter"]
        A2["DefaultTagPolicy<br/>PolicyChain"]
        A3["InMemorySessionStore<br/>(future: Redis)"]
        A4["TracingSink · JsonLinesSink<br/>MultiAuditSink (fan-out)"]
    end

    Rama -->|InspectionPipeline| Engine
    Engine --> P1 & P2 & P3 & P4
    P1 -.implements.-> A1
    P2 -.implements.-> A2
    P3 -.implements.-> A3
    P4 -.implements.-> A4

    classDef core fill:#d5e8d4,stroke:#82b366
    classDef port fill:#fff2cc,stroke:#d6b656
    classDef adapter fill:#dae8fc,stroke:#6c8ebf
    class Engine core
    class P1,P2,P3,P4 port
    class Rama,A1,A2,A3,A4 adapter
```

## 2. Request lifecycle

End-to-end on a streaming chat completion. The driving adapter (rama)
brackets the call; the engine + ports do the work in between.

```mermaid
sequenceDiagram
    participant Agent
    participant Proxy as noodle-proxy<br/>(rama service)
    participant Engine as InspectionEngine
    participant Reg as AdapterRegistry
    participant Adp as LlmAdapter<br/>(OpenAI)
    participant Pol as TagPolicy
    participant Store as SessionStore
    participant Audit as AuditSink
    participant LLM as Upstream LLM

    Agent->>Proxy: CONNECT api.openai.com:443
    Note over Proxy: TLS MITM (boring)<br/>terminate + re-originate
    Agent->>Proxy: POST /v1/chat (TLS-wrapped)

    Proxy->>Engine: process_request(probe, body)
    Engine->>Reg: select(probe)
    Reg-->>Engine: OpenAiAdapter
    Engine->>Store: get_or_init(session_id)
    Store-->>Engine: Session
    Engine->>Adp: inject_directive(session, body)
    Adp-->>Engine: rewritten body
    Engine->>Audit: record(Inject)
    Engine-->>Proxy: rewritten request

    Proxy->>LLM: POST /v1/chat (with directive)
    LLM-->>Proxy: 200 text/event-stream

    Proxy->>Engine: process_response(parts, body)
    Engine->>Adp: decode(parts, body)
    Adp-->>Engine: stream<NormalizedEvent>
    loop per event
        Engine->>Pol: process(session, event)
        Pol-->>Engine: 0..n events (redacted)
        Engine->>Audit: record(TurnStart/Redact/TurnEnd)
    end
    Engine->>Adp: encode(parts, redacted_stream)
    Adp-->>Engine: BodyStream
    Engine-->>Proxy: redacted response

    Proxy-->>Agent: 200 text/event-stream (no markers)
```

## 3. Adapter selection (Factory pattern)

```mermaid
flowchart LR
    Req["Incoming Request"] --> Probe["build RequestProbe<br/>(method, uri, headers)"]
    Probe --> Reg{"AdapterRegistry::select"}
    Reg -->|matches OpenAI host| OAI["OpenAiAdapter"]
    Reg -->|matches Anthropic host| ANT["AnthropicAdapter"]
    Reg -->|matches WS upgrade| WS["WsAdapter"]
    Reg -->|no match| Pass["pass-through<br/>(no inspection, WARN)"]

    OAI --> Run["adapter.inject_directive<br/>adapter.decode/encode"]
    ANT --> Run
    WS --> Run
```

The registry is first-match-wins, registered in order at startup. Adding
a provider is exactly: write an `LlmAdapter`, register it in
`OrderedRegistry::builder()`. No central `match` statement to update.

## 4. Stream pipeline (decode → policy → encode)

The hot path for streaming responses. Pure stream combinators; no I/O
inside the policy step.

```mermaid
flowchart LR
    Body["BodyStream<br/>(raw bytes from upstream)"]
    Decode["adapter.decode<br/>(L4+L5 codec)"]
    Filter["policy_filter<br/>(Stream::flat_map)"]
    Encode["adapter.encode<br/>(re-emit raw bytes<br/>where unchanged)"]
    Out["BodyStream<br/>(redacted bytes to client)"]

    Body --> Decode --> Filter --> Encode --> Out

    Pol["TagPolicy::process<br/>SmallVec&lt;[Event; 1]&gt;"]
    Filter -.calls per event.-> Pol

    Audit["AuditSink::record"]
    Filter -.side-effect.-> Audit
```

Backpressure is preserved end-to-end because every step is a `Stream`
adapter; no buffering except inside the policy when it is mid-marker.

## 5. Session + turn lifecycle (state machine)

```mermaid
stateDiagram-v2
    [*] --> NoSession: first request

    NoSession --> Authorized: validate auth +<br/>x-noodle-session header
    NoSession --> Rejected: missing session header
    Rejected --> [*]: 400

    Authorized --> DirectiveInjected: adapter.inject_directive<br/>(first time only)
    DirectiveInjected --> InTurn: adapter.decode emits TurnStart
    Authorized --> InTurn: subsequent requests<br/>(directive already injected)

    InTurn --> InTurn: Token / ToolCall events<br/>(policy.process per event)
    InTurn --> Idle: TurnEnd
    Idle --> InTurn: next request in same session

    Idle --> [*]: session TTL expires<br/>or proxy shutdown
```

The `directive_injected` flag lives on the `Session` itself (an
`AtomicBool`), so the "inject once" semantic survives concurrent
requests.
