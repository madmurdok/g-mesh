import sys, json, sqlite3, numpy as np
sys.path.insert(0, '/Users/Valentin_Taiurskii/Projects/ClaudeProjects/g-mesh-wt-gm441/eval/embedding')
from rerank_eval import text_to_embed
W='/Users/Valentin_Taiurskii/Projects/ClaudeProjects/g-mesh/eval/embedding/work'
R=f'{W}/runs-gm465/jina-v2-base-code-int8-structured/task-tracker-mcp'
con=sqlite3.connect(f'file:{W}/task-tracker-mcp.sqlite?mode=ro',uri=True)
rows=con.execute("SELECT id,docComment,signature,kind,filePath FROM nodes ORDER BY id").fetchall()
ids=[r[0] for r in rows if text_to_embed(r[1],r[2]) is not None]; meta={r[0]:r for r in rows}
V=np.fromfile(f'{R}/vectors.bin',dtype='<f4').reshape(len(ids),-1)
p=sqlite3.connect('file:/tmp/gm441/home-ttm/projects/c384704626904451/index.db?mode=ro',uri=True)
pv={a:np.frombuffer(b,dtype='<f4') for a,b in p.execute("SELECT nodeId,embedding FROM vectors")}
print('harness candidates',len(ids),'production vectors',len(pv),'production ids missing from harness',sum(i not in set(ids) for i in pv))
cs=[float(V[i]@pv[n]/np.linalg.norm(V[i])/np.linalg.norm(pv[n])) for i,n in enumerate(ids) if n in pv]
mx=max(float(np.abs(V[i]-pv[n]).max()) for i,n in enumerate(ids) if n in pv)
print('shared',len(cs),'min cosine %.9f'%min(cs),'max |delta| %.3e'%mx)
print('harness-only:',[(meta[n][4],meta[n][2]) for n in ids if n not in pv])
