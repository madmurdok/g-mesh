#!/usr/bin/env python3
"""Routes J and R3: fine-tune jina-v2-base-code (full or LoRA) or the gte-small
student (docs/design/GM-730-gpu-training-routes.md, sections 1.2, 1.3, 2, 6 S4
and 9.1 M7/M9).

Vectors: mean pooling over the last hidden state, L2-normalised; queries
truncated to --max-len-query, node structured texts to --max-len-doc.

Loss per batch of --batch train queries:
    InfoNCE(query; its gold positive, --docs-per-query - 1 GM-456 hard negatives,
            every other in-batch query's docs) at temperature --tau
    + alpha * KL(softmax(teacher / tau_t) || softmax(cos / tau)) over the query's
      window (RR-T gte-mb scores, $GM730_TEACHER), for queries that have a window.
A column that is one of the query's positives counts as a positive (multi-positive
log-sum-exp); its answersAlso columns are left out of its softmax (gm436 R1).
The hard negatives of a query in an epoch are drawn with the generator
(seed, epoch, query index); batches are length-bucketed (trainlib.bucketed_batches)
and every forward runs in length-sorted chunks. AdamW, 5% linear warmup, linear
decay; --bf16 autocasts the training forward (dev vectors are fp32).

--mode lora: peft LoRA of rank --rank on --lora-targets (module names),
lora_alpha = 2 * rank, no dropout; only the adapters train and are checkpointed.

Dev: every dev query ranked against every node of its repo (texts.jsonl order,
ties to the earlier node; gm436 dev_metric.repo_eval) by torch fp32 vectors;
epoch 0 is the untrained model (the base row); best epoch by dev MRR, epoch 0
included.

--control shuffled: train labels deranged across targets (gm436
data.shuffle_pairs, seed: positives, answersAlso and hard negatives of another
target; that target is the gold doc) and the teacher scores permuted within each
window.

Outputs in --out: result.json (freeze.py's format), train.json, checkpoint.pt
(GM-457 contract, trainlib). --stage final also: the best epoch's weights (LoRA
merged) exported to onnx-fp32/; for jina onnx-int8/ (export.quantize "jina");
export.json with C-P (torch vs fp32 ONNX, 1,000 dev node texts) and, for jina,
C-P2 (dev MRR of the int8 ONNX on the CPU vs the fp32 torch dev MRR, against the
base's own fp32-vs-int8 gap + 0.01).

usage: train_encoder.py --model jina|gte-small --stage grid|final --lr X --tau X --alpha X --out DIR
                        [--mode full|lora --rank R --lora-targets a,b] [--control shuffled] [options]
"""

import argparse
import shutil
import sys
import time
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import trainlib as L  # noqa: E402

D, DM = L.D, L.DM
# --model -> (route, snapshot, export.py model key)
MODELS = {"jina": ("jina", "jina", "jina"), "gte-small": ("gte", "gte-small", "gte")}
PARITY_N = 1000
CP2_MARGIN = 0.01


def load_model(model_name, a, device):
    X = L.export_module()
    _, snap, key = MODELS[model_name]
    m = X.load_torch(key, L.S.path_of(snap))
    if a.mode == "lora":
        from peft import LoraConfig, get_peft_model
        cfg = LoraConfig(r=a.rank, lora_alpha=2 * a.rank, lora_dropout=0.0, bias="none",
                         target_modules=[t for t in a.lora_targets.split(",") if t])
        m = get_peft_model(m, cfg)
    return m.to(device)


def embed_fn(model, autocast):
    import torch
    import torch.nn.functional as F

    def f(feed):
        with autocast:
            h = model(input_ids=feed["input_ids"], attention_mask=feed["attention_mask"]).last_hidden_state
        h = h.float()
        m = feed["attention_mask"].unsqueeze(-1).to(h.dtype)
        return F.normalize((h * m).sum(1) / m.sum(1).clamp_min(1.0), dim=-1)
    return f


