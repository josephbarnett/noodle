#!/usr/bin/env python3
"""What did we RECORD? — the DB side of design 018.

Reads the live CloudZero SQLite DB and shows, in plain language:
  1. that the schema is on the frame-marks model (v14, agent_run_id gone);
  2. for each session, the frame tree of every turn (main agent + the
     sub-agents it spawned), indented by depth;
  3. how those round trips rolled up into one telemetry_event per turn,
     and whether the rollup's round_trip_count matches what we recorded;
  4. the side-calls (titles / quota checks) that carry tokens but no turn.

It does NOT dump rows. Every line is labeled with what it means.

Usage
-----
    python3 tools/cz_diag_db.py
    python3 tools/cz_diag_db.py --db /path/to/cloudzero.db
    python3 tools/cz_diag_db.py --session 7d2dd766      # just one session

Default DB: ~/Library/Application Support/CloudZero/cloudzero.db
"""
import argparse
import json
import os
import pathlib
import sqlite3
import sys

DEFAULT_DB = os.path.expanduser(
    "~/Library/Application Support/CloudZero/cloudzero.db"
)


def short(s, n=14):
    """Shorten a long id for readable trees; keep it recognizable."""
    if not s:
        return ""
    return s if len(s) <= n else s[: n - 1] + "…"


def connect(path):
    if not os.path.exists(path):
        sys.exit(f"no DB at {path}\n(point --db at the file, or run the app first)")
    # as_uri() percent-encodes spaces — the default path
    # (~/Library/Application Support/…) contains one.
    con = sqlite3.connect(pathlib.Path(path).as_uri() + "?mode=ro", uri=True)
    con.row_factory = sqlite3.Row
    return con


def schema_line(con):
    v = con.execute("PRAGMA user_version").fetchone()[0]
    if v >= 14:
        note = "frame marks live, agent_run_id removed"
        flag = "OK"
    elif v == 13:
        note = "telemetry_events on marks; round_trip_records still has agent_run_id (pre-014)"
        flag = "OLD"
    else:
        note = "pre-018 schema — frame marks not present"
        flag = "OLD"
    return v, note, flag


def turn_event(con, turn_id):
    """The composed telemetry_event for a turn, if compose has run yet."""
    row = con.execute(
        """SELECT model, input_tokens, output_tokens, estimated_cost_usd,
                  provider_metadata
             FROM telemetry_events WHERE turn_id = ? LIMIT 1""",
        (turn_id,),
    ).fetchone()
    return row


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--db", default=DEFAULT_DB)
    ap.add_argument("--session", default=None,
                    help="only show sessions whose id starts with this")
    args = ap.parse_args()

    con = connect(args.db)
    v, note, flag = schema_line(con)

    print("CloudZero correlation — what we RECORDED")
    print(f"DB: {args.db}")
    print(f"schema: v{v}  ({note})  [{flag}]")
    print()

    # Pull every recorded round trip, ordered so a session reads top-to-bottom.
    rows = con.execute(
        """SELECT session_id, turn_id, frame_id, parent_frame_id, depth, role,
                  request_model, response_stop_reason, epoch
             FROM round_trip_records
            ORDER BY session_id, epoch"""
    ).fetchall()
    if not rows:
        print("No round_trip_records yet. Use the app (send a prompt), then re-run.")
        return

    # Rows with no role were written before this build stamped marks. They
    # cannot be read by the frame-tree model; set them aside with a summary
    # rather than mixing them into the trees (or crashing on NULL).
    premarks = [r for r in rows if r["role"] is None]
    marked = [r for r in rows if r["role"] is not None]
    if premarks:
        import datetime as _dt
        es = [r["epoch"] for r in premarks if r["epoch"]]
        span = ""
        if es:
            lo = _dt.datetime.fromtimestamp(min(es)).strftime("%Y-%m-%d")
            hi = _dt.datetime.fromtimestamp(max(es)).strftime("%Y-%m-%d")
            span = f"  ({lo} … {hi})"
        print(f"NOTE: {len(premarks)} pre-marks round trip(s) set aside{span} — "
              "written before this build; no frame marks to show.\n")
    if not marked:
        print("No round trips carry frame marks yet. Send a prompt with this "
              "build running, then re-run.")
        return

    # Group by session -> turn, in first-seen order.
    sessions = {}
    for r in marked:
        if args.session and not (r["session_id"] or "").startswith(args.session):
            continue
        sessions.setdefault(r["session_id"], {"turns": {}, "side": []})
        if r["role"] == "side_call" or not r["turn_id"]:
            sessions[r["session_id"]]["side"].append(r)
        else:
            sessions[r["session_id"]]["turns"].setdefault(r["turn_id"], []).append(r)

    if not sessions:
        print(f"No session matched --session {args.session!r}.")
        return

    print(f"Found {len(sessions)} session(s). A session = one agent conversation.")
    print("A TURN = one user prompt and everything done to answer it "
          "(incl. sub-agents).")
    print("A ROUND TRIP = one request/response to the model.\n")

    for sid, data in sessions.items():
        print(f"SESSION {short(sid, 20)}   "
              f"({len(data['turns'])} turn(s), {len(data['side'])} side-call(s))")
        for turn_id, rts in data["turns"].items():
            print(f"  TURN {short(turn_id, 20)}")
            print("    round trips (indented by depth = how deep in the "
                  "sub-agent tree):")
            for r in rts:
                d = r["depth"]
                indent = "      " + ("  " * (d if isinstance(d, int) and d > 0 else 0))
                who = {"main": "main agent",
                       "sub_agent": "sub-agent"}.get(r["role"], r["role"])
                parent = f"  parent={short(r['parent_frame_id'])}" if r["parent_frame_id"] else ""
                stop = f"  stop={r['response_stop_reason']}" if r["response_stop_reason"] else ""
                print(f"{indent}depth {d}  {who:10s}  frame={short(r['frame_id'])}{parent}{stop}")
            # Rollup reconciliation.
            ev = turn_event(con, turn_id)
            if ev is None:
                print("    rollup: (no turn event yet — the composer runs "
                      "on a short delay; re-run shortly)")
            else:
                rtc = None
                try:
                    rtc = json.loads(ev["provider_metadata"] or "{}").get("round_trip_count")
                except (TypeError, ValueError):
                    pass
                recorded = len(rts)
                match = "matches recorded round trips" if rtc == recorded \
                    else f"MISMATCH (recorded {recorded})"
                print(f"    rolled up → 1 turn event: model={ev['model']}  "
                      f"in={ev['input_tokens']} out={ev['output_tokens']} "
                      f"${ev['estimated_cost_usd']:.4f}  round_trip_count={rtc}  "
                      f"[{match}]")
            print()
        if data["side"]:
            print("  SIDE-CALLS (background: titles, quota, monitors — "
                  "real tokens, no turn):")
            for r in data["side"]:
                m = r["request_model"] or "?"
                print(f"    • {m}  stop={r['response_stop_reason'] or '?'}  "
                      f"(role={r['role']}, turn_id empty by design)")
            print()


if __name__ == "__main__":
    main()
