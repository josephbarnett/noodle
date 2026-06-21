#!/usr/bin/env python3
"""Turn-over-turn TIMELINE — one screen for an extended test run.

Lists every turn across the DB in time order (local time, to match a
hand-kept test log), with its round-trip count, role/depth shape, last
stop_reason, and whether it composed into a telemetry_event. A summary
line calls out the health signals that matter over a long run:

  • complete-but-uncomposed turns  → the composer/cursor wedge signal
  • open turns                     → in-flight or abandoned (no terminal)
  • side-calls                     → background, correctly turn-less

Usage
-----
    python3 tools/cz_diag_timeline.py
    python3 tools/cz_diag_timeline.py --db /path/to/cloudzero.db
    python3 tools/cz_diag_timeline.py --since "2026-06-16 09:00"   # local time

Default DB: ~/Library/Application Support/CloudZero/cloudzero.db
"""
import argparse
import datetime as dt
import json
import os
import pathlib
import sqlite3
import sys

DEFAULT_DB = os.path.expanduser(
    "~/Library/Application Support/CloudZero/cloudzero.db"
)
CONTINUE_STOPS = ("tool_use", "pause_turn")  # do NOT close a turn


def connect(path):
    if not os.path.exists(path):
        sys.exit(f"no DB at {path}")
    con = sqlite3.connect(pathlib.Path(path).as_uri() + "?mode=ro", uri=True)
    con.row_factory = sqlite3.Row
    return con


def local(epoch):
    return dt.datetime.fromtimestamp(epoch).strftime("%H:%M:%S")


def shape(records):
    """Compact role×count, e.g. 'main×2,sub_agent×3'."""
    counts = {}
    for r in records:
        counts[r["role"]] = counts.get(r["role"], 0) + 1
    return ",".join(f"{k}×{v}" for k, v in counts.items())


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--db", default=DEFAULT_DB)
    ap.add_argument("--since", default=None,
                    help="only turns at/after this LOCAL time (e.g. '2026-06-16 09:00')")
    args = ap.parse_args()

    con = connect(args.db)
    floor = 0
    if args.since:
        try:
            floor = int(dt.datetime.strptime(args.since, "%Y-%m-%d %H:%M").timestamp())
        except ValueError:
            sys.exit("--since must look like '2026-06-16 09:00' (local time)")

    rows = con.execute(
        """SELECT session_id, turn_id, role, depth, response_stop_reason, epoch
             FROM round_trip_records
            WHERE role IS NOT NULL AND epoch >= ?
            ORDER BY epoch""",
        (floor,),
    ).fetchall()
    if not rows:
        print("No marked round trips yet (run this build, send a prompt).")
        return

    # Build ordered units: turns (grouped by turn_id) and standalone side-calls.
    units = []          # (sort_epoch, kind, key, session, records)
    turns = {}
    for r in rows:
        if r["role"] == "side_call" or not r["turn_id"]:
            units.append((r["epoch"], "side", "", r["session_id"], [r]))
        else:
            if r["turn_id"] not in turns:
                turns[r["turn_id"]] = {"session": r["session_id"],
                                       "first": r["epoch"], "recs": []}
                units.append((r["epoch"], "turn", r["turn_id"], r["session_id"], None))
            turns[r["turn_id"]]["recs"].append(r)

    print("CloudZero correlation — TIMELINE (turn over turn)")
    print(f"DB: {args.db}   (times local)\n")
    print(f"{'time':8} {'session':12} {'turn / kind':18} {'RTs':>3} "
          f"{'shape':16} {'last stop':10} composed")

    composed = uncomposed_complete = open_turns = sidecalls = 0
    for sort_epoch, kind, key, session, recs in sorted(units, key=lambda u: u[0]):
        sess = (session or "?")[:12]
        if kind == "side":
            sidecalls += 1
            print(f"{local(sort_epoch):8} {sess:12} {'(side_call)':18} "
                  f"{1:>3} {'side':16} {recs[0]['response_stop_reason'] or '?':10} n/a")
            continue
        recs = turns[key]["recs"]
        n = len(recs)
        last_stop = recs[-1]["response_stop_reason"]
        is_open = last_stop in CONTINUE_STOPS
        evs = con.execute(
            "SELECT provider_metadata FROM telemetry_events WHERE turn_id = ?",
            (key,),
        ).fetchall()
        if len(evs) == 1:
            try:
                rtc = json.loads(evs[0]["provider_metadata"] or "{}").get("round_trip_count")
            except (TypeError, ValueError):
                rtc = None
            status = f"yes (rtc={rtc}{' ✓' if rtc == n else ' ✗MISMATCH'})"
            composed += 1
        elif len(evs) > 1:
            status = f"DUP ({len(evs)} events)"
        elif is_open:
            status = "no (open — not done)"
            open_turns += 1
        else:
            status = "NO — complete but uncomposed"
            uncomposed_complete += 1
        print(f"{local(turns[key]['first']):8} {sess:12} {key[:18]:18} "
              f"{n:>3} {shape(recs):16} {last_stop or '?':10} {status}")

    print()
    print(f"summary: {len(turns)} turn(s) — {composed} composed, "
          f"{uncomposed_complete} complete-but-uncomposed, {open_turns} open; "
          f"{sidecalls} side-call(s).")
    if uncomposed_complete:
        print("  ⚠ complete-but-uncomposed > 0 → composer/cursor WEDGE. "
              "Investigate the embellisher.")
    else:
        print("  ✓ no wedge: every complete turn composed; cursor advanced turn-over-turn.")


if __name__ == "__main__":
    main()
