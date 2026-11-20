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
pairs, quint8 vs fp32 at chunk 1, and C-P2: the quint8 dev MRR at chunk 1 on the
first 300 dev windows vs the fp32 export's, against the base L6's own gap).

--stage qat (docs/design/GM-734-rr-qat.md): quantization-aware fine-tune of a trained
RR model. Starts from the parameters of its fp32 export (--init-onnx, refused unless
their sha256 is --init-params-sha256), trains --steps steps of the same loss through
qat.py's emulation of export.quantize("l6") (fp32, no --bf16), over the epoch-1 batch
order of --seed. Dev rows at step 0, every --eval-every steps and at the last step:
emulated quint8 MRR over all dev windows, plus fp32 and emulated quint8 on the first
CP2_WINDOWS (the C-P2 subset); once at the start the base L6's fp32 and emulated quint8
over all dev windows. Selection: the predicted C-P2 margin on the subset
m = q300 - (f300 - --cp2-allowance) - --cp2-safety only filters; among rows with
m >= 0 the highest full-dev emulated quint8 MRR wins (ties to the earlier step), and
without one the highest full-dev emulated quint8 MRR. The selected parameters go into a
fresh (unwrapped, default-attention) model and through the --stage final export, C-P and
C-P2 unchanged; export.json adds qatFidelity (emulated torch vs ORT quint8 logits at
chunk 1). result.json's base is the base L6's emulated quint8 row, its best the selected
step (`epoch` holds the step).

usage: train_ce.py --stage grid|final --lr X --tau-t X --alpha X --out DIR [--control shuffled-teacher] [options]
       train_ce.py --stage qat --init-onnx F --init-params-sha256 H --steps N --lr X --tau-t X --alpha X --out DIR [options]
