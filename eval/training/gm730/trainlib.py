"""Shared pieces of the GM-730 trainers (score_teacher.py, train_ce.py,
train_encoder.py): data sets, config identity, the GM-457 checkpoint
contract, length-sorted forward passes, dev metrics and result.json.

Data (`[[data]]` sets of the job TOMLs, make_jobs.py):
    $GM730_WINDOWS/<split>/{windows,texts}.jsonl + manifest.json   (windows.py)
    $GM730_TEACHER/teacher.json + <split>.f32                     (score_teacher.py)
    $GM730_HF/<repo>@<rev>/                                        (snapshots.py)
    $GM456_DATA/v0.4-full/queries.jsonl                            (GM-456)
Without the env variables the laptop defaults under $GM730_DATA are used.

Checkpoint contract (GM-457 runner): <out>/checkpoint.pt, replaced atomically,
holds everything a rerun with the same --out needs to continue; it is written
at every epoch end, every --ckpt-minutes at a step boundary, and on SIGTERM,
after which the trainer exits KILLED_EXIT (75). The batch order of an epoch is
a function of (seed, epoch) only, and every sampling decision of a step of
(seed, epoch, item), so a resumed run replays the uninterrupted run's steps;
dropout uses the torch generator, which the checkpoint restores.
"""

import hashlib
import json
import math
import os
import sys
import time
from pathlib import Path

import numpy as np

# The trainers never reach the Hub: every model is a pinned snapshot directory.
os.environ.setdefault("HF_HUB_OFFLINE", "1")
os.environ.setdefault("TRANSFORMERS_OFFLINE", "1")

HERE = Path(__file__).resolve().parent
for p in (HERE.parent, HERE.parent / "gm436", HERE):
    if str(p) not in sys.path:
        sys.path.insert(0, str(p))
import data as D  # noqa: E402  (gm436)
import dev_metric as DM  # noqa: E402  (gm436)
import snapshots as S  # noqa: E402
from ckpt import KILLED_EXIT, StopOnSignal, load_ckpt, rng_state, save_ckpt, set_rng_state  # noqa: E402



def export_module():
    """This directory's export.py, loaded by path: eval/training/export.py has the same
    module name and gm436/common.py puts its directory first on sys.path."""
    import importlib.util
    name = "gm730_export"
    if name not in sys.modules:
        spec = importlib.util.spec_from_file_location(name, HERE / "export.py")
        mod = importlib.util.module_from_spec(spec)
        sys.modules[name] = mod
        spec.loader.exec_module(mod)
    return sys.modules[name]


VERSION = "v0.4-full"
# argparse dests that never change what a run computes; everything else is the config
NOT_CONFIG = {"out", "device", "allow_device_change", "stop_after_steps", "ckpt_minutes", "threads",
              "job_steps", "eta_limit_minutes"}
# exit code of a step whose S2 probe ETA for the rest of its job is over --eta-limit-minutes
ETA_EXIT = 76
# set to 1 to train on a teacher data set whose C-T did not pass (or was skipped)
ALLOW_UNCHECKED_TEACHER = "GM730_ALLOW_UNCHECKED_TEACHER"


def progress(msg):
    line = f"{time.strftime('%H:%M:%S')} {msg}"
    path = os.environ.get("GM730_PROGRESS")
    if path:
        with open(path, "a") as f:
            f.write(line + "\n")
    print(line, flush=True)


def sha256_bytes(b):
    return hashlib.sha256(b).hexdigest()


def write_json(path, obj):
    tmp = Path(str(path) + ".tmp")
    tmp.write_text(json.dumps(obj, indent=1, sort_keys=True) + "\n")
    os.replace(tmp, path)


def read_jsonl(path):
    with open(path) as f:
        return [json.loads(x) for x in f if x.strip()]


# ---- data ------------------------------------------------------------------------------
def windows_root():
    return Path(os.environ.get("GM730_WINDOWS") or S.data_root() / "windows")


def teacher_root():
    return Path(os.environ.get("GM730_TEACHER") or S.data_root() / "teacher")


def gm456_version_dir(version=VERSION):
    return Path(os.environ.get("GM456_DATA", Path.home() / "Projects/ClaudeProjects/g-mesh-data/gm456")) / version


