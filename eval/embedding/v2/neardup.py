"""GM-460 section 6 near-duplicate review: for each new/queries text, the nearest other text (GM-398, confirm, and the other new texts, all corpora) by token Jaccard; prints every pair at 0.5 or more.

usage: neardup.py [id prefix ...]   e.g. neardup.py gin req (default: all new queries)
"""
import json,re,sys,os,glob
R=os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "")
def toks(t): return set(w.lower() for w in re.findall(r"[A-Za-z0-9]+",t))
old=[]
for d in ("queries","confirm/queries"):
    for f in sorted(glob.glob(R+d+"/*.jsonl")):
        for l in open(f):
            if l.strip(): q=json.loads(l); old.append((q["id"],q["text"]))
new=[]
for f in sorted(glob.glob(R+"v2/new/queries/*.jsonl")):
    for l in open(f):
        if l.strip(): q=json.loads(l); new.append((q["id"],q["text"]))
sel=sys.argv[1:]
hits=0
for i,(qid,t) in enumerate(new):
    if sel and not any(qid.startswith(s+"-") for s in sel): continue
    a=toks(t); best=(0,None,None)
    for oid,ot in old+[x for j,x in enumerate(new) if j!=i]:
        b=toks(ot); j=len(a&b)/len(a|b)
        if j>best[0]: best=(j,oid,ot)
    if best[0]>=0.5:
        hits+=1; print("%.2f %s | %s\n     %s | %s"%(best[0],qid,t,best[1],best[2]))
print("pairs>=0.5:",hits)
