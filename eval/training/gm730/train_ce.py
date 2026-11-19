#!/usr/bin/env python3
"""Route RR: distil gte-mb into the MiniLM-L6 cross-encoder
(docs/design/GM-730-gpu-training-routes.md, sections 1.1, 2 and 6 S4).

Loss per query, over its window (30 nodes, 31 with a forced gold):
    KL(softmax(teacher / tau_t) || softmax(student)) + alpha * CE(gold in the window)
mean over the batch's queries. Batches of --batch queries in a (seed, epoch)
order; the forward runs --micro-queries queries at a time (gradients
accumulated, same loss). AdamW, 5% linear warmup, linear decay; max_len
--max-len on (query, ship text) pairs.

Dev: every dev window scored by the student; MRR / recall@10 of the first hit
(positives, answersAlso) inside the window. Epoch 0 is the off-the-shelf L6
(the base row). The best epoch (highest dev MRR, epoch 0 included, ties to
the earlier) is the result.

--control shuffled-teacher: the teacher scores permuted within each window
(gm436 data.permute_within, seed), multisets kept; gold labels unchanged.

Outputs in --out: result.json (freeze.py's format), train.json (history,
determinism, timing, parameter sha256s), checkpoint.pt (GM-457 contract,
trainlib). --stage final also exports the best epoch: onnx-fp32/ (model.onnx
+ tokenizer.json + config.json), onnx-quint8/ (export.quantize "l6", the
official file's closed form), export.json (C-P torch vs fp32 ONNX on 1,000 dev
pairs, quint8 vs fp32 at chunk 1).

usage: train_ce.py --stage grid|final --lr X --tau-t X --alpha X --out DIR [--control shuffled-teacher] [options]
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

ROUTE = "rr"
SNAPSHOT = "l6"
EXPORT_KEY = "l6"
PARITY_N = 1000
QUANT_N = 200


def load_model(device):
    import torch
    from transformers import AutoModelForSequenceClassification
    m = AutoModelForSequenceClassification.from_pretrained(str(L.S.path_of(SNAPSHOT)), torch_dtype=torch.float32)
    return m.to(device)


def logits_fn(model, autocast):
    def f(feed):
        with autocast:
            return model(**feed).logits[:, 0].float()
    return f


class Data:
    def __init__(self, a):
        tw, tt, _ = L.load_split("train")
        dw, dt, _ = L.load_split("dev")
        self.train = L.subset_by_repo(tw, set(a.train_repos) if a.train_repos else None, a.max_train_queries)
        self.dev = L.subset_by_repo(dw, set(a.dev_repos) if a.dev_repos else None, a.max_dev_queries)
        self.texts = {**tt, **dt}
        missing = sorted({k for w in self.train + self.dev for k in w["keys"]} - set(self.texts))
        if missing:
            raise SystemExit(f"{len(missing)} window keys have no text (first {missing[0]})")
        full_train = tw
        teacher = L.load_teacher("train", full_train, permute_seed=a.seed if a.control == "shuffled-teacher" else None)
        self.teacher = {w["id"]: teacher[w["id"]] for w in self.train}

    def pairs(self, w):
        return [(w["text"], self.texts[k]["ship"]) for k in w["keys"]]


def dev_eval(model, tok, data, device, a):
    """[(query id, target, rank)] over the dev windows."""
    import torch
    model.eval()
    out = []
    pad = L.pad_id(tok)
    for s in range(0, len(data.dev), a.eval_queries):
        ws = data.dev[s:s + a.eval_queries]
        encs = tok.encode_batch([p for w in ws for p in data.pairs(w)])
        with torch.no_grad():
            z = L.forward_sorted(lambda feed: model(**feed).logits[:, 0].float(), encs, a.eval_batch, pad, device,
                                 token_types=True).cpu().numpy()
        o = 0
        for w in ws:
            n = len(w["keys"])
            out.append((w["id"], w["target"], L.window_ranks(z[o:o + n], w)))
            o += n
    model.train()
    return out


def train_step(model, opt, sched, tok, data, batch_ws, device, a, autocast):
    import torch
    import torch.nn.functional as F
    pad = L.pad_id(tok)
    opt.zero_grad(set_to_none=True)
    total = 0.0
    for s in range(0, len(batch_ws), a.micro_queries):
        ws = batch_ws[s:s + a.micro_queries]
        encs = tok.encode_batch([p for w in ws for p in data.pairs(w)])
        z = L.forward_sorted(logits_fn(model, autocast), encs, a.micro_batch, pad, device, token_types=True)
        loss, o = 0.0, 0
        for w in ws:
            n = len(w["keys"])
            sl = z[o:o + n]
            o += n
            t = torch.from_numpy(np.asarray(data.teacher[w["id"]], np.float32)).to(device)
            log_s = F.log_softmax(sl, 0)
            p_t = F.softmax(t / a.tau_t, 0)
            kl = (p_t * (torch.log(p_t.clamp_min(1e-30)) - log_s)).sum()
            loss = loss + kl + a.alpha * (-log_s[w["gold"]])
        loss = loss / len(batch_ws)
        loss.backward()
        total += float(loss)
    opt.step()
    sched.step()
    return total


def export_final(model, tok_dev, data, a, out):
    """onnx-fp32 + onnx-quint8 of the loaded (best) model, C-P and the quint8 check."""
    X = L.export_module()
    model.eval()
    snap = L.S.path_of(SNAPSHOT)
    fp32_dir, q_dir = out / "onnx-fp32", out / "onnx-quint8"
    for d in (fp32_dir, q_dir):
        shutil.rmtree(d, ignore_errors=True)
    files = X.export_dir(model, EXPORT_KEY, fp32_dir, snap)
    q_sha = X.quantize(EXPORT_KEY, fp32_dir / "model.onnx", q_dir / "model.onnx")
    qfiles = {"model.onnx": q_sha, **X.copy_tokenizer(snap, q_dir)}
    pairs = [p for w in data.dev for p in data.pairs(w)][:PARITY_N]
    ptok = X.tokenizer(snap, X.PARITY_TOKENS[EXPORT_KEY])
    cp = X.parity_ce(model, fp32_dir / "model.onnx", ptok, pairs)
    qd = X.max_logit_diff(X.ort_forward(X.ort_session(fp32_dir / "model.onnx")),
                          X.ort_forward(X.ort_session(q_dir / "model.onnx")), ptok, pairs[:QUANT_N], batch=1)
    res = {"fp32": {"dir": fp32_dir.name, "files": files}, "quint8": {"dir": q_dir.name, "files": qfiles},
           "cp": cp, "quint8VsFp32": {"n": min(QUANT_N, len(pairs)), "chunk": 1, "maxAbsDiff": qd},
           "bytes": {"fp32": (fp32_dir / "model.onnx").stat().st_size, "quint8": (q_dir / "model.onnx").stat().st_size}}
    L.write_json(out / "export.json", res)
    L.progress(f"{out}: export C-P max |diff| {cp['maxAbsDiff']:.3g} -> {'pass' if cp['pass'] else 'FAIL'}; "
               f"quint8 vs fp32 {qd:.3g}")
    return res


def run(a, argv):
    import torch
    cfg, cfg_sha = L.config_of(a)
    out = Path(a.out)
    device = L.setup_torch(a)
    name, grid_args = L.grid_identity(ROUTE, argv, out)
    data = Data(a)
    tok = L.tokenizer(L.S.path_of(SNAPSHOT), a.max_len)
    model = load_model(device)
    model.train()
    autocast = L.autocast(a, device)
    steps_per_epoch = -(-len(data.train) // a.batch)
    total_steps = steps_per_epoch * a.epochs
    opt = torch.optim.AdamW(model.parameters(), lr=a.lr, weight_decay=a.weight_decay)
    sched = torch.optim.lr_scheduler.LambdaLR(opt, L.linear_warmup(total_steps, a.warmup))
    L.progress(f"{out}: {len(data.train)} train / {len(data.dev)} dev windows, {steps_per_epoch} steps/epoch, "
               f"params {L.count_params(model)}")
    lengths = [sum(len(data.texts[k]["ship"]) for k in w["keys"]) for w in data.train]

    with L.Checkpointer(a, cfg_sha) as ck:
        st = ck.load()
        if st is not None:
            L.load_trainable(model, st["params"])
            opt.load_state_dict(st["opt"])
            sched.load_state_dict(st["sched"])
            L.progress(f"{out}: resumed at epoch {st['epoch']} step {st['step']}")
        else:
            t = time.monotonic()
            ev0 = dev_eval(model, tok, data, device, a)
            m0 = L.summarize(ev0)
            st = {"epoch": 1, "step": 0, "globalStep": 0, "baseEvals": ev0, "bestEvals": ev0, "bestEpoch": 0,
                  "best": m0["mrr"], "history": [{"epoch": 0, **m0, "devSeconds": time.monotonic() - t}],
                  "bestParams": L.trainable_state(model) if a.stage == "final" else None, "losses": [],
                  "trainSeconds": 0.0, "done": False}
            L.progress(f"{out}: epoch 0 dev MRR {m0['mrr']:.4f} r@10 {m0['recall@10']:.4f}")
        probe = L.Probe(f"rr {name}", total_steps)
        t_run = time.monotonic()

        def state():
            return dict(st, params=L.trainable_state(model), opt=opt.state_dict(), sched=sched.state_dict(),
                        trainSeconds=st["trainSeconds"] + time.monotonic() - t_run)

        while not st["done"]:
            epoch = st["epoch"]
            batches = L.bucketed_batches(lengths, a.batch, a.seed, epoch)
            for bi in range(st["step"], len(batches)):
                loss = train_step(model, opt, sched, tok, data, [data.train[i] for i in batches[bi]], device, a, autocast)
                st["losses"].append(loss)
                st["step"], st["globalStep"] = bi + 1, st["globalStep"] + 1
                probe.tick(st["globalStep"])
                ck.at_step(st["globalStep"], state)
            t = time.monotonic()
            ev = dev_eval(model, tok, data, device, a)
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

    res = L.result_json(ROUTE, name, grid_args, st["bestEpoch"], st["baseEvals"], st["bestEvals"], a.seed, a.resamples)
    last = L.trainable_state(model)
    info = {"config": cfg, "configSha256": cfg_sha, "stage": a.stage, "control": a.control, "history": st["history"],
            "lastParamsSha256": L.params_sha256(last), "params": L.count_params(model),
            "queries": {"train": len(data.train), "dev": len(data.dev)}, "stepsPerEpoch": steps_per_epoch,
            "trainSeconds": st["trainSeconds"], "determinism": L.determinism_note(a),
            "checkpointBytes": (out / "checkpoint.pt").stat().st_size}
    if a.stage == "final":
        L.load_trainable(model, st["bestParams"])
        info["bestParamsSha256"] = L.params_sha256(st["bestParams"])
        info["export"] = export_final(model, tok, data, a, out)
    L.write_json(out / "train.json", info)
    L.write_json(out / "result.json", res)
    L.progress(f"{out}: done; best epoch {res['best']['epoch']} dev MRR {res['best']['mrr']:.4f}, "
               f"gain {res['gain']['mrr']:+.4f} [{res['gain']['lower']:+.4f}, {res['gain']['upper']:+.4f}]")
    ok = info.get("export", {}).get("cp", {}).get("pass", True)
    return 0 if ok else 1


def parse(argv):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--stage", choices=["grid", "final"], required=True)
    ap.add_argument("--control", choices=["none", "shuffled-teacher"], default="none")
    ap.add_argument("--lr", type=float, required=True)
    ap.add_argument("--tau-t", dest="tau_t", type=float, default=1.0, help="teacher temperature")
    ap.add_argument("--alpha", type=float, default=0.0, help="weight of the gold cross-entropy term")
    ap.add_argument("--epochs", type=int, default=2)
    ap.add_argument("--batch", type=int, default=32, help="queries (windows) per step")
    ap.add_argument("--max-len", type=int, default=256)
    ap.add_argument("--micro-queries", type=int, default=8, help="queries per forward (gradients accumulated)")
    ap.add_argument("--micro-batch", type=int, default=256, help="pairs per length-sorted forward chunk")
    ap.add_argument("--eval-queries", type=int, default=64)
    ap.add_argument("--eval-batch", type=int, default=256)
    ap.add_argument("--warmup", type=float, default=0.05)
    ap.add_argument("--weight-decay", type=float, default=0.01)
    ap.add_argument("--bf16", action="store_true", help="bf16 autocast for the training forward")
    ap.add_argument("--resamples", type=int, default=10_000)
    ap.add_argument("--train-repos", nargs="*")
    ap.add_argument("--dev-repos", nargs="*")
    ap.add_argument("--max-train-queries", type=int)
    ap.add_argument("--max-dev-queries", type=int)
    L.add_common_args(ap)
    return ap.parse_args(argv)


def main(argv=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    return run(parse(argv), argv)


if __name__ == "__main__":
    sys.exit(main())