def pairs_sha256(windows):
    """windows.py's pairsSha256: "<query id>\\t<node key>\\n" over every window entry in order."""
    h = hashlib.sha256()
    for w in windows:
        for key in w["keys"]:
            h.update(f"{w['id']}\t{key}\n".encode())
    return h.hexdigest()


def load_split(split, root=None):
    """(windows, {key: text row}, manifest) of one split; the files' sha256 and the pair
    order must be the manifest's, and its join check must have passed without the planted
    offset (a smoke subset, `smoke: true`, has no join check)."""
    d = Path(root or windows_root()) / split
    man = json.loads((d / "manifest.json").read_text())
    if not man.get("smoke") and not (man.get("joinCheckPass") is True and man.get("plantOffset") is False):
        raise SystemExit(f"{d}: manifest joinCheckPass {man.get('joinCheckPass')}, plantOffset "
                         f"{man.get('plantOffset')}: not a windows set to train or score on")
    for name, sha in man["files"].items():
        got = S.sha256_file(d / name)
        if got != sha:
            raise SystemExit(f"{d / name}: sha256 {got} != manifest {sha}")
    windows = read_jsonl(d / "windows.jsonl")
    if pairs_sha256(windows) != man["pairsSha256"]:
        raise SystemExit(f"{d}: pair order differs from the manifest's pairsSha256")
    texts = {t["key"]: t for t in read_jsonl(d / "texts.jsonl")}
    return windows, texts, man


def texts_in_order(split, root=None):
    """texts.jsonl rows in file order (the run's node order within each repo)."""
    return read_jsonl(Path(root or windows_root()) / split / "texts.jsonl")


def load_teacher(split, windows, root=None, permute_seed=None):
    """{query id: float32 scores over its window}; the array's pair order must be the
    windows' (pairsSha256 and length). permute_seed: each window's scores permuted
    within the window (gm436 data.permute_within, multiset kept)."""
    d = Path(root or teacher_root())
    meta = json.loads((d / "teacher.json").read_text())
    ct = meta.get("ct") or {}
    if ct.get("pass") is not True and os.environ.get(ALLOW_UNCHECKED_TEACHER) != "1":
        raise SystemExit(f"{d}/teacher.json: C-T {'skipped' if 'skipped' in ct else 'did not pass'} "
                         f"({ct.get('maxAbsDiff', ct.get('skipped'))}); {ALLOW_UNCHECKED_TEACHER}=1 to use it anyway")
    sp = meta["splits"][split]
    arr = np.fromfile(d / sp["file"], dtype="<f4")
    if S.sha256_file(d / sp["file"]) != sp["sha256"]:
        raise SystemExit(f"{d / sp['file']}: sha256 differs from teacher.json")
    if sp["pairsSha256"] != pairs_sha256(windows) or len(arr) != sum(len(w["keys"]) for w in windows):
        raise SystemExit(f"{d}: teacher {split} array is not in this windows file's pair order")
    out, s = {}, 0
    for w in windows:
        n = len(w["keys"])
        out[w["id"]] = arr[s:s + n].copy()
        s += n
    if permute_seed is not None:
        out = {k: np.asarray(v, np.float32) for k, v in D.permute_within(out, permute_seed).items()}
    return out


def subset_by_repo(rows, repos=None, limit=None):
    """Rows whose repo is in `repos` (all when None), then the first `limit`."""
    out = [r for r in rows if repos is None or r["repo"] in repos]
    return out[:limit] if limit else out


# ---- run identity ----------------------------------------------------------------------
def config_of(a):
    cfg = {k: v for k, v in sorted(vars(a).items()) if k not in NOT_CONFIG}
    return cfg, sha256_bytes(json.dumps(cfg, sort_keys=True).encode())


def grid_identity(route, argv, out):
    """(name, args) for result.json: the step name (the --out basename, as freeze.py reads
    it) and the args of the make_jobs grid config that argv carries as one contiguous run;
    without one, argv minus the runner's --out/--device."""
    import make_jobs as MJ
    argv = list(argv)
    for teacher in (True, False):
        try:
            configs = MJ.grid(route, teacher)
        except SystemExit:
            continue
        for _, args in configs:
            n = len(args)
            if any(argv[i:i + n] == args for i in range(len(argv) - n + 1)):
                return Path(out).name, list(args)
    rest, skip = [], False
    for x in argv:
        if skip:
            skip = False
            continue
        if x in ("--out", "--device"):
            skip = True
            continue
        rest.append(x)
    return Path(out).name, rest


