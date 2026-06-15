export type Mode = "http" | "ooda" | "otlp";

interface Props {
  mode: Mode;
  onChange: (m: Mode) => void;
}

export function ModeSwitcher({ mode, onChange }: Props) {
  return (
    <div className="modes">
      <button
        className={mode === "http" ? "active" : ""}
        onClick={() => onChange("http")}
      >
        HTTP
      </button>
      <button
        className={mode === "ooda" ? "active" : ""}
        onClick={() => onChange("ooda")}
        title="OODA: agent ↔ LLM conversation reconstruction"
      >
        OODA
      </button>
      <button
        className={mode === "otlp" ? "active" : ""}
        onClick={() => onChange("otlp")}
        title="OTLP: ad-hoc SQL over the embellishment rollups.db"
      >
        OTLP
      </button>
    </div>
  );
}
