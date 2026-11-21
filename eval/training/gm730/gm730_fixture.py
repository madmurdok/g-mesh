"""A tiny world for the GM-730 tests: a GM-456 version dir and Rust run (the GM-436
fixture), windows and texts built by windows.py from it, a teacher array, and tiny random
BERT snapshots under the pinned snapshot names. Everything lives under one temp dir and is
reached through environment variables (`World.env`); nothing is downloaded.

The snapshots (`World.write_snapshots`) need torch, transformers and tokenizers; the rest
needs numpy only.
"""

import hashlib
import json
import os
import sys
from pathlib import Path
from unittest import mock

import numpy as np

HERE = Path(__file__).resolve().parent
for p in (HERE.parent / "gm436", HERE):
    if str(p) not in sys.path:
        sys.path.insert(0, str(p))
import fixture_gm436 as FX  # noqa: E402
import snapshots as S  # noqa: E402
import windows as W  # noqa: E402

VERSION = "v0.4-full"
K_WINDOW = 8
WORDS = ["find", "variant", "body", "src", "0", "1", "2", "3"]
SPECIAL = ["[PAD]", "[UNK]", "[CLS]", "[SEP]"]


def sha256_bytes(b):
    return hashlib.sha256(b).hexdigest()


class World:
    def __init__(self, root, repos=("alpha",), nodes=24, train_targets=6, dev_targets=3, per_target=3):
        self.root = Path(root)
        self.nodes = nodes
        with mock.patch.object(FX, "VERSION", VERSION):
            self.fx = FX.Fixture(self.root / "fx", repos=repos, nodes=nodes, train_targets=train_targets,
                                 dev_targets=dev_targets, per_target=per_target)
        self.windows_root = self.root / "windows"
        self.teacher_root = self.root / "teacher"
        self.hf_root = self.root / "hf"
        self.data_root = self.root / "gm730"
        self.patches = []

    # ---- environment ---------------------------------------------------------------
    def env(self):
        return {**self.fx.env(), "GM730_WINDOWS": str(self.windows_root), "GM730_TEACHER": str(self.teacher_root),
                "GM730_HF": str(self.hf_root), "GM730_DATA": str(self.data_root)}

    def child_env(self):
        e = {k: v for k, v in os.environ.items() if not k.startswith(("GM730_", "GM456_", "GM436_"))}
        e.update(self.env())
        e["PYTHONDONTWRITEBYTECODE"] = "1"
        e["TOKENIZERS_PARALLELISM"] = "false"
        return e

    def activate(self, case):
        """Put the world's variables in os.environ and windows.py's variant on the fixture's for `case`."""
        p = mock.patch.dict(os.environ, self.env())
        p.start()
        case.addCleanup(p.stop)
        v = mock.patch.object(W, "VARIANT", FX.VARIANT)
        v.start()
        case.addCleanup(v.stop)

    # ---- windows ---------------------------------------------------------------------
    def import_vectors(self):
        for repo in self.fx.repos:
            W.K.import_run(self.fx.mine, self.fx.vdir, FX.VARIANT, repo)

    def build_windows(self, split, plant_offset=False):
        return W.build_windows(split, K_WINDOW, plant_offset, VERSION)[0]

    def text_rows(self):
        return [{"key": FX.key_of(repo, i), "repo": repo, "structured": f"f{i} src f{i}", "ship": f"f{i} body"}
                for repo in self.fx.repos for i in range(self.nodes)]

    def write_split(self, split, windows=None, join_pass=True, plant_offset=False, smoke=False):
        """<windows root>/<split>/{windows,texts}.jsonl and manifest.json (windows.build's schema)."""
        windows = self.build_windows(split) if windows is None else windows
        out = self.windows_root / split
        out.mkdir(parents=True, exist_ok=True)
        files = {"windows.jsonl": W.write_jsonl(out / "windows.jsonl", windows),
                 "texts.jsonl": W.write_jsonl(out / "texts.jsonl", self.text_rows())}
        man = {"split": split, "k": K_WINDOW, "plantOffset": plant_offset, "files": files,
               "pairsSha256": W.pairs_sha256(windows), "joinCheckPass": join_pass}
        if smoke:
            man["smoke"] = True
        (out / "manifest.json").write_text(json.dumps(man, indent=1))
        return windows, man

    def write_splits(self):
        self.import_vectors()
        return {s: self.write_split(s)[0] for s in ("train", "dev")}

    # ---- teacher -----------------------------------------------------------------------
    def write_teacher(self, windows_by_split, ct=None, seed=7):
        """teacher.json + <split>.f32 in score_teacher.py's layout; scores are seeded normals."""
        self.teacher_root.mkdir(parents=True, exist_ok=True)
        rng = np.random.default_rng(seed)
        meta = {"splits": {}, "ct": ct if ct is not None else {"check": "C-T", "pass": True, "maxAbsDiff": 0.01}}
        for split, windows in windows_by_split.items():
            n = sum(len(w["keys"]) for w in windows)
            arr = rng.normal(size=n).astype("<f4")
            f = self.teacher_root / f"{split}.f32"
            arr.tofile(f)
            meta["splits"][split] = {"file": f.name, "sha256": sha256_bytes(f.read_bytes()), "pairs": n,
                                     "pairsSha256": W.pairs_sha256(windows)}
        (self.teacher_root / "teacher.json").write_text(json.dumps(meta, indent=1))
        return meta

    # ---- tiny models -------------------------------------------------------------------
    def vocab(self):
        return SPECIAL + WORDS + [f"f{i}" for i in range(self.nodes)]

    def write_snapshots(self, names=("l6", "gte-mb", "gte-small", "jina"), seed=0):
        import torch
        import transformers
        from tokenizers import Tokenizer, models, pre_tokenizers, processors
        vocab = self.vocab()
        tok = Tokenizer(models.WordLevel({w: i for i, w in enumerate(vocab)}, unk_token="[UNK]"))
        tok.pre_tokenizer = pre_tokenizers.Whitespace()
        cls, sep = vocab.index("[CLS]"), vocab.index("[SEP]")
        tok.post_processor = processors.TemplateProcessing(
            single="[CLS] $A [SEP]", pair="[CLS] $A [SEP] $B:1 [SEP]:1", special_tokens=[("[CLS]", cls), ("[SEP]", sep)])
        cfg = transformers.BertConfig(vocab_size=len(vocab), hidden_size=16, num_hidden_layers=1, num_attention_heads=2,
                                      intermediate_size=32, max_position_embeddings=64, pad_token_id=0, num_labels=1)
        for name in names:
            torch.manual_seed(seed)
            model = (transformers.BertForSequenceClassification(cfg) if name in ("l6", "gte-mb") else transformers.BertModel(cfg))
            d = self.hf_root / f"{S.PINS[name][0].split('/')[-1]}@{S.PINS[name][1]}"
            d.mkdir(parents=True, exist_ok=True)
            model.save_pretrained(d)
            tok.save(str(d / "tokenizer.json"))
        return vocab