# ---- torch setup -----------------------------------------------------------------------
def setup_torch(a):
    """Seeds, threads, deterministic algorithms; returns the torch.device."""
    import random
    import torch
    device = torch.device(a.device)
    if device.type == "cuda":
        os.environ.setdefault("CUBLAS_WORKSPACE_CONFIG", ":4096:8")
        if not torch.cuda.is_available():
            raise SystemExit("--device cuda: CUDA is not available on this host")
        torch.backends.cuda.matmul.allow_tf32 = False
        torch.backends.cudnn.allow_tf32 = False
        torch.backends.cudnn.benchmark = False
    torch.set_num_threads(a.threads)
    # warn_only: an op without a deterministic kernel warns instead of stopping a paid job
    torch.use_deterministic_algorithms(True, warn_only=True)
    random.seed(a.seed)
    np.random.seed(a.seed)
    torch.manual_seed(a.seed)
    return device


def determinism_note(a):
    """C-D: what determinism this run had."""
    import torch
    return {"seed": a.seed, "device": a.device, "threads": a.threads, "torch": torch.__version__,
            "cublasWorkspaceConfig": os.environ.get("CUBLAS_WORKSPACE_CONFIG"),
            "deterministicAlgorithms": torch.are_deterministic_algorithms_enabled(),
            "deterministicWarnOnly": torch.is_deterministic_algorithms_warn_only_enabled(),
            "cudaDevice": torch.cuda.get_device_name(0) if a.device == "cuda" and torch.cuda.is_available() else None}


def autocast(a, device):
    """bf16 autocast when --bf16 (any device), else a no-op context."""
    import contextlib
    import torch
    if getattr(a, "bf16", False):
        return torch.autocast(device_type=device.type, dtype=torch.bfloat16)
    return contextlib.nullcontext()


def linear_warmup(total_steps, warmup_frac=0.05):
    """LambdaLR factor: linear warmup over ceil(warmup_frac * total) steps, then linear decay to 0."""
    warm = max(1, int(math.ceil(warmup_frac * total_steps)))

    def f(step):
        if step < warm:
            return (step + 1) / warm
        return max(0.0, (total_steps - step) / max(1, total_steps - warm))
    return f


def trainable_state(model):
    """{name: CPU copy} of the parameters that train (all of a full fine-tune, the adapters of LoRA)."""
    return {n: p.detach().cpu().clone() for n, p in model.named_parameters() if p.requires_grad}


def load_trainable(model, sd):
    import torch
    named = {n: p for n, p in model.named_parameters() if p.requires_grad}
    if set(named) != set(sd):
        raise SystemExit(f"checkpoint parameters differ from the model's trainable ones "
                         f"({len(set(named) ^ set(sd))} names differ)")
    with torch.no_grad():
        for n, p in named.items():
            p.copy_(sd[n].to(p.device, p.dtype))


def params_sha256(sd):
    """sha256 over the sorted names and the float32 bytes of a state dict."""
    h = hashlib.sha256()
    for n in sorted(sd):
        h.update(n.encode())
        h.update(sd[n].detach().cpu().float().contiguous().numpy().tobytes())
    return h.hexdigest()


def count_params(model):
    return {"total": sum(p.numel() for p in model.parameters()),
            "trainable": sum(p.numel() for p in model.parameters() if p.requires_grad)}


# ---- tokens and length-sorted passes ---------------------------------------------------------
def tokenizer(snap_dir, max_len):
    from tokenizers import Tokenizer
    tok = Tokenizer.from_file(str(Path(snap_dir) / "tokenizer.json"))
    tok.enable_truncation(max_len)
    tok.no_padding()
    return tok


def pad_id(tok):
    return next((i for i in (tok.token_to_id(t) for t in ("[PAD]", "<pad>")) if i is not None), 0)


