#!/usr/bin/env python3
"""GM-436 query adapters on frozen encoder vectors (docs/design/GM-436-laptop-training.md,
section 4, routes R1 and R2).

R1, gold labels: InfoNCE over [the query's positives; its GM-456 hard
negatives; every other in-batch query's positives and hard negatives], minus
the query's `answersAlso` (data.batch_columns), multi-positive log-sum-exp.
R2, distillation: KL(p_teacher || p_student) over the teacher cache's
candidate list + alpha * the R1 loss.

Forms: full W (init I, lambda ||W - I||_F^2), low-rank W = I + U V^T (U = 0,
V ~ N(0, 0.01)), orthogonal Procrustes (closed form). Every form ends as a
dense d x d W with q' = W q.

Fit: AdamW, <= --epochs epochs, early stop on dev MRR (patience); the dev MRR
of the initial W is epoch 0, and the best-dev W is the result. A checkpoint
after every epoch holds the parameters, AdamW state, history, best W and RNG
states; the epoch's batch order is a function of (seed, epoch), so a
kill-and-resume run reproduces an uninterrupted run's W bit for bit
(same --threads).

Outputs in --out: checkpoint.pt, result.json (config + its sha256, history,
best dev MRR with a target-clustered CI, train MRR for C6, W sha256),
dev_ranks.jsonl (per dev query first-hit rank of the best W).

usage: train_adapter.py --base <variant> --form full|lowrank|procrustes --out <dir> [options]
"""

import argparse
import hashlib
import json
import os
import random
import sys
import time
from pathlib import Path

import numpy as np
import torch

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import cache as K  # noqa: E402
import data as D  # noqa: E402
import dev_metric as M  # noqa: E402

C = K.C
KILLED_EXIT = 75


def progress(msg):
    path = os.environ.get("GM436_PROGRESS")
    line = f"{time.strftime('%H:%M:%S')} {msg}"
    if path:
        with open(path, "a") as f:
            f.write(line + "\n")
    print(line, flush=True)


def config_of(a):
    cfg = {k: getattr(a, k) for k in ("version", "base", "route", "form", "tau", "lam", "rank", "lr", "epochs",
                                       "patience", "batch", "fraction", "control", "alpha", "tau_t", "teacher_dir",
                                       "exclude_language", "train_repos", "dev_repos", "seed")}
    return cfg, hashlib.sha256(json.dumps(cfg, sort_keys=True).encode()).hexdigest()


def w_sha256(W):
    return hashlib.sha256(np.ascontiguousarray(W, dtype="<f4").tobytes()).hexdigest()


# --- data --------------------------------------------------------------------

class Base:
    """One base encoder's node and query vectors, checked against GM-456 by (key, textSha256)."""

    def __init__(self, vdir, variant, repos):
        mine = vdir / "mine"
        nt, qt = K.tables_for(mine, variant, repos[0])
        self.nrows, self.nmat = nt.load(repos)
        self.qrows, self.qmat = qt.load(repos)
        loaded = {r["repo"] for r in self.nrows}
        if set(repos) - loaded:
            raise SystemExit(f"{variant}: no imported vectors for {sorted(set(repos) - loaded)}; run cache.py import")
        self.prefix = K.variant_row(mine, variant).get("query_prefix", "")
        spans = {}
        for repo in repos:
            spans.update(K.frame_spans(vdir, repo))
        self.node = {}
        stale = 0
        for i, r in enumerate(self.nrows):
            if r["key"] in spans and r["textSha256"] != spans[r["key"]]:
                stale += 1
                continue
            self.node[r["key"]] = i
        self.query = {r["key"]: (i, r["textSha256"]) for i, r in enumerate(self.qrows)}
        self.stale_nodes = stale
        self.nn = K.normalise(self.nmat)

    def keep(self, queries):
        """Queries whose vector matches their text and whose positives have vectors; the rest are counted."""
        out, dropped = [], {"queryVector": 0, "positives": 0}
        for q in queries:
            hit = self.query.get(q["id"])
            if hit is None or hit[1] != K.query_text_sha(self.prefix, q["text"]):
                dropped["queryVector"] += 1
                continue
            pos = [k for k in q["positives"] if k in self.node]
            if not pos:
                dropped["positives"] += 1
                continue
            out.append(dict(q, positives=pos, negatives=[k for k in q["negatives"] if k in self.node],
                            answersAlso=[k for k in q["answersAlso"] if k in self.node]))
        return out, dropped

    def qvec(self, queries):
        return self.qmat[[self.query[q["id"]][0] for q in queries]]