"""

import argparse
import os
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
CP2_WINDOWS = 300  # dev windows of the quint8 C-P2 check (chunk 1)
CP2_MARGIN = 0.01


def load_model(device, attn_implementation=None):
    """The base L6 cross-encoder, fp32; `attn_implementation` "eager" for the qat wrappers,
    None (transformers' default) for everything that is exported."""
    import torch
    from transformers import AutoModelForSequenceClassification
    kw = {"attn_implementation": attn_implementation} if attn_implementation else {}
    m = AutoModelForSequenceClassification.from_pretrained(str(L.S.path_of(SNAPSHOT)), torch_dtype=torch.float32, **kw)
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


def dev_eval(model, tok, data, device, a, windows=None):
    """[(query id, target, rank)] over `windows` (default: all dev windows), in order."""
    import torch
    model.eval()
    out = []
    pad = L.pad_id(tok)
    windows = data.dev if windows is None else windows
    for s in range(0, len(windows), a.eval_queries):
        ws = windows[s:s + a.eval_queries]
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


def ort_window_evals(onnx_path, tok, data, windows, batch):
    """[(query id, target, rank)] of `windows` scored by an ONNX cross-encoder on the CPU,
    `batch` pairs per ORT run (1 for a dynamic-int8 model)."""
    X = L.export_module()
    f = X.ort_forward(X.ort_session(onnx_path))
    out = []
    for w in windows:
        z = np.concatenate([f(feed).reshape(-1) for feed in X.batches(tok, data.pairs(w), batch)])
        out.append((w["id"], w["target"], L.window_ranks(z, w)))
    return out


def quint8_check(fp32_onnx, q_onnx, tok, data, out):
    """C-P2 for the quint8 student on the first CP2_WINDOWS dev windows: its dev MRR (chunk 1)
    >= the fp32 export's - (the base L6's fp32 -> quint8 gap through the same export and
    recipe + CP2_MARGIN)."""
    X = L.export_module()
    snap = L.S.path_of(SNAPSHOT)
    sub = data.dev[:CP2_WINDOWS]
    tmp = out / "_base_tmp"
    shutil.rmtree(tmp, ignore_errors=True)
    t = time.monotonic()
    try:
        X.export_dir(X.load_torch(EXPORT_KEY, snap), EXPORT_KEY, tmp / "fp32", snap)
        X.quantize(EXPORT_KEY, tmp / "fp32" / "model.onnx", tmp / "quint8" / "model.onnx")
        mrr = {k: L.summarize(ort_window_evals(p, tok, data, sub, b))["mrr"]
               for k, p, b in (("fp32", fp32_onnx, 16), ("quint8", q_onnx, 1),
                               ("baseFp32", tmp / "fp32" / "model.onnx", 16),
                               ("baseQuint8", tmp / "quint8" / "model.onnx", 1))}
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    gap = mrr["baseFp32"] - mrr["baseQuint8"]
    floor = mrr["fp32"] - (gap + CP2_MARGIN)
    return {"check": "C-P2", "windows": len(sub), "quint8DevMrr": mrr["quint8"], "fp32DevMrr": mrr["fp32"],
            "baseFp32DevMrr": mrr["baseFp32"], "baseQuint8DevMrr": mrr["baseQuint8"], "baseGap": gap,
            "margin": CP2_MARGIN, "floor": floor, "pass": mrr["quint8"] >= floor, "cpuSeconds": time.monotonic() - t}


def qat_fidelity(emulated, q_onnx, ptok, pairs):
    """Emulated quint8 (a qat-wrapped torch model) vs the ORT quint8 file, one pair per run
    (chunk 1): max and median |logit diff|."""
    X = L.export_module()
    ft, fo = X.torch_forward(emulated, EXPORT_KEY), X.ort_forward(X.ort_session(q_onnx))
    d = np.array([float(np.abs(ft(feed).reshape(-1) - fo(feed).reshape(-1)).max()) for feed in X.batches(ptok, pairs, 1)])
    return {"n": len(d), "chunk": 1, "maxAbsDiff": float(d.max()) if len(d) else 0.0,
            "medianAbsDiff": float(np.median(d)) if len(d) else 0.0}


def export_final(model, tok_dev, data, a, out, fidelity_model=None):
    """onnx-fp32 + onnx-quint8 of the loaded (best) model, C-P and the quint8 C-P2; with
    `fidelity_model` (the qat-wrapped model holding the same parameters) also qatFidelity."""
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
    if fidelity_model is not None:
        res["qatFidelity"] = qat_fidelity(fidelity_model, q_dir / "model.onnx", ptok, pairs[:QUANT_N])
    res["cp2"] = quint8_check(fp32_dir / "model.onnx", q_dir / "model.onnx", tok_dev, data, out)
    L.write_json(out / "export.json", res)
    c2 = res["cp2"]
    L.progress(f"{out}: export C-P max |diff| {cp['maxAbsDiff']:.3g} -> {'pass' if cp['pass'] else 'FAIL'}; "
               f"quint8 vs fp32 {qd:.3g}; C-P2 quint8 {c2['quint8DevMrr']:.4f} >= {c2['floor']:.4f} -> "
               f"{'pass' if c2['pass'] else 'FAIL'}")
    return res


def run(a, argv):
    import torch
    if a.stage == "qat":
        return run_qat(a, argv)
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
        probe = L.Probe.of(f"rr {name}", total_steps, a)
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
    ex = info.get("export", {})
    ok = ex.get("cp", {}).get("pass", True) and ex.get("cp2", {}).get("pass", True)
    return 0 if ok else 1


def selection_key(row):
    """Order of the qat selection: rows with a non-negative predicted C-P2 margin first, then
    the full-dev emulated quint8 MRR; the selected row is the first maximum."""
    return (bool(row["eligible"]), row["quint8Emu"]["mrr"])


def select_row(rows):
    """Index of the selected qat dev row (first maximum of selection_key), or None."""
    best = None
    for i, r in enumerate(rows):
        if best is None or selection_key(r) > selection_key(rows[best]):
            best = i
    return best


def predicted_margin(q300, f300, a):
    """Predicted C-P2 margin on the subset: emulated quint8 minus the gate's floor
    (fp32 - allowance), less a safety for the instance-vs-emulation noise."""
    return q300 - (f300 - a.cp2_allowance) - a.cp2_safety


def init_path(arg):
    """--init-onnx with `~` and `$VAR` expanded (a job passes `$GM730_INIT/model.onnx`; the
    config keeps the unexpanded text, so it is the same on the laptop and the instance)."""
    p = os.path.expandvars(os.path.expanduser(arg))
    if "$" in p:
        raise SystemExit(f"--init-onnx {arg}: an environment variable is not set")
    return Path(p)


def run_qat(a, argv):
    import torch
    import qat as Q
    if a.bf16:
        raise SystemExit("--stage qat trains in fp32 (bf16 would blur the 255-level grid); drop --bf16")
    if a.control != "none":
        raise SystemExit("--stage qat has no control")
    if not a.init_onnx or a.steps is None or a.steps < 0 or a.eval_every < 1:
        raise SystemExit("--stage qat needs --init-onnx, --steps >= 0 and --eval-every >= 1")
    cfg, cfg_sha = L.config_of(a)
    out = Path(a.out)
    device = L.setup_torch(a)
    name, grid_args = L.grid_identity(ROUTE, argv, out)
    data = Data(a)
    tok = L.tokenizer(L.S.path_of(SNAPSHOT), a.max_len)
    model = load_model(device, "eager")
    init_sha = L.init_from_onnx(model, init_path(a.init_onnx), a.init_params_sha256)
    Q.wrap(model)
    model.train()
    autocast = L.autocast(a, device)
    opt = torch.optim.AdamW(model.parameters(), lr=a.lr, weight_decay=a.weight_decay)
    sched = torch.optim.lr_scheduler.LambdaLR(opt, L.linear_warmup(max(1, a.steps), a.warmup))
    lengths = [sum(len(data.texts[k]["ship"]) for k in w["keys"]) for w in data.train]
    per_epoch = -(-len(data.train) // a.batch)
    orders = {}

    def batch_at(g):
        """The g-th (0-based) batch: epoch 1 + g // per_epoch of the (seed, epoch) order."""
        epoch = 1 + g // per_epoch
        if epoch not in orders:
            orders[epoch] = L.bucketed_batches(lengths, a.batch, a.seed, epoch)
        return [data.train[i] for i in orders[epoch][g % per_epoch]]

    sub = data.dev[:CP2_WINDOWS]
    L.progress(f"{out}: qat from {a.init_onnx} (params {init_sha[:12]}), {len(data.train)} train / {len(data.dev)} "
               f"dev windows, {a.steps} steps, eval every {a.eval_every}")

    def eval_rows(m):
        """(full-dev emulated quint8 evals, fp32 evals on the subset)."""
        evq = dev_eval(m, tok, data, device, a)
        with Q.bypass(m):
            evf = dev_eval(m, tok, data, device, a, windows=sub)
        return evq, evf

    def eval_point(st, step):
        t = time.monotonic()
        evq, evf = eval_rows(model)
        q300, f300 = L.summarize(evq[:len(sub)])["mrr"], L.summarize(evf)["mrr"]
        margin = predicted_margin(q300, f300, a)
        row = {"step": step, "quint8Emu": L.summarize(evq), "fp32Sub": L.summarize(evf),
               "quint8EmuSub": L.summarize(evq[:len(sub)]), "subWindows": len(sub), "margin": margin,
               "eligible": margin >= 0,
               "loss": float(np.mean(st["losses"])) if st["losses"] else None, "devSeconds": time.monotonic() - t}
        st["history"].append(row)
        st["losses"] = []
        if st["selected"] is None or selection_key(row) > selection_key(st["selected"]):
            st.update(selected=row, bestEvals=evq, bestParams=L.trainable_state(model))
        L.progress(f"{out}: step {step}/{a.steps} loss {row['loss']} emulated quint8 dev MRR "
                   f"{row['quint8Emu']['mrr']:.4f}; subset q {q300:.4f} fp32 {f300:.4f} margin {margin:+.4f}; "
                   f"selected step {st['selected']['step']}")

    with L.Checkpointer(a, cfg_sha) as ck:
        st = ck.load()
        if st is not None:
            L.load_trainable(model, st["params"])
            opt.load_state_dict(st["opt"])
            sched.load_state_dict(st["sched"])
            L.progress(f"{out}: resumed at step {st['step']}")
        else:
            t = time.monotonic()
            base = Q.wrap(load_model(device, "eager"))
            bq = dev_eval(base, tok, data, device, a)
            with Q.bypass(base):
                bf = dev_eval(base, tok, data, device, a)
            del base
            st = {"step": 0, "history": [], "losses": [], "selected": None, "bestEvals": None, "bestParams": None,
                  "baseEvals": bq,
                  "baseRows": {"fp32": L.summarize(bf), "quint8Emu": L.summarize(bq),
                               "fp32Sub": L.summarize(bf[:len(sub)]), "quint8EmuSub": L.summarize(bq[:len(sub)]),
                               "devSeconds": time.monotonic() - t},
                  "trainSeconds": 0.0}
            L.progress(f"{out}: base L6 dev MRR fp32 {st['baseRows']['fp32']['mrr']:.4f}, emulated quint8 "
                       f"{st['baseRows']['quint8Emu']['mrr']:.4f}")
            eval_point(st, 0)
        probe = L.Probe.of(f"rr qat {name}", a.steps, a)
        t_run = time.monotonic()

        def state():
            return dict(st, params=L.trainable_state(model), opt=opt.state_dict(), sched=sched.state_dict(),
                        trainSeconds=st["trainSeconds"] + time.monotonic() - t_run)

        if st["step"] == 0 and a.steps == 0:
            ck.save(state())
        while st["step"] < a.steps:
            g = st["step"]
            st["losses"].append(train_step(model, opt, sched, tok, data, batch_at(g), device, a, autocast))
            st["step"] = g + 1
            probe.tick(st["step"])
            if st["step"] % a.eval_every == 0 or st["step"] == a.steps:
                eval_point(st, st["step"])
            ck.at_step(st["step"], state)
            if st["step"] == a.steps:
                ck.save(state())
        st["trainSeconds"] += time.monotonic() - t_run

    sel = st["selected"]
    if st["history"][select_row(st["history"])]["step"] != sel["step"]:
        raise SystemExit("qat selection: the running choice differs from select_row over the history")
    res = L.result_json(ROUTE, name, grid_args, sel["step"], st["baseEvals"], st["bestEvals"], a.seed, a.resamples)
    res["best"]["step"] = sel["step"]
    info = {"config": cfg, "configSha256": cfg_sha, "stage": a.stage, "control": a.control, "history": st["history"],
            "baseRows": st["baseRows"], "initOnnx": str(a.init_onnx), "initParamsSha256": init_sha,
            "selectedStep": sel["step"], "selectedEligible": sel["eligible"],
            "bestParamsSha256": L.params_sha256(st["bestParams"]),
            "lastParamsSha256": L.params_sha256(L.trainable_state(model)), "params": L.count_params(model),
            "queries": {"train": len(data.train), "dev": len(data.dev)}, "steps": a.steps,
            "stepsPerEpoch": per_epoch, "trainSeconds": st["trainSeconds"], "determinism": L.determinism_note(a),
            "checkpointBytes": (out / "checkpoint.pt").stat().st_size}
    L.load_trainable(model, st["bestParams"])
    Q.enabled(model, True)
    model.eval()
    fresh = load_model(device)
    L.load_trainable(fresh, st["bestParams"])
    info["export"] = export_final(fresh, tok, data, a, out, fidelity_model=model)
    L.write_json(out / "train.json", info)
    L.write_json(out / "result.json", res)
    L.progress(f"{out}: done; selected step {sel['step']} ({'margin ok' if sel['eligible'] else 'no row passed the margin'}),"
               f" emulated quint8 dev MRR {sel['quint8Emu']['mrr']:.4f}")
    ex = info["export"]
    return 0 if ex["cp"]["pass"] and ex["cp2"]["pass"] else 1


QAT_ARGS = ("init_onnx", "init_params_sha256", "steps", "eval_every", "cp2_allowance", "cp2_safety")


def parse(argv):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--stage", choices=["grid", "final", "qat"], required=True)
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
    q = ap.add_argument_group("--stage qat")
    q.add_argument("--init-onnx", help="fp32 export whose parameters the fine-tune starts from")
    q.add_argument("--init-params-sha256", help="refuse --init-onnx unless its parameters have this params_sha256")
    q.add_argument("--steps", type=int, help="optimiser steps")
    q.add_argument("--eval-every", type=int, default=100, help="dev rows every this many steps (and at 0 and the end)")
    q.add_argument("--cp2-allowance", type=float, default=0.0023,
                   help="the C-P2 floor's distance below fp32 (instance base gap + CP2_MARGIN)")
    q.add_argument("--cp2-safety", type=float, default=0.0015, help="emulation / instance noise taken off the margin")
    L.add_common_args(ap)
    a = ap.parse_args(argv)
    if a.stage != "qat":
        # a grid / final config (and its sha) does not depend on the qat options
        for k in QAT_ARGS:
            delattr(a, k)
    return a


def main(argv=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    return run(parse(argv), argv)


if __name__ == "__main__":
    sys.exit(main())
