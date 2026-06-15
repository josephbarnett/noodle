import json
from mitmproxy import http
S={"rts":[]}
def response(flow):
    if "/v1/messages" not in flow.request.path: return
    try: body=json.loads(flow.request.get_text())
    except: body={}
    utext=[]; trs=[]
    for m in body.get("messages",[]):
        if m.get("role")=="user":
            c=m.get("content")
            if isinstance(c,str): utext.append(c)
            elif isinstance(c,list):
                for b in c:
                    if isinstance(b,dict):
                        if b.get("type")=="text": utext.append(b.get("text",""))
                        elif b.get("type")=="tool_result": trs.append(b.get("tool_use_id"))
    mt=body.get("max_tokens")
    new_user=False; session_wrap=False
    for m in reversed(body.get("messages",[])):
        if m.get("role")=="user":
            c=m.get("content"); txt=""
            if isinstance(c,str): txt=c
            elif isinstance(c,list):
                for b in c:
                    if isinstance(b,dict) and b.get("type")=="text": txt+=b.get("text","")
            if txt.strip(): new_user=True
            if txt.lstrip().startswith("<session>"): session_wrap=True
            break
    cb={}; stop=None
    for ln in (flow.response.get_text() or "").splitlines():
        if not ln.startswith("data:"): continue
        try: ev=json.loads(ln[5:].strip())
        except: continue
        t=ev.get("type")
        if t=="content_block_start":
            b=ev.get("content_block",{})
            if b.get("type")=="tool_use": cb[ev.get("index")]={"name":b.get("name"),"id":b.get("id"),"json":""}
        elif t=="content_block_delta":
            d=ev.get("delta",{})
            if d.get("type")=="input_json_delta" and ev.get("index") in cb: cb[ev.get("index")]["json"]+=d.get("partial_json","")
        elif t=="message_delta":
            sr=ev.get("delta",{}).get("stop_reason")
            if sr: stop=sr
    tus=[]; spawns=[]
    for idx,b in sorted(cb.items()):
        tus.append((b["id"],b["name"]))
        if b["name"] in ("Task","Agent"):
            try: inp=json.loads(b["json"])
            except: inp={}
            spawns.append((b["id"], str(inp.get("prompt",""))[:80]))
    S["rts"].append({"utext":"\n".join(utext),"trs":trs,"tus":tus,"spawns":spawns,"stop":stop,"mt":mt,"new_user":new_user,"session_wrap":session_wrap})
def done():
    rts=S["rts"]
    frames={}                 # id -> {parent,depth}
    pending_tu={}             # unanswered tool_use id -> emitting frame
    pending_spawn={}          # spawn prompt key -> (sid, parent_frame)
    root_set=False
    TERM={"end_turn","max_tokens","stop_sequence"}
    turn=0; in_turn=False
    print(f"\n=== CONNECTED-COMPONENT RECONSTRUCTION ({len(rts)} round-trips) ===")
    for i,rt in enumerate(rts,1):
        frame=None; role="side-call"
        # a) CHAIN FIRST: answers a pending tree tool_use -> the emitter frame
        ans=[tu for tu in rt["trs"] if tu in pending_tu]
        if ans:
            frame=pending_tu[ans[0]]; role="main" if frame=="ROOT" else "sub-agent"
        # b) OPEN a new sub-agent: fingerprint match CONSUMES the spawn (one-shot)
        if frame is None:
            for key,(sid,par) in list(pending_spawn.items()):
                if key and key in rt["utext"]:
                    frame=sid; frames[frame]={"parent":par,"depth":frames[par]["depth"]+1}
                    role="sub-agent"; del pending_spawn[key]; break
        # c) ROOT seed: first genuine user-input RT that is NOT a preamble probe
        #    (quota max_tokens==1, or a <session> title-gen). No tool_use required.
        if frame is None and not root_set and rt["new_user"] and rt["mt"]!=1 and not rt["session_wrap"]:
            frame="ROOT"; frames["ROOT"]={"parent":None,"depth":0}; root_set=True; role="main"
        # else side-call
        d = frames[frame]["depth"] if frame in frames else "-"
        par = frames[frame]["parent"] if frame in frames else "-"
        # turn: ROOT depth-0 span
        if frame=="ROOT" and not in_turn: turn+=1; in_turn=True
        tno = turn if frame is not None else "-"
        print(f"RT{i:2} {role:9} frame={(frame[:13] if frame else '—'):13} parent={str(par)[:13]:13} depth={d} stop={rt['stop'] or '-':10} turn={tno}")
        # register emitted tool_uses
        for tu_id,name in rt["tus"]:
            pending_tu[tu_id]=frame
        for sid,pr in rt["spawns"]:
            pending_spawn[pr]=(sid, frame)
        # remove answered
        for tu in rt["trs"]: pending_tu.pop(tu,None)
        # close turn at ROOT depth-0 terminal with nothing pending under root
        if frame=="ROOT" and rt["stop"] in TERM:
            open_children = any(v=="ROOT" or (v in frames) for k,v in pending_tu.items())
            if not open_children:
                in_turn=False; print(f"     └─ ROOT {rt['stop']} → TURN {turn} ENDS")