# --- model -------------------------------------------------------------------

class Adapter(torch.nn.Module):
    def __init__(self, d, form, rank):
        super().__init__()
        self.form = form
        self.register_buffer("eye", torch.eye(d, dtype=torch.float32))
        if form == "full":
            self.W = torch.nn.Parameter(torch.eye(d, dtype=torch.float32))
        else:
            self.U = torch.nn.Parameter(torch.zeros(d, rank))
            self.V = torch.nn.Parameter(torch.randn(d, rank) * 0.01)

    def matrix(self):
        return self.W if self.form == "full" else self.eye + self.U @ self.V.T


def r1_loss(qa_n, cols_n, pos, valid, tau):
    logits = qa_n @ cols_n.T / tau
    neg_inf = torch.tensor(float("-inf"))
    all_ = torch.logsumexp(torch.where(valid, logits, neg_inf), dim=1)
    good = torch.logsumexp(torch.where(pos, logits, neg_inf), dim=1)
    return (all_ - good).mean()


def batch_tensors(base, batch):
    cols, pos, valid = D.batch_columns(batch)
    cn = torch.from_numpy(base.nn[[base.node[k] for k in cols]].astype(np.float32))
    return cn, torch.from_numpy(pos), torch.from_numpy(valid)


def kd_tensors(base, batch, teacher, tau_t):
    rows, lens = [], []
    for q in batch:
        keys, scores = teacher[q["id"]]
        keep = [(base.node[k], s) for k, s in zip(keys, scores) if k in base.node]
        rows.append(keep)
        lens.append(len(keep))
    width = max(lens)
    idx = np.zeros((len(batch), width), np.int64)
    ts = np.full((len(batch), width), -np.inf)
    mask = np.zeros((len(batch), width), bool)
    for i, keep in enumerate(rows):
        for j, (n, s) in enumerate(keep):
            idx[i, j], ts[i, j], mask[i, j] = n, s / tau_t, True
    p_t = torch.from_numpy(np.exp(ts - np.logaddexp.reduce(ts, axis=1, keepdims=True)).astype(np.float32))
    cand = torch.from_numpy(base.nn[idx].astype(np.float32))
    return cand, p_t, torch.from_numpy(mask)


def kd_loss(qa_n, cand, p_t, mask, tau_s):
    logits = torch.einsum("bd,bkd->bk", qa_n, cand) / tau_s
    logits = torch.where(mask, logits, torch.tensor(float("-inf")))
    log_ps = torch.log_softmax(logits, dim=1)
    terms = torch.where(mask & (p_t > 0), p_t * (torch.log(p_t.clamp_min(1e-30)) - log_ps), torch.zeros(()))
    return terms.sum(1).mean()


def procrustes(base, train):
    """Orthogonal W minimising sum ||W q_i - c_i||, q_i and c_i (target vectors) normalised."""
    q = K.normalise(base.qvec(train))
    c = base.nn[[base.node[t["target"]] if t["target"] in base.node else base.node[t["positives"][0]] for t in train]]
    u, _, vt = np.linalg.svd(q.T @ c)
    return (u @ vt).T.astype(np.float32)


# --- loop --------------------------------------------------------------------

def dev_eval(base, W, dev):
    evals, _ = M.repo_eval(W, dev, base.qrows, base.qmat, base.nrows, base.nmat)
    return evals


def save_ckpt(path, obj):
    tmp = Path(str(path) + ".tmp")
    torch.save(obj, tmp)
    with open(tmp, "rb+") as f:
        os.fsync(f.fileno())
    os.replace(tmp, path)


def rng_state():
    return {"python": random.getstate(), "numpy": np.random.get_state(), "torch": torch.get_rng_state()}


def set_rng_state(s):
    random.setstate(s["python"])
    np.random.set_state(s["numpy"])
    torch.set_rng_state(s["torch"])