class Data:
    def __init__(self, a):
        vdir = L.gm456_version_dir()
        train_repos = set(a.train_repos) if a.train_repos else None
        dev_repos = set(a.dev_repos) if a.dev_repos else None
        self.train_q = L.subset_by_repo(D.load_queries(vdir, "train"), train_repos, a.max_train_queries)
        self.dev_q = L.subset_by_repo(D.load_queries(vdir, "dev"), dev_repos, a.max_dev_queries)
        if a.control == "shuffled":
            self.train_q = D.shuffle_pairs(self.train_q, a.seed)
        tw, _, _ = L.load_split("train")
        self.texts = {t["key"]: t["structured"] for split in ("train", "dev") for t in L.texts_in_order(split)}
        self.dev_nodes = [t for t in L.texts_in_order("dev") if dev_repos is None or t["repo"] in dev_repos]
        self.windows = {}
        if a.alpha > 0:
            teacher = L.load_teacher("train", tw, permute_seed=a.seed if a.control == "shuffled" else None)
            want = {q["id"] for q in self.train_q}
            self.windows = {w["id"]: (w["keys"], teacher[w["id"]]) for w in tw if w["id"] in want}
        self.dropped = {"noGoldText": 0, "negativesWithoutText": 0}
        keep = []
        for q in self.train_q:
            gold = q.get("labelsOf", q["target"])
            if gold not in self.texts:
                self.dropped["noGoldText"] += 1
                continue
            n0 = len(q["negatives"])
            q = dict(q, gold=gold, negatives=[k for k in q["negatives"] if k in self.texts])
            self.dropped["negativesWithoutText"] += n0 - len(q["negatives"])
            keep.append(q)
        self.train_q = keep
        if not self.train_q or not self.dev_q:
            raise SystemExit("no train or dev queries left")

    def sample_negatives(self, qi, q, k, seed, epoch):
        negs = q["negatives"]
        if len(negs) <= k:
            return list(negs)
        rng = np.random.default_rng([seed, epoch, qi])
        return [negs[i] for i in sorted(rng.choice(len(negs), size=k, replace=False).tolist())]


def batch_columns(batch_q, docs):
    """Columns (unique doc keys of the batch, first-seen order) and the pos / valid masks
    (gm436 data.batch_columns semantics) for queries whose docs are `docs[i]`."""
    cols, col = [], {}
    for ds in docs:
        for k in ds:
            if k not in col:
                col[k] = len(cols)
                cols.append(k)
    pos = np.zeros((len(batch_q), len(cols)), bool)
    valid = np.ones((len(batch_q), len(cols)), bool)
    for i, q in enumerate(batch_q):
        p = set(q["positives"]) | {q["gold"]}
        for k in q["answersAlso"]:
            if k in col and k not in p:
                valid[i, col[k]] = False
        for k in p:
            if k in col:
                pos[i, col[k]] = True
    return cols, pos, valid


def train_step(model, opt, sched, toks, data, idx, epoch, device, a, autocast):
    import torch
    import torch.nn.functional as F
    qtok, dtok = toks
    batch = [data.train_q[i] for i in idx]
    docs = [[q["gold"]] + data.sample_negatives(int(i), q, a.docs_per_query - 1, a.seed, epoch) for i, q in zip(idx, batch)]
    cols, pos, valid = batch_columns(batch, docs)
    keys = list(cols)
    seen = set(keys)
    wins = []
    for q in batch:
        w = data.windows.get(q["id"]) if a.alpha > 0 else None
        wins.append(w)
        if w:
            for k in w[0]:
                if k not in seen:
                    seen.add(k)
                    keys.append(k)
    at = {k: j for j, k in enumerate(keys)}
    f = embed_fn(model, autocast)
    qv = L.forward_sorted(f, qtok.encode_batch([q["text"] for q in batch]), a.fwd_batch, L.pad_id(qtok), device)
    dv = L.forward_sorted(f, dtok.encode_batch([data.texts[k] for k in keys]), a.fwd_batch, L.pad_id(dtok), device)
    logits = qv @ dv[:len(cols)].T / a.tau
    pos_t, valid_t = torch.from_numpy(pos).to(device), torch.from_numpy(valid).to(device)
    ninf = logits.new_tensor(float("-inf"))
    nce = (torch.logsumexp(torch.where(valid_t, logits, ninf), 1) - torch.logsumexp(torch.where(pos_t, logits, ninf), 1)).mean()
    loss = nce
    kl_val = None
    if a.alpha > 0:
        kls = []
        for i, w in enumerate(wins):
            if not w:
                continue
            wk, t = w
            s = qv[i] @ dv[[at[k] for k in wk]].T / a.tau
            p_t = F.softmax(torch.from_numpy(np.asarray(t, np.float32)).to(device) / a.tau_t, 0)
            kls.append((p_t * (torch.log(p_t.clamp_min(1e-30)) - F.log_softmax(s, 0))).sum())
        if kls:
            kl = torch.stack(kls).mean()
            loss = loss + a.alpha * kl
            kl_val = float(kl)
    opt.zero_grad(set_to_none=True)
    loss.backward()
    opt.step()
    sched.step()
    return {"loss": float(loss), "nce": float(nce), "kl": kl_val, "docs": len(keys)}


