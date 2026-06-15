// S22 (refactor-overview.md §10): per-row badge surfacing
// `marks.turn_id`. Renders a short prefix of the turn id so a
// scanning operator can see when a turn starts / continues
// without taking the full ULID width. Click → copy to clipboard.
//
// `marks.turn_id` is a ULID (26 chars). The display takes the
// last 6 chars (the random tail — more visually distinctive than
// the leading timestamp portion, which collides across rows
// captured in the same second). `title` carries the full id.

import { useCallback } from "react";

interface Props {
  turnId: string | null | undefined;
  /** Optional className passthrough so callers can place the
   *  badge in their row layout. */
  className?: string;
}

export function TurnIdBadge({ turnId, className }: Props) {
  const onClick = useCallback(
    (e: React.MouseEvent) => {
      if (!turnId) return;
      e.stopPropagation();
      void navigator.clipboard?.writeText(turnId);
    },
    [turnId],
  );

  if (!turnId) return null;
  const short = turnId.length > 6 ? turnId.slice(-6) : turnId;
  return (
    <span
      className={`turn-id-badge${className ? " " + className : ""}`}
      title={`turn_id: ${turnId} (click to copy)`}
      onClick={onClick}
      role="button"
      tabIndex={-1}
    >
      turn:{short}
    </span>
  );
}