def train(a):
    cfg, cfg_sha = config_of(a)
    out = Path(a.out)
    out.mkdir(parents=True, exist_ok=True)
    ckpt_path = out / "checkpoint.pt"
    torch.set_num_threads(a.threads)
    torch.use_deterministic_algorithms(True)
    random.seed(a.seed)
    np.random.seed(a.seed)
    torch.manual_seed(a.seed)

    vdir = C.version_dir(a.version)
    train_q = D.load_queries(vdir, "train", set(a.train_repos) if a.train_repos else None, tuple(a.exclude_language or ()))
    dev_q = D.load_queries(vdir, "dev", set(a.dev_repos) if a.dev_repos else None)
    repos = sorted({q["repo"] for q in train_q + dev_q})
    base = Base(vdir, a.base, repos)
    train_q = D.subset_queries(train_q, a.fraction, a.seed)
    if a.control == "c1":
        train_q = D.shuffle_pairs(train_q, a.seed)
    train_q, drop_train = base.keep(train_q)
    dev_q, drop_dev = base.keep(dev_q)
    if not train_q or not dev_q:
        raise SystemExit("no train or dev queries left after the vector checks")
    d = base.nmat.shape[1]
    progress(f"{a.out}: {len(train_q)} train / {len(dev_q)} dev queries, d={d}, dropped train {drop_train} dev {drop_dev}, stale nodes {base.stale_nodes}")

    teacher = None
    if a.route == "r2":
        teacher = K.load_teacher(a.teacher_dir)
        if a.control == "c2":
            perm = D.permute_within({qid: s for qid, (_, s) in teacher.items()}, a.seed)
            teacher = {qid: (keys, perm[qid]) for qid, (keys, _) in teacher.items()}
        missing = [q["id"] for q in train_q if q["id"] not in teacher]
        if missing:
            raise SystemExit(f"{len(missing)} train queries have no teacher scores (first {missing[0]})")

    if a.form == "procrustes":
        W = procrustes(base, train_q)
        ev = dev_eval(base, W, dev_q)
        state = {"configSha256": cfg_sha, "config": cfg, "done": True, "bestW": W, "bestEpoch": 1,
                 "history": [{"epoch": 1, **M.metrics([r for *_, r in ev])}], "bestEvals": ev}
        save_ckpt(ckpt_path, state)
        return finish(a, base, train_q, dev_q, state, drop_train, drop_dev)

    model = Adapter(d, a.form, a.rank)
    opt = torch.optim.AdamW(model.parameters(), lr=a.lr, weight_decay=0.0)
    if ckpt_path.exists():
        state = torch.load(ckpt_path, weights_only=False)
        if state["configSha256"] != cfg_sha:
            raise SystemExit(f"{ckpt_path}: written for config {state['configSha256'][:12]}, this run is {cfg_sha[:12]}")
        if state["done"]:
            return finish(a, base, train_q, dev_q, state, drop_train, drop_dev)
        model.load_state_dict(state["params"])
        opt.load_state_dict(state["opt"])
        set_rng_state(state["rng"])
        progress(f"{a.out}: resumed after epoch {state['epoch']}")
    else:
        W0 = model.matrix().detach().numpy().copy()
        ev = dev_eval(base, W0, dev_q)
        m0 = M.metrics([r for *_, r in ev])
        state = {"configSha256": cfg_sha, "config": cfg, "done": False, "epoch": 0, "bad": 0,
                 "best": m0["mrr"], "bestEpoch": 0, "bestW": W0, "bestEvals": ev,
                 "history": [{"epoch": 0, **m0}]}
        progress(f"{a.out}: epoch 0 dev MRR {m0['mrr']:.4f}")

    Q = torch.from_numpy(base.qvec(train_q).astype(np.float32))
    for epoch in range(state["epoch"] + 1, a.epochs + 1):
        perm = np.random.default_rng([a.seed, epoch]).permutation(len(train_q))
        losses = []
        for s in range(0, len(perm), a.batch):
            bi = perm[s:s + a.batch]
            batch = [train_q[i] for i in bi]
            W = model.matrix()
            qa = torch.nn.functional.normalize(Q[bi] @ W.T, dim=1)
            cn, pos, valid = batch_tensors(base, batch)
            loss = r1_loss(qa, cn, pos, valid, a.tau)
            if a.route == "r2":
                cand, p_t, mask = kd_tensors(base, batch, teacher, a.tau_t)
                loss = kd_loss(qa, cand, p_t, mask, a.tau) + a.alpha * loss
            if a.form == "full" and a.lam:
                loss = loss + a.lam * ((W - model.eye) ** 2).sum()
            opt.zero_grad()
            loss.backward()
            opt.step()
            losses.append(float(loss))
        Wn = model.matrix().detach().numpy().copy()
        ev = dev_eval(base, Wn, dev_q)
        m = M.metrics([r for *_, r in ev])
        state["history"].append({"epoch": epoch, "loss": float(np.mean(losses)), **m})
        if m["mrr"] > state["best"]:
            state.update(best=m["mrr"], bestEpoch=epoch, bestW=Wn, bestEvals=ev, bad=0)
        else:
            state["bad"] += 1
        state.update(epoch=epoch, lastW=Wn, params=model.state_dict(), opt=opt.state_dict(), rng=rng_state(),
                     done=state["bad"] >= a.patience or epoch == a.epochs)
        save_ckpt(ckpt_path, state)
        progress(f"{a.out}: epoch {epoch}/{a.epochs} loss {np.mean(losses):.4f} dev MRR {m['mrr']:.4f} (best {state['best']:.4f} @ {state['bestEpoch']})")
        if state["done"]:
            break
        if a.stop_after_epochs and epoch >= a.stop_after_epochs:
            progress(f"{a.out}: stopping after epoch {epoch} (--stop-after-epochs)")
            sys.exit(KILLED_EXIT)
    return finish(a, base, train_q, dev_q, state, drop_train, drop_dev)