def embed_all(fn_feed, tok, texts, chunk, device):
    """(n, d) float64 vectors of texts via fn_feed(feed) in length-sorted chunks."""
    import torch
    with torch.no_grad():
        v = L.forward_sorted(fn_feed, tok.encode_batch(list(texts)), chunk, L.pad_id(tok), device)
    return v.double().cpu().numpy()


def dev_rank(qmat, nmat, data):
    nrows = [{"repo": t["repo"], "key": t["key"]} for t in data.dev_nodes]
    qrows = [{"key": q["id"]} for q in data.dev_q]
    ev, missing = DM.repo_eval(None, data.dev_q, qrows, qmat, nrows, nmat)
    if missing:
        raise SystemExit(f"{len(missing)} dev queries without vectors")
    return ev


def dev_eval(model, toks, data, device, a):
    import contextlib
    model.eval()
    f = embed_fn(model, contextlib.nullcontext())
    qmat = embed_all(f, toks[0], [q["text"] for q in data.dev_q], a.eval_batch, device)
    nmat = embed_all(f, toks[1], [t["structured"] for t in data.dev_nodes], a.eval_batch, device)
    model.train()
    return dev_rank(qmat, nmat, data)


def ort_dev_eval(onnx_path, toks, data, a):
    """Dev evals of an exported ONNX on the CPU (ORT), mean-pooled as the torch path."""
    import torch
    X = L.export_module()
    sess = X.ort_session(onnx_path)
    names = [i.name for i in sess.get_inputs()]

    def f(feed):
        np_feed = {k: v.cpu().numpy() for k, v in feed.items()}
        if "token_type_ids" in names:
            np_feed["token_type_ids"] = np.zeros_like(np_feed["input_ids"])
        h = sess.run(None, {k: np_feed[k] for k in names})[0]
        v = X.mean_pool(h, np_feed["attention_mask"])
        return torch.from_numpy(v / np.linalg.norm(v, axis=1, keepdims=True))
    cpu = torch.device("cpu")
    qmat = embed_all(f, toks[0], [q["text"] for q in data.dev_q], a.eval_batch, cpu)
    nmat = embed_all(f, toks[1], [t["structured"] for t in data.dev_nodes], a.eval_batch, cpu)
    return dev_rank(qmat, nmat, data)


def export_final(model, base_mrr, best_mrr, toks, data, a, out):
    """Export the loaded best model; C-P, and for jina int8 + C-P2."""
    X = L.export_module()
    _, snap_name, key = MODELS[a.model]
    snap = L.S.path_of(snap_name)
    if a.mode == "lora":
        model = model.merge_and_unload()
    model.eval()
    fp32_dir = out / "onnx-fp32"
    shutil.rmtree(fp32_dir, ignore_errors=True)
    res = {"fp32": {"dir": fp32_dir.name, "files": X.export_dir(model, key, fp32_dir, snap)}}
    texts = [t["structured"] for t in data.dev_nodes][:PARITY_N]
    res["cp"] = X.parity_embed(model, key, fp32_dir / "model.onnx", X.tokenizer(snap, X.PARITY_TOKENS[key]), texts)
    res["bytes"] = {"fp32": (fp32_dir / "model.onnx").stat().st_size}
    ok = res["cp"]["pass"]
    if key == "jina":
        q_dir = out / "onnx-int8"
        shutil.rmtree(q_dir, ignore_errors=True)
        q_sha = X.quantize("jina", fp32_dir / "model.onnx", q_dir / "model.onnx")
        res["int8"] = {"dir": q_dir.name, "files": {"model.onnx": q_sha, **X.copy_tokenizer(snap, q_dir)}}
        res["bytes"]["int8"] = (q_dir / "model.onnx").stat().st_size
        # the base's own fp32 -> int8 gap, measured through the same export + recipe
        tmp = out / "_base_tmp"
        shutil.rmtree(tmp, ignore_errors=True)
        try:
            X.export_dir(X.load_torch(key, snap), key, tmp / "fp32", snap)
            X.quantize("jina", tmp / "fp32" / "model.onnx", tmp / "int8" / "model.onnx")
            t = time.monotonic()
            base_int8 = L.summarize(ort_dev_eval(tmp / "int8" / "model.onnx", toks, data, a))["mrr"]
            int8 = L.summarize(ort_dev_eval(q_dir / "model.onnx", toks, data, a))["mrr"]
            secs = time.monotonic() - t
        finally:
            shutil.rmtree(tmp, ignore_errors=True)
        gap = base_mrr - base_int8
        floor = best_mrr - (gap + CP2_MARGIN)
        res["cp2"] = {"check": "C-P2", "int8DevMrr": int8, "fp32DevMrr": best_mrr, "baseFp32DevMrr": base_mrr,
                      "baseInt8DevMrr": base_int8, "baseGap": gap, "margin": CP2_MARGIN, "floor": floor,
                      "pass": int8 >= floor, "cpuSeconds": secs}
        ok = ok and res["cp2"]["pass"]
    L.write_json(out / "export.json", res)
    L.progress(f"{out}: export C-P {res['cp']['maxCosDist']:.3g} -> {'pass' if res['cp']['pass'] else 'FAIL'}"
               + (f"; C-P2 int8 {res['cp2']['int8DevMrr']:.4f} >= {res['cp2']['floor']:.4f} -> "
                  f"{'pass' if res['cp2']['pass'] else 'FAIL'}" if "cp2" in res else ""))
    return res, ok


