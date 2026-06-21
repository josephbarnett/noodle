#!/usr/bin/env python3
"""Are we WORKING or BROKEN? — the verdict for design 018.

Checks the recorded correlation against the rules the design guarantees,
and (if a TAP file is given) cross-checks the DB against the raw wire.
Each check prints PASS / FAIL / SKIP with one line on what it means, so a
FAIL tells you exactly which guarantee broke.

Checks (DB-internal — always run):
  1. SCHEMA      — DB is on the frame-marks model (v14).
  2. MAIN-ROOT   — every depth-0 round trip is the main agent at frame ROOT.
  3. NO-ORPHANS  — every parent_frame_id points at a frame that exists.
  4. FRAME-STABLE— a frame_id keeps the same parent and depth wherever it appears.
  5. SIDE-NO-TURN— side-calls carry no turn_id and no depth.
  6. ROLLUP      — each turn rolls up to exactly one event whose
                   round_trip_count matches the recorded round trips.

Cross-check (only with --tap):
  7. REAL-FRAMES — every sub-agent frame_id is a real tool_use id seen on
                   the wire (we never invent identity).

Usage
-----
    python3 tools/cz_diag_correlate.py
    python3 tools/cz_diag_correlate.py --tap ~/Library/Application\\ Support/CloudZero/tap.jsonl

Default DB: ~/Library/Application Support/CloudZero/cloudzero.db
"""
import argparse
import json
import os
import pathlib
import re
import sqlite3
import sys

DEFAULT_DB = os.path.expanduser(
    "~/Library/Application Support/CloudZero/cloudzero.db"
)

PASS, FAIL, SKIP = "PASS", "FAIL", "SKIP"


class Report:
    def __init__(self):
        self.rows = []

    def add(self, name, status, meaning, detail=""):
        self.rows.append((name, status, meaning, detail))

    def render(self):
        worst_ok = True
        for name, status, meaning, detail in self.rows:
            print(f"[{status:4}] {name:12} — {meaning}")
            if detail:
                print(f"            {detail}")
            if status == FAIL:
                worst_ok = False
        print()
        if worst_ok and any(s == PASS for _, s, _, _ in self.rows):
            print("VERDICT: working — every checked guarantee held.")
        elif not worst_ok:
            print("VERDICT: BROKEN — see the FAIL line(s) above.")
        else:
            print("VERDICT: inconclusive — not enough data yet (send a prompt, re-run).")


def connect(path):
    if not os.path.exists(path):
        sys.exit(f"no DB at {path}")
    # as_uri() percent-encodes spaces — the default path
    # (~/Library/Application Support/…) contains one.
    con = sqlite3.connect(pathlib.Path(path).as_uri() + "?mode=ro", uri=True)
    con.row_factory = sqlite3.Row
    return con


def check_schema(con, rep):
    v = con.execute("PRAGMA user_version").fetchone()[0]
    if v >= 14:
        rep.add("SCHEMA", PASS, "frame-marks schema (v14), agent_run_id removed")
    else:
        rep.add("SCHEMA", FAIL, "expected v14 frame-marks schema",
                f"found v{v} — rebuild/reinstall so migration 014 applies")


def check_main_root(con, rep):
    bad = con.execute(
        """SELECT count(*) FROM round_trip_records
            WHERE depth = 0 AND (role != 'main' OR frame_id != 'ROOT')"""
    ).fetchone()[0]
    if bad == 0:
        rep.add("MAIN-ROOT", PASS, "every depth-0 round trip is the main agent at ROOT")
    else:
        rep.add("MAIN-ROOT", FAIL, "a depth-0 round trip is not main/ROOT",
                f"{bad} offending row(s) — depth-0 must be the main frame")


def check_orphans(con, rep):
    rows = con.execute(
        """SELECT DISTINCT session_id, parent_frame_id FROM round_trip_records
            WHERE parent_frame_id IS NOT NULL AND parent_frame_id != ''"""
    ).fetchall()
    orphans = []
    for r in rows:
        exists = con.execute(
            """SELECT 1 FROM round_trip_records
                WHERE session_id = ? AND frame_id = ? LIMIT 1""",
            (r["session_id"], r["parent_frame_id"]),
        ).fetchone()
        if not exists:
            orphans.append((r["session_id"], r["parent_frame_id"]))
    if not orphans:
        rep.add("NO-ORPHANS", PASS, "every parent_frame_id points at a real frame")
    else:
        rep.add("NO-ORPHANS", FAIL, "a parent_frame_id has no matching frame",
                f"{len(orphans)} orphan(s), e.g. {orphans[0][1][:14]}")


def check_frame_stable(con, rep):
    rows = con.execute(
        """SELECT session_id, frame_id, parent_frame_id, depth
             FROM round_trip_records
            WHERE frame_id IS NOT NULL AND frame_id != ''"""
    ).fetchall()
    seen = {}
    bad = []
    for r in rows:
        key = (r["session_id"], r["frame_id"])
        sig = (r["parent_frame_id"], r["depth"])
        if key in seen and seen[key] != sig:
            bad.append(r["frame_id"])
        seen[key] = sig
    if not bad:
        rep.add("FRAME-STABLE", PASS,
                "each frame keeps one parent and depth throughout")
    else:
        rep.add("FRAME-STABLE", FAIL, "a frame changed parent or depth",
                f"unstable frame(s): {bad[0][:14]}")