def finish(a, base, train_q, dev_q, state, drop_train, drop_dev):
    out = Path(a.out)
    W = np.asarray(state["bestW"], np.float32)
    ev = state["bestEvals"]
    rr = M.rr_values(ev)
    targets = [t for _, t, _ in ev]
    with open(out / "dev_ranks.jsonl", "w") as f:
        for qid, t, r in ev:
            f.write(json.dumps({"id": qid, "target": t, "rank": r if np.isfinite(r) else None}) + "\n")
    train_ev, _ = M.repo_eval(W, train_q, base.qrows, base.qmat, base.nrows, base.nmat) if not a.no_train_metric else ([], [])
    res = {"config": state["config"], "configSha256": state["configSha256"], "bestEpoch": state["bestEpoch"],
           "history": state["history"], "wSha256": w_sha256(W),
           "lastWSha256": w_sha256(state["lastW"]) if "lastW" in state else None, "dim": int(W.shape[0]),
           "dev": {**M.metrics([r for *_, r in ev]), "mrrCI": M.cluster_bootstrap(rr, targets, a.resamples, a.seed),
                   "queries": len(ev), "targets": len(set(targets))},
           "train": M.metrics([r for *_, r in train_ev]) if train_ev else None,
           "queries": {"train": len(train_q), "dev": len(dev_q), "droppedTrain": drop_train, "droppedDev": drop_dev}}
    C.write_json(out / "result.json", res)
    progress(f"{a.out}: done, best epoch {state['bestEpoch']}, dev MRR {res['dev']['mrr']:.4f}, W sha256 {res['wSha256'][:12]}")
    return res


def parse(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--version", default="v0.4-full")
    ap.add_argument("--base", required=True, help="variant whose vectors are imported (cache.py import)")
    ap.add_argument("--route", choices=["r1", "r2"], default="r1")
    ap.add_argument("--form", choices=["full", "lowrank", "procrustes"], required=True)
    ap.add_argument("--tau", type=float, default=0.05)
    ap.add_argument("--lam", type=float, default=0.0)
    ap.add_argument("--rank", type=int, default=32)
    ap.add_argument("--lr", type=float, default=1e-3)
    ap.add_argument("--epochs", type=int, default=30)
    ap.add_argument("--patience", type=int, default=3)
    ap.add_argument("--batch", type=int, default=512)
    ap.add_argument("--fraction", type=float, choices=[0.25, 0.5, 1.0], default=1.0)
    ap.add_argument("--control", choices=["none", "c1", "c2"], default="none")
    ap.add_argument("--alpha", type=float, default=0.0, help="R2: weight of the gold InfoNCE term")
    ap.add_argument("--tau-t", dest="tau_t", type=float, default=0.05)
    ap.add_argument("--teacher-dir")
    ap.add_argument("--exclude-language", nargs="*", default=[])
    ap.add_argument("--train-repos", nargs="*")
    ap.add_argument("--dev-repos", nargs="*")
    ap.add_argument("--seed", type=int, default=D.SEED)
    ap.add_argument("--threads", type=int, default=2)
    ap.add_argument("--resamples", type=int, default=M.RESAMPLES)
    ap.add_argument("--no-train-metric", action="store_true", help="skip C6's train MRR")
    ap.add_argument("--stop-after-epochs", type=int, help="exit 75 after this epoch's checkpoint (resume test)")
    ap.add_argument("--out", required=True)
    a = ap.parse_args(argv)
    if a.route == "r2" and not a.teacher_dir:
        ap.error("--route r2 needs --teacher-dir")
    if a.control == "c2" and a.route != "r2":
        ap.error("--control c2 is the shuffled-teacher control of R2")
    if a.route == "r2" and a.form == "procrustes":
        ap.error("R2 trains a full or low-rank form")
    return a


if __name__ == "__main__":
    train(parse())