def run(a, argv):
    import torch
    route, snap, _ = MODELS[a.model]
    cfg, cfg_sha = L.config_of(a)
    out = Path(a.out)
    device = L.setup_torch(a)
    name, grid_args = L.grid_identity(route, argv, out)
    data = Data(a)
    toks = (L.tokenizer(L.S.path_of(snap), a.max_len_query), L.tokenizer(L.S.path_of(snap), a.max_len_doc))
    model = load_model(a.model, a, device)
    model.train()
    autocast = L.autocast(a, device)
    steps_per_epoch = -(-len(data.train_q) // a.batch)
    total_steps = steps_per_epoch * a.epochs
    opt = torch.optim.AdamW([p for p in model.parameters() if p.requires_grad], lr=a.lr, weight_decay=a.weight_decay)
    sched = torch.optim.lr_scheduler.LambdaLR(opt, L.linear_warmup(total_steps, a.warmup))
    L.progress(f"{out}: {a.model} {a.mode}, {len(data.train_q)} train / {len(data.dev_q)} dev queries, "
               f"{len(data.dev_nodes)} dev nodes, {len(data.windows)} teacher windows, {steps_per_epoch} steps/epoch, "
               f"params {L.count_params(model)}")
    gl = toks[1].encode_batch([data.texts[q["gold"]] for q in data.train_q])
    lengths = [len(e.ids) for e in gl]

    with L.Checkpointer(a, cfg_sha) as ck:
        st = ck.load()
        if st is not None:
            L.load_trainable(model, st["params"])
            opt.load_state_dict(st["opt"])
            sched.load_state_dict(st["sched"])
            L.progress(f"{out}: resumed at epoch {st['epoch']} step {st['step']}")
        else:
            t = time.monotonic()
            ev0 = dev_eval(model, toks, data, device, a)
            m0 = L.summarize(ev0)
            st = {"epoch": 1, "step": 0, "globalStep": 0, "baseEvals": ev0, "bestEvals": ev0, "bestEpoch": 0,
                  "best": m0["mrr"], "history": [{"epoch": 0, **m0, "devSeconds": time.monotonic() - t}],
                  "bestParams": L.trainable_state(model) if a.stage == "final" else None, "losses": [],
                  "docsPerStep": [], "trainSeconds": 0.0, "done": False}
            L.progress(f"{out}: epoch 0 dev MRR {m0['mrr']:.4f} r@10 {m0['recall@10']:.4f}")
        probe = L.Probe(f"{route} {name}", total_steps)
        t_run = time.monotonic()

        def state():
            return dict(st, params=L.trainable_state(model), opt=opt.state_dict(), sched=sched.state_dict(),
                        trainSeconds=st["trainSeconds"] + time.monotonic() - t_run)

        while not st["done"]:
            epoch = st["epoch"]
            batches = L.bucketed_batches(lengths, a.batch, a.seed, epoch)
            for bi in range(st["step"], len(batches)):
                r = train_step(model, opt, sched, toks, data, batches[bi], epoch, device, a, autocast)
                st["losses"].append(r["loss"])
                st["docsPerStep"].append(r["docs"])
                st["step"], st["globalStep"] = bi + 1, st["globalStep"] + 1
                probe.tick(st["globalStep"])
                ck.at_step(st["globalStep"], state)
            t = time.monotonic()
            ev = dev_eval(model, toks, data, device, a)
            m = L.summarize(ev)
            st["history"].append({"epoch": epoch, **m, "loss": float(np.mean(st["losses"])) if st["losses"] else None,
                                  "devSeconds": time.monotonic() - t})
            if m["mrr"] > st["best"]:
                st.update(best=m["mrr"], bestEpoch=epoch, bestEvals=ev)
                if a.stage == "final":
                    st["bestParams"] = L.trainable_state(model)
            L.progress(f"{out}: epoch {epoch}/{a.epochs} loss {st['history'][-1]['loss']} dev MRR {m['mrr']:.4f} "
                       f"(best {st['best']:.4f} @ {st['bestEpoch']})")
            st.update(epoch=epoch + 1, step=0, losses=[], done=epoch >= a.epochs)
            ck.save(state())
        st["trainSeconds"] += time.monotonic() - t_run

    res = L.result_json(route, name, grid_args, st["bestEpoch"], st["baseEvals"], st["bestEvals"], a.seed, a.resamples)
    info = {"config": cfg, "configSha256": cfg_sha, "stage": a.stage, "control": a.control, "history": st["history"],
            "lastParamsSha256": L.params_sha256(L.trainable_state(model)), "params": L.count_params(model),
            "queries": {"train": len(data.train_q), "dev": len(data.dev_q), "devNodes": len(data.dev_nodes),
                        "teacherWindows": len(data.windows), "dropped": data.dropped},
            "stepsPerEpoch": steps_per_epoch, "meanDocsPerStep": float(np.mean(st["docsPerStep"])) if st["docsPerStep"] else None,
            "trainSeconds": st["trainSeconds"], "determinism": L.determinism_note(a),
            "checkpointBytes": (out / "checkpoint.pt").stat().st_size}
    ok = True
    if a.stage == "final":
        L.load_trainable(model, st["bestParams"])
        info["bestParamsSha256"] = L.params_sha256(st["bestParams"])
        info["export"], ok = export_final(model, res["base"]["mrr"], res["best"]["mrr"], toks, data, a, out)
    L.write_json(out / "train.json", info)
    L.write_json(out / "result.json", res)
    L.progress(f"{out}: done; best epoch {res['best']['epoch']} dev MRR {res['best']['mrr']:.4f}, "
               f"gain {res['gain']['mrr']:+.4f} [{res['gain']['lower']:+.4f}, {res['gain']['upper']:+.4f}]")
    return 0 if ok else 1


def parse(argv):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--model", choices=sorted(MODELS), required=True)
    ap.add_argument("--stage", choices=["grid", "final"], required=True)
    ap.add_argument("--control", choices=["none", "shuffled"], default="none")
    ap.add_argument("--mode", choices=["full", "lora"], default="full")
    ap.add_argument("--rank", type=int, default=16)
    ap.add_argument("--lora-targets", default="query,key,value,dense,up_gated_layer,down_layer")
    ap.add_argument("--lr", type=float, required=True)
    ap.add_argument("--tau", type=float, default=0.05)
    ap.add_argument("--tau-t", dest="tau_t", type=float, default=1.0, help="teacher temperature of the KL term")
    ap.add_argument("--alpha", type=float, default=0.0, help="weight of the KL-to-teacher term")
    ap.add_argument("--epochs", type=int, default=2)
    ap.add_argument("--batch", type=int, default=32, help="queries per step")
    ap.add_argument("--docs-per-query", type=int, default=8, help="gold + hard negatives per query")
    ap.add_argument("--max-len-query", type=int, default=64)
    ap.add_argument("--max-len-doc", type=int, default=256)
    ap.add_argument("--bf16", action="store_true", help="bf16 autocast for the training forward")
    ap.add_argument("--fwd-batch", type=int, default=128, help="texts per length-sorted training forward chunk")
    ap.add_argument("--eval-batch", type=int, default=128)
    ap.add_argument("--warmup", type=float, default=0.05)
    ap.add_argument("--weight-decay", type=float, default=0.01)
    ap.add_argument("--resamples", type=int, default=10_000)
    ap.add_argument("--train-repos", nargs="*")
    ap.add_argument("--dev-repos", nargs="*")
    ap.add_argument("--max-train-queries", type=int)
    ap.add_argument("--max-dev-queries", type=int)
    L.add_common_args(ap)
    a = ap.parse_args(argv)
    if a.mode == "lora" and a.model != "jina":
        ap.error("--mode lora is route J's (jina) only")
    return a


def main(argv=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    return run(parse(argv), argv)


if __name__ == "__main__":
    sys.exit(main())
