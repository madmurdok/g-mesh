"""Unit tests for gm441_rerank.py.

The corpus, the logit cache and the cross-encoder are stand-ins, so the tests
need no work dir or model; numpy and tokenizers must import (user site).

Run: python3 eval/embedding/test_gm441_rerank.py
"""

import importlib.util
import io
import json
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location("gm441_rerank", HERE / "gm441_rerank.py")
M = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(M)
G, R = M.G, M.R

QUERIES = {
    "q1": {"positive": True, "mechanical": False, "language": "rust", "text": "q1"},
    "q2": {"positive": True, "mechanical": False, "language": "go", "text": "q2"},
}
# Cached ce-minilm logits per query and node. F4 orders the top 30 by
# logit + 80 * cosine: q1 -> c (41.4), b (39.2), a (35.0); q2 -> e (54), d (48).
LOGITS = {"q1": {"a": -5.0, "b": 0.0, "c": 3.0}, "q2": {"d": 0.0, "e": 30.0}}
HITS = {"q1": [["a", 0.50], ["b", 0.49], ["c", 0.48]], "q2": [["d", 0.6], ["e", 0.3]]}
EXPECTED = {"q1": ["c"], "q2": ["e"]}


def stext(node):
    return f"text {node}"


class FakeCorpus:
    def __init__(self, *_args):
        self.name = "c"
        self.queries = QUERIES
        self.lists = {}
        self.stext = {n: stext(n) for n in "abcde"}


def pooled(by_language):
    means = [sum(v) / len(v) for _, v in sorted(by_language.items())]
    return sum(means) / len(means)


def base_metrics(hits):
    """recall@10 and MRR of `hits` per the fixture's queries, language-pooled."""
    r10, mrr = {}, {}
    for q, lst in hits.items():
        ids = [h[0] for h in lst]
        rank = next((i + 1 for i, n in enumerate(ids) if n in EXPECTED[q]), None)
        lang = QUERIES[q]["language"]
        r10.setdefault(lang, []).append(1.0 if rank and rank <= 10 else 0.0)
        mrr.setdefault(lang, []).append(1.0 / rank if rank else 0.0)
    return pooled(r10), pooled(mrr)