def check_side_no_turn(con, rep):
    bad = con.execute(
        """SELECT count(*) FROM round_trip_records
            WHERE role = 'side_call'
              AND ((turn_id IS NOT NULL AND turn_id != '') OR depth IS NOT NULL)"""
    ).fetchone()[0]
    if bad == 0:
        rep.add("SIDE-NO-TURN", PASS,
                "side-calls carry no turn and no depth (as designed)")
    else:
        rep.add("SIDE-NO-TURN", FAIL, "a side-call wrongly carries a turn/depth",
                f"{bad} offending side-call row(s)")


# stop_reasons that DO NOT close a turn (more round trips will follow).
CONTINUE_STOPS = ("tool_use", "pause_turn")


def check_rollup(con, rep):
    # Scope to MARKED main turns only (role='main'); pre-marks rows have no
    # frame model and are judged elsewhere. A turn is "complete" when its last
    # round trip ended on a terminal stop_reason — only then should it compose.
    # The composer's round_trip_count = len(turnGroup.records) = ALL non-side
    # rows of the turn (main + sub_agent). Count the same denominator here.
    turns = con.execute(
        """SELECT turn_id, count(*) AS n FROM round_trip_records
            WHERE role != 'side_call' AND turn_id IS NOT NULL AND turn_id != ''
            GROUP BY turn_id"""
    ).fetchall()
    if not turns:
        rep.add("ROLLUP", SKIP, "no marked turns recorded yet (run this build, send a prompt)")
        return

    complete_ok, open_pending, problems = 0, 0, []
    for t in turns:
        last = con.execute(
            """SELECT response_stop_reason FROM round_trip_records
                WHERE turn_id = ? AND role = 'main'
                ORDER BY epoch DESC LIMIT 1""",
            (t["turn_id"],),
        ).fetchone()
        is_open = (last and last["response_stop_reason"] in CONTINUE_STOPS)
        evs = con.execute(
            "SELECT provider_metadata FROM telemetry_events WHERE turn_id = ?",
            (t["turn_id"],),
        ).fetchall()
        if is_open:
            if len(evs) == 0:
                open_pending += 1          # not done yet — correct to have no event
            else:
                problems.append(f"{t['turn_id'][:12]} composed while still open")
            continue
        # Complete turn: must have exactly one event with a matching count.
        if len(evs) != 1:
            problems.append(f"{t['turn_id'][:12]} complete but {len(evs)} event(s) "
                            "(composer didn't finalize it)")
            continue
        try:
            rtc = json.loads(evs[0]["provider_metadata"] or "{}").get("round_trip_count")
        except (TypeError, ValueError):
            rtc = None
        if rtc != t["n"]:
            problems.append(f"{t['turn_id'][:12]} count {rtc} != recorded {t['n']}")
        else:
            complete_ok += 1

    tail = f" ({open_pending} turn(s) still open — correctly uncomposed)" if open_pending else ""
    if problems:
        rep.add("ROLLUP", FAIL,
                "a COMPLETE turn did not roll up cleanly (composer/embellisher issue, not marks)",
                "; ".join(problems[:3]) + tail)
    elif complete_ok:
        rep.add("ROLLUP", PASS,
                f"all {complete_ok} complete turn(s) → one event, count matches{tail}")
    else:
        rep.add("ROLLUP", SKIP, f"no complete turns to judge yet{tail}")


def check_real_frames(con, rep, tap_path):
    if not tap_path:
        rep.add("REAL-FRAMES", SKIP, "pass --tap to cross-check frames against the wire")
        return
    if not os.path.exists(tap_path):
        rep.add("REAL-FRAMES", SKIP, f"no TAP file at {tap_path}")
        return
    # Collect every tool_use id that appears anywhere in the TAP.
    wire_ids = set()
    pat = re.compile(r'"id"\s*:\s*"(toolu_[A-Za-z0-9]+)"')
    with open(tap_path) as f:
        for line in f:
            wire_ids.update(pat.findall(line))
    frames = con.execute(
        """SELECT DISTINCT frame_id FROM round_trip_records
            WHERE frame_id IS NOT NULL AND frame_id NOT IN ('', 'ROOT')"""
    ).fetchall()
    invented = [r["frame_id"] for r in frames if r["frame_id"] not in wire_ids]
    if not frames:
        rep.add("REAL-FRAMES", SKIP, "no sub-agent frames recorded to check")
    elif not invented:
        rep.add("REAL-FRAMES", PASS,
                "every sub-agent frame is a real tool_use id from the wire")
    else:
        rep.add("REAL-FRAMES", FAIL, "a frame_id was not seen on the wire",
                f"{len(invented)} invented id(s), e.g. {invented[0][:18]}")


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--db", default=DEFAULT_DB)
    ap.add_argument("--tap", default=None,
                    help="TAP file to cross-check frame_ids against the wire")
    args = ap.parse_args()

    con = connect(args.db)
    print("CloudZero correlation — VERDICT")
    print(f"DB: {args.db}")
    pre = con.execute(
        "SELECT count(*) FROM round_trip_records WHERE role IS NULL"
    ).fetchone()[0]
    marked = con.execute(
        "SELECT count(*) FROM round_trip_records WHERE role IS NOT NULL"
    ).fetchone()[0]
    print(f"scope: {marked} marked round trip(s) judged; "
          f"{pre} pre-marks row(s) set aside (written before this build).\n")

    rep = Report()
    check_schema(con, rep)
    check_main_root(con, rep)
    check_orphans(con, rep)
    check_frame_stable(con, rep)
    check_side_no_turn(con, rep)
    check_rollup(con, rep)
    check_real_frames(con, rep, args.tap)
    rep.render()


if __name__ == "__main__":
    main()
