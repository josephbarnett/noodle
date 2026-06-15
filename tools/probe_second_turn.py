"""Fail-before probe: run the EXACT ADR-052 §6 loop (copied verbatim from
tools/validate_frame_tree.py done()) over a session with TWO user turns.

Each RT descriptor uses the same fields validate_frame_tree.py builds from the
wire: utext (user text), trs (tool_result ids in request), tus (response
tool_use ids), spawns, stop, mt, new_user, session_wrap.

Turn 1: a ROOT prompt -> Bash tool_use -> Bash result -> end_turn  (RT1-2)
Turn 2: a fresh user prompt -> end_turn                            (RT3)

The second user turn (RT3) carries genuine new user text, mt!=1, no <session>,
and NO tool_result (turn 1 ended terminal, nothing pending). This is exactly
how Claude Code sends a 2nd prompt. Watch where the algorithm puts it.
"""

def run(rts):
    frames={}; pending_tu={}; pending_spawn={}
    root_set=False; TERM={"end_turn","max_tokens","stop_sequence"}
    turn=0; in_turn=False
    for i,rt in enumerate(rts,1):
        frame=None; role="side-call"
        ans=[tu for tu in rt["trs"] if tu in pending_tu]
        if ans:
            frame=pending_tu[ans[0]]; role="main" if frame=="ROOT" else "sub-agent"
        if frame is None:
            for key,(sid,par) in list(pending_spawn.items()):
                if key and key in rt["utext"]:
                    frame=sid; frames[frame]={"parent":par,"depth":frames[par]["depth"]+1}
                    role="sub-agent"; del pending_spawn[key]; break
        if frame is None and not root_set and rt["new_user"] and rt["mt"]!=1 and not rt["session_wrap"]:
            frame="ROOT"; frames["ROOT"]={"parent":None,"depth":0}; root_set=True; role="main"
        if frame=="ROOT" and not in_turn: turn+=1; in_turn=True
        tno = turn if frame is not None else "-"
        print(f"RT{i} {role:9} frame={(frame or '—'):6} turn={tno}  ({rt['note']})")
        for tu_id,name in rt["tus"]: pending_tu[tu_id]=frame
        for sid,pr in rt["spawns"]: pending_spawn[pr]=(sid,frame)
        for tu in rt["trs"]: pending_tu.pop(tu,None)
        if frame=="ROOT" and rt["stop"] in TERM:
            open_children=any(v=="ROOT" or (v in frames) for k,v in pending_tu.items())
            if not open_children: in_turn=False; print(f"   └─ TURN {turn} ENDS")

T1_RT1={"note":"turn1 prompt -> Bash","utext":"please run ls","trs":[],"tus":[("bash1","Bash")],"spawns":[],"stop":"tool_use","mt":64000,"new_user":True,"session_wrap":False}
T1_RT2={"note":"turn1 bash result -> end_turn","utext":"please run ls","trs":["bash1"],"tus":[],"spawns":[],"stop":"end_turn","mt":64000,"new_user":False,"session_wrap":False}
T2_RT3={"note":"turn2 NEW user prompt -> end_turn","utext":"now explain the result","trs":[],"tus":[],"spawns":[],"stop":"end_turn","mt":64000,"new_user":True,"session_wrap":False}

print("=== TWO-TURN SESSION through the verbatim §6 loop ===")
run([T1_RT1,T1_RT2,T2_RT3])