class Run(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name) / "schedule"
        self.logits = json.loads(json.dumps(LOGITS))
        self.scored = []

    def tearDown(self):
        self.tmp.cleanup()

    def write_arm(self, arm, hits=HITS, reranked=True):
        d = self.root / arm / "c"
        d.mkdir(parents=True)
        d.joinpath("rankings.jsonl").write_text(
            "".join(json.dumps({"id": q, "expected": EXPECTED[q], "hits": hits[q]}) + "\n" for q in hits))
        r10, mrr = base_metrics(hits)
        checkpoints = [
            {"embedded": 2, "reranked": False, "recall10": 0.0, "mrr": 0.0},
            {"embedded": 5, "reranked": reranked, "recall10": r10, "mrr": mrr},
        ]
        d.joinpath("schedule.json").write_text(json.dumps({"nodes": 5, "checkpoints": checkpoints}))
        return d

    def fake_cache(self):
        test = self

        class Cache:
            def __init__(self, _work, cs, _sha):
                self.d = {c.name: {q: {G.tsha(stext(n)): s for n, s in m.items()} for q, m in test.logits.items()}
                          for c in cs}

            def get(self, c, q, text):
                return self.d[c.name][q][G.tsha(text)]

        return Cache

    def score_missing(self, _cs, cache, need, _ce):
        self.scored.append(sum(len(m) for m in need.values()))
        for (cn, q), m in need.items():
            cache.d[cn][q].update({h: 0.0 for h in m})

    def main(self, *extra):
        argv = ["gm441_rerank.py", "--work", str(Path(self.tmp.name) / "work"), "--schedule-root", str(self.root),
                *extra]
        fake_ce = mock.Mock(return_value=mock.Mock(files={"model.onnx": "sha"}))
        with mock.patch.object(sys, "argv", argv), \
                mock.patch.object(R, "Corpus", FakeCorpus), \
                mock.patch.object(R, "sha256_file", lambda _p: "sha"), \
                mock.patch.object(R, "CrossEncoder", fake_ce), \
                mock.patch.object(G, "Cache", self.fake_cache()), \
                mock.patch.object(G, "score_missing", self.score_missing), \
                mock.patch.object(M, "add_structured_text", lambda _c: None), \
                redirect_stdout(io.StringIO()) as out:
            M.main()
        return out.getvalue()

    def last(self, d):
        return json.loads(d.joinpath("schedule.json").read_text())["checkpoints"][-1]

    def test_f4_reorders_the_top_window_by_logit_plus_80_cosine(self):
        """The reranked rankings and metrics are F4's on a hand-computed fixture.
        Control: score by `BETA * cosine` alone (no logit): q1 keeps a, b, c."""
        dirs = [self.write_arm(arm) for arm in ("id", "tier")]
        out = self.main()
        self.assertIn("identical across ['id', 'tier']: True", out)
        for d in dirs:
            lines = [json.loads(x) for x in d.joinpath("rankings.reranked.jsonl").read_text().splitlines()]
            self.assertEqual([[h[0] for h in x["hits"]] for x in lines], [["c", "b", "a"], ["e", "d"]])
            self.assertAlmostEqual(lines[0]["hits"][0][1], 3.0 + 80 * 0.48)
            last = self.last(d)
            self.assertEqual((last["recall10Reranked"], last["mrrReranked"]), (1.0, 1.0))
            first = json.loads(d.joinpath("schedule.json").read_text())["checkpoints"][0]
            self.assertNotIn("mrrReranked", first)

    def test_a_corrupted_logit_changes_the_reranked_metrics(self):
        """The rerank reads the cached logits; one wrong logit moves q1's answer to rank 2."""
        self.logits["q1"]["a"] = 100.0
        d = self.write_arm("id")
        self.main()
        self.assertEqual(self.last(d)["mrrReranked"], (0.5 + 1.0) / 2)

    def test_arms_whose_reranked_rankings_differ_stop(self):
        """The cross-arm identity check can fail."""
        self.write_arm("id")
        self.write_arm("tier", hits={"q1": HITS["q1"], "q2": HITS["q2"][:1]})
        with self.assertRaises(SystemExit) as stop:
            self.main()
        self.assertIn("differ between arms", str(stop.exception.code))

    def test_missing_pairs_are_scored_once_across_arms_not_fatal(self):
        """A pair missing from the cache is scored (once for both arms) and the run completes.
        Control: restore `if missing: sys.exit("STOP ...")`."""
        del self.logits["q1"]["b"]
        dirs = [self.write_arm(arm) for arm in ("id", "tier")]
        self.main()
        self.assertEqual(self.scored, [1])
        for d in dirs:
            self.assertIn("mrrReranked", self.last(d))

    def test_more_missing_pairs_than_max_score_stop_with_the_count(self):
        """--max-score stops before scoring and names the count."""
        del self.logits["q1"]["b"]
        del self.logits["q2"]["e"]
        self.write_arm("id")
        with self.assertRaises(SystemExit) as stop:
            self.main("--max-score", "1")
        self.assertIn("2 pairs to score exceeds --max-score 1", str(stop.exception.code))
        self.assertEqual(self.scored, [])

    def test_a_schedule_without_a_reranked_100_percent_checkpoint_stops(self):
        """The rerank applies only to the reranked 100% checkpoint."""
        self.write_arm("id", reranked=False)
        with self.assertRaises(SystemExit) as stop:
            self.main()
        self.assertIn("does not end in a reranked 100% checkpoint", str(stop.exception.code))


if __name__ == "__main__":
    unittest.main()