def collate(encs, pad, device, token_types=False):
    """Padded input tensors of encodings (padding to the longest of this list)."""
    import torch
    L = max(len(e.ids) for e in encs)
    ids = np.full((len(encs), L), pad, np.int64)
    mask = np.zeros((len(encs), L), np.int64)
    tt = np.zeros((len(encs), L), np.int64)
    for r, e in enumerate(encs):
        n = len(e.ids)
        ids[r, :n], mask[r, :n], tt[r, :n] = e.ids, 1, e.type_ids
    out = {"input_ids": torch.from_numpy(ids).to(device), "attention_mask": torch.from_numpy(mask).to(device)}
    if token_types:
        out["token_type_ids"] = torch.from_numpy(tt).to(device)
    return out


def sorted_chunks(encs, chunk):
    """Index chunks of `encs` sorted by length (stable), so each forward pads little."""
    order = sorted(range(len(encs)), key=lambda i: len(encs[i].ids))
    return [order[s:s + chunk] for s in range(0, len(order), chunk)]


def forward_sorted(fn, encs, chunk, pad, device, token_types=False):
    """fn(feed) -> (n, ...) tensor for every encoding, run in length-sorted chunks and put
    back in input order. Gradients flow through when fn builds a graph."""
    import torch
    parts, idx = [], []
    for c in sorted_chunks(encs, chunk):
        parts.append(fn(collate([encs[i] for i in c], pad, device, token_types)))
        idx += c
    out = torch.cat(parts, 0)
    inv = torch.empty(len(idx), dtype=torch.long)
    inv[torch.tensor(idx)] = torch.arange(len(idx))
    return out[inv.to(out.device)]


def bucketed_batches(lengths, batch, seed, epoch, chunk=50):
    """One epoch's batches: a (seed, epoch) permutation cut into runs of chunk * batch items,
    each run sorted by length and cut into batches, then the batches shuffled with the same
    generator. Every item appears once; padding stays within similar lengths."""
    lengths = np.asarray(lengths)
    rng = np.random.default_rng([seed, epoch])
    perm = rng.permutation(len(lengths))
    out = []
    for s in range(0, len(perm), batch * chunk):
        part = perm[s:s + batch * chunk]
        part = part[np.argsort(lengths[part], kind="stable")]
        out += [part[i:i + batch] for i in range(0, len(part), batch)]
    return [out[i] for i in rng.permutation(len(out))]


# ---- dev metric and result.json -----------------------------------------------------------
def window_ranks(scores, window):
    """1-based first-hit rank inside a window (hits: positives and answersAlso; ties to the
    earlier window entry); inf when the window holds no hit."""
    rel = sorted(set(window["positives"]) | set(window["answersAlso"]))
    return float(DM.first_hit_ranks([np.asarray(scores, np.float64)], [rel])[0])


def summarize(evals):
    """{mrr, recall@10} of [(id, target, rank)]."""
    return DM.metrics([r for *_, r in evals])


def result_json(route, name, args, best_epoch, base_evals, best_evals, seed, resamples):
    """freeze.py's result.json: best, base and the paired (best - base) MRR gain over the
    same dev queries, clustered by target."""
    if [q for q, *_ in base_evals] != [q for q, *_ in best_evals]:
        raise SystemExit("base and best dev evals cover different queries")
    b, s = summarize(base_evals), summarize(best_evals)
    g = DM.paired_bootstrap(DM.rr_values(base_evals), DM.rr_values(best_evals), [t for _, t, _ in best_evals],
                            resamples, seed)
    return {"route": route, "name": name, "args": list(args),
            "best": {"epoch": int(best_epoch), "mrr": s["mrr"], "recall@10": s["recall@10"]},
            "base": {"mrr": b["mrr"], "recall@10": b["recall@10"]},
            "gain": {"mrr": g["point"], "lower": g["lower"], "upper": g["upper"]}}


# ---- checkpoints ----------------------------------------------------------------------------
class Checkpointer:
    """The GM-457 checkpoint contract around one trainer's loop."""

    def __init__(self, a, cfg_sha):
        self.out = Path(a.out)
        self.out.mkdir(parents=True, exist_ok=True)
        self.path = self.out / "checkpoint.pt"
        self.a, self.cfg_sha = a, cfg_sha
        self.every = float(a.ckpt_minutes) * 60
        self.last = time.monotonic()
        self.signal = StopOnSignal()

    def __enter__(self):
        self.signal.__enter__()
        return self

    def __exit__(self, *exc):
        return self.signal.__exit__(*exc)

    def load(self):
        """The checkpoint's state, or None; refuses another config or device."""
        if not self.path.exists():
            return None
        st = load_ckpt(self.path)
        if st["configSha256"] != self.cfg_sha:
            raise SystemExit(f"{self.path}: written for config {st['configSha256'][:12]}, this run is {self.cfg_sha[:12]}")
        if st.get("device", "cpu") != self.a.device and not self.a.allow_device_change:
            raise SystemExit(f"{self.path}: written on device {st.get('device')}, this run is {self.a.device} "
                             "(--allow-device-change to resume anyway)")
        if st.get("rng") is not None:
            try:
                set_rng_state(st["rng"], self.a.allow_device_change)
            except ValueError as e:
                raise SystemExit(f"{self.path}: {e}")
        st["device"] = self.a.device
        return st

    def save(self, state):
        state = dict(state, configSha256=self.cfg_sha, device=self.a.device, rng=rng_state())
        save_ckpt(self.path, state)
        self.last = time.monotonic()

    def at_step(self, global_step, state_fn):
        """After a finished step: save on time, and on SIGTERM or --stop-after-steps also
        exit KILLED_EXIT. state_fn() builds the state only when it is saved."""
        stop = self.signal.requested or (self.a.stop_after_steps and global_step >= self.a.stop_after_steps)
        if stop or time.monotonic() - self.last >= self.every:
            self.save(state_fn())
        if stop:
            progress(f"{self.out}: stopped after step {global_step}, checkpoint saved")
            sys.exit(KILLED_EXIT)


def add_common_args(ap, seed_default=730):
    ap.add_argument("--seed", type=int, default=seed_default)
    ap.add_argument("--threads", type=int, default=max(1, min(8, os.cpu_count() or 1)),
                    help="torch CPU threads; not part of the config")
    ap.add_argument("--device", choices=["cpu", "cuda"], default="cpu", help="not part of the config")
    ap.add_argument("--allow-device-change", action="store_true", help="resume a checkpoint written on another device")
    ap.add_argument("--ckpt-minutes", type=float, default=5.0,
                    help="checkpoint at the first step boundary after this many minutes (and every epoch end)")
    ap.add_argument("--stop-after-steps", type=int,
                    help="save a checkpoint and exit 75 after this global step (resume test, as a SIGTERM)")
    ap.add_argument("--job-steps", type=int, default=1,
                    help="steps of the job from this one to its last, this one included (job ETA); not part of the config")
    ap.add_argument("--eta-limit-minutes", type=float,
                    help=f"exit {ETA_EXIT} after the probe when the job ETA is over this (S2); not part of the config")
    ap.add_argument("--out", required=True)


class Probe:
    """S2 probe: s/step of the first `n` timed steps, this step's ETA and the job's (this
    step's rest plus job_steps - 1 more steps of the same length), printed once; exits
    ETA_EXIT when the job ETA is over limit_minutes."""

    def __init__(self, label, total_steps, n=3, job_steps=1, limit_minutes=None):
        self.label, self.total, self.n = label, total_steps, n
        self.job_steps, self.limit = max(1, int(job_steps or 1)), limit_minutes
        self.t0, self.k, self.rate = None, 0, None

    @classmethod
    def of(cls, label, total_steps, a):
        return cls(label, total_steps, job_steps=getattr(a, "job_steps", 1),
                   limit_minutes=getattr(a, "eta_limit_minutes", None))

    def tick(self, done_steps):
        now = time.monotonic()
        if self.t0 is None:
            self.t0 = now
            return
        self.k += 1
        if self.k == self.n:
            self.rate = (now - self.t0) / self.n
            eta = self.rate * max(0, self.total - done_steps)
            job_eta = eta + self.rate * self.total * (self.job_steps - 1)
            progress(f"PROBE {self.label}: {self.rate:.3f} s/step, {self.total} steps, ETA {eta / 60:.1f} min; "
                     f"job ETA {job_eta / 60:.1f} min over {self.job_steps} step(s)"
                     + (f", limit {self.limit:g} min" if self.limit else ""))
            if self.limit and job_eta / 60 > self.limit:
                progress(f"PROBE {self.label}: job ETA {job_eta / 60:.1f} min > --eta-limit-minutes {self.limit:g}: "
                         f"stop (S2), exit {ETA_EXIT}")
                sys.exit(ETA_EXIT)
