"""Unit tests for the GM-452/S2 arm parameterisation of the eval scripts.

Covers gm697_floor_refit.py --report/--report-arm, shipped_floor_rates.py
--after with --floors/--custom, --st-runs in gm701_floor_or_blend.py and
gm461_learned_verdict.py, the paired per-language bootstrap
gm452_paired_bootstrap.py, and that the default (no new flag) output equals
the pre-change scripts (efbdf7ff~); GM-452/S7: gm461 --per-query-out, the bootstrap's
stack mode (--base-stack/--stack/--base-arm/--stack-arm) and its NaN-resample count.
GM-452/S12: gm461 --floors-json/--floors-arm, --beta, --tune-beta-on-fit (FIT queries
only) and the base-arm gating of controls 1 and 2b.

Synthetic fixtures only: tiny run/eval dirs in a temp dir; g-mesh-data and the
held-out eval data are never read. numpy must import; the regression group
needs git and the efbdf7ff~ commit.

Run: ~/Projects/ClaudeProjects/g-mesh-data/gm436/venv/bin/python -I eval/embedding/test_gm452_arms.py
"""

import contextlib
import hashlib
import io
import json
import shutil
import subprocess
import sys
import tempfile
import unittest
import warnings
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

import numpy as np

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
sys.path.insert(0, str(HERE))

import gm452_paired_bootstrap as PB  # noqa: E402
import gm461_learned_verdict as V  # noqa: E402
import gm697_floor_refit as G697  # noqa: E402
import gm701_floor_or_blend as G701  # noqa: E402
import q5_floor_sensitivity as q5  # noqa: E402
import shipped_floor_rates as SFR  # noqa: E402

LANGS = ("go", "python", "rust", "typescript")
BEFORE = "efbdf7ff~"  # the commit before GM-452/S2's code change


# ---- fixture: a run dir + eval dir in the harness formats --------------------------------------
def odd(qid):
    return hashlib.sha256(qid.encode()).digest()[0] % 2 == 1


def make_id(prefix, want_odd, used):
    """A fresh id whose sha256 parity is want_odd (held-out side for both split rules)."""
    i = 0
    while True:
        qid = f"{prefix}-{i}"
        if qid not in used and odd(qid) == want_odd:
            used.add(qid)
            return qid
        i += 1


def q(lang, mech, ho, shape, top, text=None):
    """shape: 'right' (expected row on top), 'wrong' (expected row 2nd), 'absent' (no expected row)."""
    return dict(lang=lang, mech=mech, ho=ho, shape=shape, top=top, text=text)


def write_fixture(root, specs, corpus="c", run_name="arm-x", ids=None, unranked=()):
    """Writes eval/ (queries) and runs/<run_name>/<corpus>/ (manifest, rankings).
    ids: optional list of ids (one per spec) to reuse between arms; unranked: spec indices with a
    query but no rankings row (an id missing from this arm). Returns (run, eval_dir, ids)."""
    root = Path(root)
    ev = root / "eval"
    run = root / "runs" / run_name
    (ev / "queries" / "mechanical").mkdir(parents=True, exist_ok=True)
    (run / corpus).mkdir(parents=True, exist_ok=True)
    used, out_ids = set(), []
    nl, mech, ranks = [], [], []
    for n, s in enumerate(specs):
        qid = ids[n] if ids else make_id(f"{s['lang'][:2]}{'m' if s['mech'] else 'n'}{n}", s["ho"], used)
        out_ids.append(qid)
        text = s["text"] or (f"parseThing{n}" if s["mech"] else f"find the thing {n}")
        positive = s["shape"] != "absent"
        qrow = dict(id=qid, corpus=corpus, language=s["lang"], kind="positive" if positive else "absent",
                    text=text, expected=[{"qualifiedName": f"E{n}"}] if positive else [])
        (mech if s["mech"] else nl).append(qrow)
        exp, other = f"e{n}", f"x{n}"
        if s["shape"] == "right":
            hits = [[exp, s["top"]], [other, round(s["top"] - 0.1, 4)]]
        elif s["shape"] == "wrong":
            hits = [[other, s["top"]], [exp, round(s["top"] - 0.05, 4)]]
        else:
            hits = [[other, s["top"]]]
        if n not in unranked:
            ranks.append(dict(id=qid, hits=hits, expected=[exp] if positive else [], topLanguage=s["lang"]))
    files = []
    for rel, rows in ((f"queries/{corpus}.jsonl", nl), (f"queries/mechanical/{corpus}.jsonl", mech)):
        p = ev / rel
        p.write_text("".join(json.dumps(r) + "\n" for r in rows))
        files.append([rel, hashlib.sha256(p.read_bytes()).hexdigest()])
    (run / corpus / "manifest.json").write_text(json.dumps({"corpus": corpus, "queryFiles": files}))
    (run / corpus / "rankings.jsonl").write_text("".join(json.dumps(r) + "\n" for r in ranks))
    return run, ev, out_ids


def full_arm(off=0.0):
    """Every language: name fit/held-out queries, NL fit and held-out positives (right and wrong top),
    NL held-out absent pages above and below the floor. D6 floor per language = 0.65 + lang offset + off
    (the lowest in-fit rank-1 positive, 0.655 + ...). Held-out NL CW pos 1/2 and CW absent 1/2 per language."""
    specs = []
    for k, l in enumerate(LANGS):
        o = k * 0.01 + off
        specs += [
            q(l, True, False, "right", 0.70 + o),   # name, FIT half
            q(l, True, True, "right", 0.72 + o),    # name, held-out
            q(l, True, True, "absent", 0.30),       # name absent, held-out
            q(l, False, False, "right", 0.655 + o),  # NL fit: sets the D6 floor
            q(l, False, True, "right", 0.80 + o),   # NL held-out right
            q(l, False, True, "wrong", 0.75 + o),   # NL held-out wrong top above floor: CW pos
            q(l, False, True, "absent", 0.76 + o),  # NL held-out absent above floor: CW absent
            q(l, False, True, "absent", 0.40),      # NL held-out absent below floor
        ]
    return specs


def expected_d6(off=0.0):
    return {l: q5.round_down_2(0.655 + k * 0.01 + off) for k, l in enumerate(LANGS)}


def run_script(name, *args, cwd=None):
    p = subprocess.run([sys.executable, "-I", str(name), *map(str, args)], capture_output=True, text=True, cwd=cwd)
    return p.returncode, p.stdout, p.stderr


class TmpCase(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="gm452-test-"))

    def tearDown(self):
        shutil.rmtree(self.tmp, ignore_errors=True)


# ---- 1. gm697 --report / --report-arm ----------------------------------------------------------
def report(arm, floors, cwp, cwa, other_arm="base-arm"):
    def blk(f, p, a):
        return {"floors": {"floors": f, "confidentWrongPositives": {"events": p[0], "total": p[1]},
                           "confidentWrongAbsent": {"events": a[0], "total": a[1]}}}
    return {"arms": {arm: blk(floors, cwp, cwa), other_arm: blk({l: 0.1 for l in LANGS}, (0, 1), (0, 1))}}


class Gm697Report(TmpCase):
    def setUp(self):
        super().setUp()
        self.run_dir, self.ev, _ = write_fixture(self.tmp, full_arm(), run_name="arm-j")
        self.d6 = expected_d6()

    def go(self, rep, *extra):
        p = self.tmp / "report.json"
        p.write_text(json.dumps(rep))
        js = self.tmp / "out.json"
        code, out, err = run_script(HERE / "gm697_floor_refit.py", "--run", self.run_dir, "--eval-dir", self.ev,
                                    "--report", p, "--json", js, *extra)
        return code, out, err, (json.loads(js.read_text()) if js.exists() else None)

    def test_report_matching_arm_passes_c1_c2(self):
        code, out, err, js = self.go(report("arm-j", self.d6, (4, 8), (4, 8)))
        self.assertEqual(code, 0, out + err)
        labels = {c[0]: c[1] for c in js["controls"]}
        self.assertTrue(labels["C1 pooled D6 fit == report floors"])
        self.assertTrue(labels["C2 (CW pos, CW abs) at D6 refit == report"])
        self.assertEqual(js["d6"], self.d6)

    def test_report_mode_skips_shipped_c2_and_c3(self):
        _, out, _, js = self.go(report("arm-j", self.d6, (4, 8), (4, 8)))
        labels = [c[0] for c in js["controls"]]
        self.assertFalse(any("shipped" in l or l.startswith("C3") or "S19" in l for l in labels), labels)
        self.assertEqual(len(labels), 3, labels)  # self-test, C1, C2 (refit)

    def test_report_floor_mismatch_fails_c1(self):
        bad = dict(self.d6, rust=self.d6["rust"] + 0.01)
        code, out, err, js = self.go(report("arm-j", bad, (4, 8), (4, 8)))
        self.assertEqual(code, 3)
        self.assertIn("CONTROL FAILED", err)
        self.assertIn("| C1 pooled D6 fit == report floors | FAIL |", out)

    def test_report_cw_mismatch_fails_c2(self):
        for cwp, cwa in (((5, 8), (4, 8)), ((4, 8), (4, 9))):
            code, out, err, _ = self.go(report("arm-j", self.d6, cwp, cwa))
            self.assertEqual(code, 3, (cwp, cwa))
            self.assertIn("| C2 (CW pos, CW abs) at D6 refit == report | FAIL |", out)
            self.assertIn("| C1 pooled D6 fit == report floors | PASS |", out)

    def test_report_arm_selects_other_key(self):
        rep = report("other-name", self.d6, (4, 8), (4, 8), other_arm="arm-j")
        code, out, err, _ = self.go(rep, "--report-arm", "other-name")
        self.assertEqual(code, 0, out + err)
        code, out, err, _ = self.go(rep)  # default key = run dir name -> the planted block
        self.assertEqual(code, 3)

    def test_unknown_report_arm_exits_with_message(self):
        code, out, err, _ = self.go(report("arm-j", self.d6, (4, 8), (4, 8)), "--report-arm", "nope")
        self.assertNotEqual(code, 0)
        self.assertIn("'nope' not in", err)
        self.assertNotIn("Traceback", err)

    def test_expected_from_report_reads_arm(self):
        p = self.tmp / "r.json"
        p.write_text(json.dumps(report("a", {"go": 0.5}, (1, 2), (3, 4))))
        self.assertEqual(G697.expected_from_report(p, "a"), ({"go": 0.5}, [(1, 2), (3, 4)]))


# ---- 2. shipped_floor_rates --after honours --floors / --custom -------------------------------
def after_rows(out, set_name, lang):
    """(misled before, misled after) cells of the --after table row."""
    for line in out.splitlines():
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        if len(cells) == 11 and cells[0] == set_name and cells[1] == lang:
            return cells[2], cells[3]
    raise AssertionError(f"row {set_name}/{lang} not in output:\n{out}")


class SfrAfter(TmpCase):
    def setUp(self):
        super().setUp()
        # name queries (no whitespace -> HARD below floor), rank-1 right: misled iff below the floor.
        specs = [q("go", True, False, "right", 0.55), q("python", True, False, "right", 0.575),
                 q("rust", True, False, "right", 0.70), q("typescript", True, False, "right", 0.70)]
        self.run_dir, self.ev, _ = write_fixture(self.tmp, specs)

    def after(self, *args):
        code, out, err = run_script(HERE / "shipped_floor_rates.py", "--run", self.run_dir, "--eval-dir", self.ev,
                                    "--after", *args)
        self.assertEqual(code, 0, err)
        return out

    def test_after_default_uses_shipped(self):
        out = self.after()
        self.assertEqual(after_rows(out, "name", "go"), ("100.0% (1/1)", "100.0% (1/1)"))  # 0.55 < 0.59
        self.assertEqual(after_rows(out, "name", "python"), ("0.0% (0/1)", "0.0% (0/1)"))  # 0.575 >= 0.57

    def test_after_custom_floors(self):
        out = self.after("--custom", "go=0.50,python=0.60,rust=0.50,typescript=0.50")
        self.assertEqual(after_rows(out, "name", "go"), ("0.0% (0/1)", "0.0% (0/1)"))
        self.assertEqual(after_rows(out, "name", "python"), ("100.0% (1/1)", "100.0% (1/1)"))
        self.assertIn("signal disagreements, rust vs simulated f, over pages with rows: 0 ", out)

    def test_after_floors_fitted(self):
        out = self.after("--floors", "fitted")  # FITTED: go 0.56, python 0.58
        self.assertEqual(after_rows(out, "name", "go"), ("100.0% (1/1)", "100.0% (1/1)"))
        self.assertEqual(after_rows(out, "name", "python"), ("100.0% (1/1)", "100.0% (1/1)"))

    def test_rust_signal_uses_passed_floors(self):
        r = dict(text="parseThing", top=0.55, tl="go")
        self.assertEqual(SFR.rust_signal(r), "HARD")  # shipped go 0.59
        self.assertEqual(SFR.rust_signal(r, {"go": 0.50}), "NONE")
        self.assertEqual(SFR.rust_signal(dict(r, tl="zig"), {"go": 0.50}, 0.60), "HARD")  # default floor
        self.assertEqual(SFR.rust_signal(dict(r, text="find the thing"), {"go": 0.60}), "SOFT")


# ---- 3. --st-runs selects the recall arm's runs dir --------------------------------------------
class Stop(Exception):
    pass


class StRuns(TmpCase):
    def setup_st_dir(self, st_runs):
        """gm701.setup up to load_corpora; returns the st_dir it resolved."""
        got = {}

        def stop(work, eval_dir, st_dir, corpora_dir, k):
            got["st_dir"] = st_dir
            raise Stop

        saved = dict(G701.R.SETTINGS)
        try:
            with mock.patch.object(G701.R, "sha256_file", return_value=G701.S.SHIPPED_SHA), \
                    mock.patch.object(G701.B, "load_corpora", side_effect=stop):
                with self.assertRaises(Stop):
                    if st_runs is None:
                        G701.setup(self.tmp, None)
                    else:
                        G701.setup(self.tmp, None, st_runs)
        finally:
            G701.R.SETTINGS.clear()
            G701.R.SETTINGS.update(saved)
        return got["st_dir"]

    def test_gm701_setup_st_runs(self):
        arm = self.tmp / "abs" / "runs-j"
        arm.mkdir(parents=True)
        self.assertEqual(self.setup_st_dir(str(arm)), arm)
        self.assertEqual(self.setup_st_dir("runs/jina-ft"), self.tmp / "runs/jina-ft")
        self.assertEqual(self.setup_st_dir(None), self.tmp / "runs" / G701.B.ST)

    def capture_setup(self, module, setup_owner, argv):
        got = {}

        def rec(work, cache_dir, st_runs=None, ce=None):
            got.update(work=work, st_runs=st_runs, ce=ce)
            raise Stop

        with mock.patch.object(setup_owner, "setup", side_effect=rec), \
                mock.patch.object(sys, "argv", [module.__file__, *argv]):
            with self.assertRaises(Stop):
                module.main()
        return got

    def test_gm701_main_forwards_st_runs(self):
        got = self.capture_setup(G701, G701, ["--work", str(self.tmp), "--st-runs", "runs/jina-ft"])
        self.assertEqual(got["st_runs"], "runs/jina-ft")
        self.assertIsNone(self.capture_setup(G701, G701, ["--work", str(self.tmp)])["st_runs"])

    def test_gm461_main_forwards_st_runs(self):
        got = self.capture_setup(V, V.G, ["--work", str(self.tmp), "--st-runs", "runs/jina-ft"])
        self.assertEqual(got["st_runs"], "runs/jina-ft")
        self.assertIsNone(self.capture_setup(V, V.G, ["--work", str(self.tmp)])["st_runs"])


# ---- 4. paired per-language bootstrap ----------------------------------------------------------
def mat(rows):
    """rows of (misled num, den, cw_pos num, den, cw_abs num, den)."""
    return np.array(rows, dtype=float)


ABS1, ABS0 = (0, 0, 0, 0, 1, 1), (0, 0, 0, 0, 0, 1)  # absent page above / below its floor


def boot(groups, resamples):
    """PB.run at seed 398; absent-only fixtures leave misled and cw_pos without denominators (NaN)."""
    with warnings.catch_warnings():
        warnings.simplefilter("ignore", RuntimeWarning)
        return PB.run(groups, np.random.default_rng(398), resamples)


class Bootstrap(TmpCase):
    def test_defaults_seed_398_and_10000(self):
        self.assertEqual((PB.SEED, PB.RESAMPLES), (398, 10000))
        run, ev, _ = write_fixture(self.tmp, full_arm())
        out = self.tmp / "o.json"
        code, _, err = run_script(HERE / "gm452_paired_bootstrap.py", "--base-run", run, "--run", run,
                                  "--eval-dir", ev, "--resamples", "20", "--out", out)
        self.assertEqual(code, 0, err)
        js = json.loads(out.read_text())
        self.assertEqual((js["seed"], js["floors_kind"]), (398, "d6"))

    def test_hand_computed_delta(self):
        # absent pages: base 1/4 above floor, candidate 3/4 -> cw_abs delta +0.5
        B = mat([ABS1, ABS0, ABS0, ABS0])
        C = mat([ABS1, ABS1, ABS1, ABS0])
        p, lo, hi = boot([(B, C)], 200)
        self.assertAlmostEqual(p[2], 0.5)
        self.assertTrue(np.isnan(p[0]) and np.isnan(p[1]))
        self.assertLessEqual(lo[2], 0.5)
        self.assertGreaterEqual(hi[2], 0.5)

    def test_identical_arms_ci_zero(self):
        B = mat([ABS1, ABS0, ABS0, ABS1, ABS0, ABS1, ABS0, ABS0])
        p, lo, hi = boot([(B, B.copy())], 500)
        self.assertEqual((p[2], lo[2], hi[2]), (0.0, 0.0, 0.0))

    def test_stratified_by_language(self):
        # go: base and cand all above; python: base all below, cand all above. Resampling within each
        # language keeps 2 go + 2 python pages per draw, so the pooled delta is exactly 0.5 every time.
        go_b, go_c = mat([ABS1, ABS1]), mat([ABS1, ABS1])
        py_b, py_c = mat([ABS0, ABS0]), mat([ABS1, ABS1])
        p, lo, hi = boot([(go_b, go_c), (py_b, py_c)], 500)
        self.assertEqual((p[2], lo[2], hi[2]), (0.5, 0.5, 0.5))

    def test_same_seed_same_draws(self):
        B = mat([ABS1, ABS0, ABS0, ABS1, ABS0])
        C = mat([ABS1, ABS1, ABS0, ABS0, ABS1])
        a = boot([(B, C)], 300)
        b = boot([(B, C)], 300)
        for x, y in zip(a, b):
            np.testing.assert_array_equal(x, y)

    def test_pairs_by_id_not_order_and_drops_unpaired(self):
        base = {"a": ("go", np.array(ABS1)), "b": ("go", np.array(ABS0)), "c": ("python", np.array(ABS0)),
                "only-base": ("go", np.array(ABS1))}
        cand = {"c": ("python", np.array(ABS1)), "only-cand": ("go", np.array(ABS0)),
                "b": ("go", np.array(ABS1)), "a": ("go", np.array(ABS1))}
        pr, nb, nc, npair = PB.paired(base, cand)
        self.assertEqual((nb, nc, npair), (4, 4, 3))
        np.testing.assert_array_equal(pr["go"][0], mat([ABS1, ABS0]))  # ids a, b in id order
        np.testing.assert_array_equal(pr["go"][1], mat([ABS1, ABS1]))
        np.testing.assert_array_equal(pr["python"][0], mat([ABS0]))
        np.testing.assert_array_equal(pr["python"][1], mat([ABS1]))

    def test_main_end_to_end_fixed_floors_per_arm(self):
        # candidate: same ids, every score +0.05 except the low absent pages -> its own D6 floors are +0.05;
        # each held-out page keeps its side of its own arm's floor, so every delta is 0 with CI [0, 0].
        b_run, ev, ids = write_fixture(self.tmp / "b", full_arm())
        shifted = [dict(s, top=s["top"] + 0.05) if s["top"] > 0.5 else s for s in full_arm()]
        c_run, ev_c, _ = write_fixture(self.tmp / "c", shifted, ids=ids)
        out1, out2 = self.tmp / "o1.json", self.tmp / "o2.json"
        for out in (out1, out2):
            code, stdout, err = run_script(HERE / "gm452_paired_bootstrap.py", "--base-run", b_run, "--run", c_run,
                                           "--eval-dir", ev, "--resamples", "200", "--out", out)
            self.assertEqual(code, 0, err)
        self.assertEqual(out1.read_text(), out2.read_text())  # deterministic
        js = json.loads(out1.read_text())
        self.assertEqual(js["base_floors"], expected_d6())
        self.assertEqual(js["cand_floors"], expected_d6(0.05))
        self.assertNotIn("unpaired", stdout)
        nl = js["rows"]["NL/pooled"]
        self.assertEqual(nl["n"], 16)
        self.assertEqual(js["rows"]["NAME/pooled"]["n"], 8)
        for m in ("cw_pos", "cw_abs", "misled"):
            self.assertEqual((nl[m]["point"], nl[m]["lo"], nl[m]["hi"]), (0.0, 0.0, 0.0), m)

    def test_main_reports_unpaired_ids(self):
        b_run, ev, ids = write_fixture(self.tmp / "b", full_arm())
        specs = full_arm()
        drop = {n for n, s in enumerate(specs) if s["lang"] == "go" and not s["mech"] and s["shape"] == "absent"}
        c_run, _, _ = write_fixture(self.tmp / "c", specs, ids=ids, unranked=drop)
        code, stdout, err = run_script(HERE / "gm452_paired_bootstrap.py", "--base-run", b_run, "--run", c_run,
                                       "--eval-dir", ev, "--resamples", "50")
        self.assertEqual(code, 0, err)
        self.assertIn("note: NL: ids differ between arms (base 16, candidate 14, paired 14); unpaired ids dropped",
                      stdout)
        self.assertIn("| NL | go | 2 |", stdout)


# ---- 6. gm461 --per-query-out (GM-452/S7) ------------------------------------------------------
PQ_FIELDS = {"arm", "id", "corpus", "language", "pop", "ho", "positive", "rank1", "below", "misled", "cw"}


def oc(qid, lang, mech, ho, positive, rank, top, top_language="same", corpus="c"):
    """A rerank_eval-style outcome dict as gm461's arms hold them."""
    return dict(id=qid, corpus=corpus, language=lang, top_language=lang if top_language == "same" else top_language,
                top=top, rank=rank, positive=positive, mechanical=mech, held_out=ho)


class PerQueryOut(TmpCase):
    FLOORS = {"go": 0.5, "python": 0.6}

    def outs(self):
        used = set()
        nm_odd, nm_even = make_id("nm", True, used), make_id("nm", False, used)
        return [
            oc("n-misled", "go", False, True, True, 1, 0.4),      # rank-1 positive refused
            oc("n-right", "go", False, True, True, 1, 0.9),       # rank-1 positive shown
            oc("n-wrong", "python", False, False, True, 2, 0.7),  # wrong top shown (fit half)
            oc("n-wrong-ref", "python", False, True, True, 3, 0.5),  # wrong top refused
            oc("n-abs-shown", "go", False, True, False, None, 0.8),
            oc("n-abs-ref", "go", False, True, False, None, 0.1),
            oc("n-notop", "go", False, True, True, 1, None),       # empty window: no verdict
            oc("n-nolang", "go", False, True, True, 1, 0.9, top_language=None),
            oc("n-nofloor", "rust", False, True, True, 1, 0.9),   # no floor for rust: no verdict
            # name rows: ho follows sha256(id) parity, not the outcome's held_out flag
            oc(nm_odd, "go", True, False, True, 1, 0.9),
            oc(nm_even, "go", True, True, True, 1, 0.9),
        ], nm_odd, nm_even

    def test_rows_fields_and_values(self):
        outs, nm_odd, nm_even = self.outs()
        rows = V.per_query_rows({"F4": (outs, self.FLOORS)})
        for r in rows:
            self.assertEqual(set(r), PQ_FIELDS)
        got = {r["id"]: r for r in rows}
        self.assertEqual(set(got), {"n-misled", "n-right", "n-wrong", "n-wrong-ref", "n-abs-shown", "n-abs-ref",
                                    nm_odd, nm_even})
        self.assertEqual(len(rows), 8)
        want = {  # pop, ho, positive, rank1, below, misled, cw
            "n-misled": ("NL", True, True, True, True, True, False),
            "n-right": ("NL", True, True, True, False, False, False),
            "n-wrong": ("NL", False, True, False, False, False, True),
            "n-wrong-ref": ("NL", True, True, False, True, False, False),
            "n-abs-shown": ("NL", True, False, False, False, False, True),
            "n-abs-ref": ("NL", True, False, False, True, False, False),
            nm_odd: ("NAME", True, True, True, False, False, False),
            nm_even: ("NAME", False, True, True, False, False, False),
        }
        for qid, w in want.items():
            r = got[qid]
            self.assertEqual((r["pop"], r["ho"], r["positive"], r["rank1"], r["below"], r["misled"], r["cw"]), w, qid)
            self.assertEqual((r["arm"], r["corpus"]), ("F4", "c"))
        self.assertEqual(got["n-wrong"]["language"], "python")
        json.dumps(rows)  # JSON-serialisable (plain bools, no numpy types)
        self.assertTrue(all(type(r[k]) is bool for r in rows for k in ("ho", "positive", "rank1", "below", "misled", "cw")))

    def test_one_row_per_arm_and_query(self):
        outs, _, _ = self.outs()
        clf = [dict(o, top_language="clf:NL", top=0.2 if o["id"] == "n-right" else 0.9) for o in outs]
        rows = V.per_query_rows({"F4": (outs, self.FLOORS), "L-m": (clf, {"clf:NL": 0.5})})
        self.assertEqual(sum(r["arm"] == "F4" for r in rows), 8)
        self.assertEqual(sum(r["arm"] == "L-m" for r in rows), len(outs))  # one classifier key covers every row
        self.assertEqual(len({(r["arm"], r["id"]) for r in rows}), len(rows))
        lm = {r["id"]: r for r in rows if r["arm"] == "L-m"}
        self.assertTrue(lm["n-right"]["below"] and lm["n-right"]["misled"])
        self.assertFalse(lm["n-misled"]["below"])

    # main(): everything before the per-query write is stubbed; control 2b (the next step) stops the run
    def run_main(self, *extra):
        outs, _, _ = self.outs()
        outs = [o for o in outs if o["top"] is not None and o["top_language"] and o["language"] != "rust"]
        nl = {(o["corpus"], o["id"]): 0.1 for o in outs if not o["mechanical"]}
        nm = {(o["corpus"], o["id"]): 0.9 for o in outs if o["mechanical"]}
        th = {"C0-m": 0.0, "L-m": 0.5, "L-d6": 0.5, "L-noCE": 0.5}

        def fit(rows, pop, *a):
            z = nl if pop == "NL" else nm
            return ({"ho": {"F4": {"kept_k": 0, "caught_k": 0}}, "thresholds": th, "stopped_arms": []},
                    {"full": z, "C0shift": z, "C0": z, "noCE": z}, None, {"C0": None})

        with mock.patch.object(V.G, "setup", return_value=(None, None, None, "sha")), \
                mock.patch.object(V.SFR, "structured_floors", return_value=dict(self.FLOORS)), \
                mock.patch.object(V, "features", return_value=([], [])), \
                mock.patch.object(V, "arm_outs", return_value=(outs, [dict(o) for o in outs])), \
                mock.patch.object(V, "fit_population", side_effect=fit), \
                mock.patch.object(V.R, "gm434", return_value=dict(V.GM697_INT8_NL_HO)), \
                mock.patch.object(V, "c0_d6_check", side_effect=Stop), \
                mock.patch.object(sys, "argv", [V.__file__, "--work", str(self.tmp), *map(str, extra)]):
            with self.assertRaises(Stop):
                V.main()
        return outs

    def test_main_flag_writes_jsonl(self):
        pq = self.tmp / "pq" / "rows.jsonl"
        pq.parent.mkdir()
        outs = self.run_main("--per-query-out", pq)
        rows = [json.loads(l) for l in pq.read_text().splitlines()]
        arms = ("int8", "F4", "C0", "C0-m", "L-m", "L-d6", "L-noCE")
        self.assertEqual(sorted({r["arm"] for r in rows}), sorted(arms))
        self.assertEqual(len(rows), len(arms) * len(outs))
        self.assertEqual(len({(r["arm"], r["id"]) for r in rows}), len(rows))
        for r in rows:
            self.assertEqual(set(r), PQ_FIELDS)
        got = {(r["arm"], r["id"]): r for r in rows}
        # L-m: NL z 0.1 < 0.5 -> refused; NAME z 0.9 -> shown. F4: cosine floors.
        self.assertTrue(got[("L-m", "n-right")]["misled"])
        self.assertFalse(got[("F4", "n-right")]["below"])
        self.assertTrue(got[("F4", "n-misled")]["misled"])

    def test_main_without_flag_writes_nothing(self):
        self.run_main()
        self.assertEqual(list(self.tmp.rglob("*")), [])


# ---- 7. bootstrap stack mode (GM-452/S7) --------------------------------------------------------
def srow(arm, qid, lang, positive, rank1, below, pop="NL", ho=True):
    return dict(arm=arm, id=qid, corpus="c", language=lang, pop=pop, ho=ho, positive=positive, rank1=rank1,
                below=below, misled=bool(positive and rank1 and below),
                cw=bool((not below) and not (positive and rank1)))


def write_jsonl(path, rows):
    Path(path).write_text("".join(json.dumps(r) + "\n" for r in rows))
    return path


def boot_script(*args):
    return run_script(HERE / "gm452_paired_bootstrap.py", *args)


class StackMode(TmpCase):
    """Hand-computable fixture. NL held-out, F4 arm:
    go: 2 wrong-top positives, shown in base and candidate (cw_pos 1 -> 1, delta 0, every draw);
    python: 2 wrong-top positives, refused in base, shown in candidate (cw_pos 0 -> 1, delta +1, every draw).
    Pooled with per-language strata: 2 go + 2 python per draw -> delta +0.5 exactly, CI [0.5, 0.5].
    Decoys that must be ignored: arm X rows (all refused), NL fit-half rows, a NAME row, an unpaired id."""

    def files(self, base_arm="F4", shuffle=True):
        def f4(cand):
            return [srow(base_arm if not cand else "F4", "g1", "go", True, False, False),
                    srow(base_arm if not cand else "F4", "g2", "go", True, False, False),
                    srow(base_arm if not cand else "F4", "p1", "python", True, False, not cand),
                    srow(base_arm if not cand else "F4", "p2", "python", True, False, not cand)]
        decoys = lambda arm: [srow("X", i, l, True, False, True) for i, l in
                              (("g1", "go"), ("g2", "go"), ("p1", "python"), ("p2", "python"))] + \
            [srow(arm, "fit1", "python", True, False, True, ho=False),
             srow(arm, "nm1", "go", True, True, True, pop="NAME")]
        base = f4(False) + decoys(base_arm) + ([srow("F4", "g1", "go", True, False, True)] if base_arm != "F4" else [])
        cand = list(reversed(f4(True))) + decoys("F4") + [srow("F4", "only-cand", "go", True, False, False)]
        if base_arm != "F4":  # base file's F4 rows contradict the selected arm
            base = [r for r in base if not (r["arm"] == "F4" and r["id"] == "g1")] + \
                [srow("F4", i, l, True, False, False) for i, l in (("p1", "python"), ("p2", "python"))]
        return write_jsonl(self.tmp / "base.jsonl", base), write_jsonl(self.tmp / "cand.jsonl", cand)

    def check(self, js, stdout):
        nl = js["rows"]
        self.assertEqual(nl["NL/pooled"]["n"], 4)
        self.assertEqual(nl["NL/go"]["n"], 2)
        self.assertEqual(nl["NL/python"]["n"], 2)
        c = nl["NL/pooled"]["cw_pos"]
        self.assertEqual((c["point"], c["lo"], c["hi"]), (0.5, 0.5, 0.5))
        c = nl["NL/python"]["cw_pos"]
        self.assertEqual((c["point"], c["lo"], c["hi"]), (1.0, 1.0, 1.0))
        c = nl["NL/go"]["cw_pos"]
        self.assertEqual((c["point"], c["lo"], c["hi"]), (0.0, 0.0, 0.0))
        self.assertEqual(nl["NAME/pooled"]["n"], 1)
        self.assertIn("note: NL: ids differ between arms (base 4, candidate 5, paired 4); unpaired ids dropped", stdout)
        self.assertIn("| NL | pooled | 4 | ", stdout)
        self.assertIn("+50.0 [+50.0, +50.0]", stdout)

    def test_stack_hand_computed_and_deterministic(self):
        b, c = self.files()
        outs = []
        for k in (1, 2):
            out = self.tmp / f"o{k}.json"
            code, stdout, err = boot_script("--base-stack", b, "--stack", c, "--resamples", "300", "--out", out)
            self.assertEqual(code, 0, err)
            outs.append(out.read_text())
        self.assertEqual(outs[0], outs[1])
        js = json.loads(outs[0])
        self.assertEqual((js["seed"], js["mode"], js["base_arm"], js["stack_arm"]), (398, "stack", "F4", "F4"))
        self.check(js, stdout)

    def test_arm_selection(self):
        b, c = self.files(base_arm="B")
        out = self.tmp / "o.json"
        code, stdout, err = boot_script("--base-stack", b, "--stack", c, "--base-arm", "B", "--stack-arm", "F4",
                                        "--resamples", "100", "--out", out)
        self.assertEqual(code, 0, err)
        js = json.loads(out.read_text())
        self.assertEqual((js["base_arm"], js["stack_arm"]), ("B", "F4"))
        self.check(js, stdout)
        # --stack-arm X: candidate X refuses everything -> python 0 -> 0, go 1 -> 0
        code, stdout, err = boot_script("--base-stack", b, "--stack", c, "--base-arm", "B", "--stack-arm", "X",
                                        "--resamples", "100", "--out", out)
        self.assertEqual(code, 0, err)
        c = json.loads(out.read_text())["rows"]["NL/pooled"]["cw_pos"]
        self.assertEqual((c["point"], c["lo"], c["hi"]), (-0.5, -0.5, -0.5))

    def test_stack_ci_matches_run_at_seed_398(self):
        # non-degenerate data: the stack-mode interval is PB.run on the per-language strata at seed 398
        rows_b, rows_c = [], []
        pat_b = [(True, True, False), (True, False, False), (False, False, False), (True, True, True), (False, False, True)]
        pat_c = [(True, True, True), (True, False, True), (False, False, False), (True, True, False), (False, False, False)]
        for lang in ("go", "rust"):
            for i in range(10):
                pb, pc = pat_b[i % 5], pat_c[(i * 3) % 5]
                rows_b.append(srow("F4", f"{lang}{i}", lang, *pb))
                rows_c.append(srow("F4", f"{lang}{i}", lang, pb[0], pb[1], pc[2]))
        b, c = write_jsonl(self.tmp / "b.jsonl", rows_b), write_jsonl(self.tmp / "c.jsonl", rows_c)
        out = self.tmp / "o.json"
        code, _, err = boot_script("--base-stack", b, "--stack", c, "--resamples", "400", "--out", out)
        self.assertEqual(code, 0, err)
        js = json.loads(out.read_text())
        groups = []
        for lang in ("go", "rust"):
            sel = sorted(r["id"] for r in rows_b if r["language"] == lang)
            vb = {r["id"]: PB.vector(r["positive"], r["rank1"], r["below"]) for r in rows_b}
            vc = {r["id"]: PB.vector(r["positive"], r["rank1"], r["below"]) for r in rows_c}
            groups.append((np.array([vb[i] for i in sel]), np.array([vc[i] for i in sel])))
        p, lo, hi = boot(groups, 400)
        for i, m in enumerate(PB.METRICS):
            got = js["rows"]["NL/pooled"][m]
            self.assertAlmostEqual(got["point"], float(p[i]), msg=m)
            self.assertAlmostEqual(got["lo"], float(lo[i]), msg=m)
            self.assertAlmostEqual(got["hi"], float(hi[i]), msg=m)
        self.assertNotEqual(js["rows"]["NL/pooled"]["misled"]["lo"], js["rows"]["NL/pooled"]["misled"]["hi"])

    def test_vector_and_stack_indicators(self):
        np.testing.assert_array_equal(PB.vector(True, True, True), [1, 1, 0, 1, 0, 0])
        np.testing.assert_array_equal(PB.vector(True, False, False), [0, 0, 1, 1, 0, 0])
        np.testing.assert_array_equal(PB.vector(True, False, True), [0, 0, 0, 1, 0, 0])
        np.testing.assert_array_equal(PB.vector(False, False, False), [0, 0, 0, 0, 1, 1])
        np.testing.assert_array_equal(PB.vector(False, False, True), [0, 0, 0, 0, 0, 1])
        b, _ = self.files()
        got = PB.stack_indicators(b, "F4", "NL")
        self.assertEqual(sorted(got), ["g1", "g2", "p1", "p2"])
        self.assertEqual(got["p1"][0], "python")
        np.testing.assert_array_equal(got["p1"][1], [0, 0, 0, 1, 0, 0])
        self.assertEqual(sorted(PB.stack_indicators(b, "F4", "NAME")), ["nm1"])
        self.assertEqual(sorted(PB.stack_indicators(b, "X", "NL")), ["g1", "g2", "p1", "p2"])
        np.testing.assert_array_equal(PB.stack_indicators(b, "X", "NL")["g1"][1], [0, 0, 0, 1, 0, 0])

    def test_mode_exclusivity(self):
        b, c = self.files()
        run, ev, _ = write_fixture(self.tmp / "fx", full_arm())
        need_both = "error: stack mode needs both --base-stack and --stack"
        exclude = "error: --base-stack/--stack exclude --base-run/--run"
        embedder = "error: --base-run, --run and --eval-dir are required (or --base-stack and --stack)"
        for args, msg in ((["--base-stack", b], need_both), (["--stack", c], need_both),
                          (["--base-stack", b, "--stack", c, "--base-run", run], exclude),
                          (["--base-stack", b, "--stack", c, "--run", run], exclude),
                          ([], embedder), (["--base-run", run, "--run", run], embedder)):
            with self.subTest(args=args):
                code, _, err = boot_script(*args, "--resamples", "10")
                self.assertEqual(code, 2, err)
                self.assertIn(msg, err)


# ---- 8. resamples with an empty denominator are counted (GM-452/S7) -----------------------------
class NanResamples(TmpCase):
    def test_run_counts_all_nan_metrics(self):
        st = {}
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", RuntimeWarning)
            PB.run([(mat([ABS1, ABS0, ABS0]), mat([ABS1, ABS1, ABS0]))], np.random.default_rng(398), 250, st)
        self.assertEqual(st["nan_resamples"], [250, 250, 0])

    def test_run_counts_partial_nan_draws(self):
        # 1 positive among 6 rows: a draw without it has no misled/cw_pos denominator.
        POS = (1, 1, 0, 1, 0, 0)
        B = mat([POS, ABS1, ABS0, ABS0, ABS1, ABS0])
        R = 400
        st = {}
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", RuntimeWarning)
            p, lo, hi = PB.run([(B, B.copy())], np.random.default_rng(398), R, st)
        rng = np.random.default_rng(398)
        want = sum(0 not in rng.integers(0, 6, 6) for _ in range(R))  # replay the draws
        self.assertTrue(0 < want < R)
        self.assertEqual(st["nan_resamples"], [want, want, 0])
        self.assertEqual((lo[0], hi[0]), (0.0, 0.0))  # percentiles over the non-NaN draws

    def test_run_without_stats_unchanged(self):
        B = mat([ABS1, ABS0, ABS0, ABS1])
        a = boot([(B, B.copy())], 100)
        st = {}
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", RuntimeWarning)
            b = PB.run([(B, B.copy())], np.random.default_rng(398), 100, st)
        for x, y in zip(a, b):
            np.testing.assert_array_equal(x, y)

    def assert_counts(self, js, err, R, lang_row):
        self.assertEqual(js["rows"][lang_row]["nan_resamples"], [R, R, 0])
        rows = js["rows"].values()
        total = [sum(r["nan_resamples"][m] for r in rows) for m in range(3)]
        self.assertEqual(js["nan_resamples_total"], total)
        self.assertIn(f"nan resamples dropped by nanpercentile (misled, cw_pos, cw_abs; summed over rows): {total}", err)

    def test_stack_mode_counts(self):
        R = 60
        rows = [srow("F4", "g1", "go", False, False, False), srow("F4", "g2", "go", False, False, True),
                srow("F4", "p1", "python", True, True, False), srow("F4", "p2", "python", True, True, True)]
        b = write_jsonl(self.tmp / "b.jsonl", rows)
        out = self.tmp / "o.json"
        code, stdout, err = boot_script("--base-stack", b, "--stack", b, "--resamples", R, "--out", out)
        self.assertEqual(code, 0, err)
        js = json.loads(out.read_text())
        self.assertEqual(js["rows"]["NL/python"]["nan_resamples"], [0, 0, R])
        self.assertEqual(js["rows"]["NL/pooled"]["nan_resamples"], [0, 0, 0])
        self.assert_counts(js, err, R, "NL/go")
        self.assertEqual(js["nan_resamples_total"], [R, R, R])
        self.assertNotIn("nan resamples", stdout)

    def test_embedder_mode_counts(self):
        # go has no held-out NL positive: its NL row has no misled/cw_pos denominator in any draw
        specs = [s for s in full_arm() if not (s["lang"] == "go" and not s["mech"] and s["ho"] and s["shape"] != "absent")]
        run, ev, _ = write_fixture(self.tmp, specs)
        R = 40
        out = self.tmp / "o.json"
        code, stdout, err = boot_script("--base-run", run, "--run", run, "--eval-dir", ev, "--resamples", R,
                                        "--out", out)
        self.assertEqual(code, 0, err)
        js = json.loads(out.read_text())
        self.assert_counts(js, err, R, "NL/go")
        self.assertNotIn("nan resamples", stdout)


# ---- 9. gm461 arm floors, beta, FIT-only beta (GM-452/S12) -------------------------------------
def beta_corpus(specs, name="c"):
    """One corpus; specs: (qid, held_out, positive, mechanical, [(nid, cos)], {nid: logit}, expected).
    Returns (corpus, score_of) with score_of recording every (qid, nid) it scores."""
    queries, lists, expected, logits, node_lang = {}, {}, {}, {}, {}
    for qid, ho, pos, mech, lst, lg, exp in specs:
        queries[qid] = dict(text="find the thing", mechanical=mech, language="go", positive=pos, held_out=ho)
        lists[qid], expected[qid] = lst, set(exp)
        for nid, _ in lst:
            logits[(qid, nid)] = lg[nid]
            node_lang[nid] = "go"
    c = SimpleNamespace(name=name, qids=list(queries), queries=queries, lists={V.B.ST: lists},
                        expected=expected, node_lang=node_lang, trimmed_doc={})
    seen = []

    def sc(c_, q_, nid):
        seen.append(q_)
        return logits[(q_, nid)]
    return c, sc, seen


# a (cos 0.9) is the cosine top, b (0.8) the CE's: blend picks a iff beta * 0.1 > 10, i.e. beta > 100
AB = [("a", 0.9), ("b", 0.8)]
AB_LOGITS = {"a": 0.0, "b": 10.0}


class ArmFloors(TmpCase):
    FLAT = {"go": 0.5, "python": 0.45, "rust": 0.55, "typescript": 0.4}

    def write(self, obj):
        p = self.tmp / f"f{len(list(self.tmp.iterdir()))}.json"
        p.write_text(json.dumps(obj))
        return p

    def test_flat_json(self):
        self.assertEqual(V.load_floors(self.write(self.FLAT)), self.FLAT)
        got = V.load_floors(self.write({k: str(v) for k, v in self.FLAT.items()}))
        self.assertEqual(got, self.FLAT)  # values coerced to float
        self.assertTrue(all(type(v) is float for v in got.values()))

    def test_report_with_arm_reads_that_arms_d6_floors(self):
        rep = self.write(report("arm-j", self.FLAT, (1, 2), (3, 4), other_arm="base-arm"))
        self.assertEqual(V.load_floors(rep, "arm-j"), self.FLAT)
        self.assertEqual(V.load_floors(rep, "base-arm"), {l: 0.1 for l in LANGS})

    def test_report_without_arm_exits(self):
        rep = self.write(report("arm-j", self.FLAT, (1, 2), (3, 4)))
        with self.assertRaises(SystemExit) as cm:
            V.load_floors(rep)
        self.assertIn("--floors-arm", str(cm.exception.code))

    def test_missing_language_exits(self):
        short = {k: v for k, v in self.FLAT.items() if k != "rust"}
        for obj, arm in ((short, None), (report("arm-j", short, (1, 2), (3, 4)), "arm-j")):
            with self.subTest(report=arm is not None):
                with self.assertRaises(SystemExit) as cm:
                    V.load_floors(self.write(obj), arm)
                self.assertIn("!= languages", str(cm.exception.code))


class BetaFeatures(unittest.TestCase):
    def rows(self, **kw):
        qid = make_id("q", False, set())  # FIT side for the sha256 rule features() checks
        c, sc, _ = beta_corpus([(qid, False, True, False, AB, AB_LOGITS, ["b"])])
        rows, _ = V.features([c], sc, {("c", qid): False}, {"go": 0.5}, **kw)
        return rows[0]

    def test_blend_uses_given_beta(self):
        r = self.rows(beta=1.0)  # 10 + 0.8 > 0 + 0.9: b shown
        self.assertTrue(r["right"])
        self.assertEqual((r["cos_t"], r["logit_t"]), (0.8, 10.0))
        r = self.rows(beta=200.0)  # 0 + 180 > 10 + 160: a shown
        self.assertFalse(r["right"])
        self.assertEqual((r["cos_t"], r["logit_t"]), (0.9, 0.0))

    def test_default_beta_is_shipped_40(self):
        self.assertEqual(V.S.BETA, 40.0)
        self.assertEqual(self.rows(), self.rows(beta=40.0))


class TuneBetaOnFit(unittest.TestCase):
    def corpus(self):
        # FIT wants a (beta > 100); two held-out queries want b (beta < 100) and would outvote it on MRR.
        return beta_corpus([
            ("fit-1", False, True, False, AB, AB_LOGITS, ["a"]),
            ("ho-1", True, True, False, AB, AB_LOGITS, ["b"]),
            ("ho-2", True, True, False, AB, AB_LOGITS, ["b"]),
            ("name-1", False, True, True, AB, AB_LOGITS, ["b"]),   # mechanical: not FIT NL
            ("abs-1", False, False, False, AB, AB_LOGITS, []),     # absent: not scored
        ])

    def test_held_out_never_scored_and_would_change_beta(self):
        c, sc, seen = self.corpus()
        (beta, fit_key, added), n = V.tune_beta_on_fit([c], sc)
        self.assertEqual(n, 1)
        self.assertEqual(set(seen), {"fit-1"})
        self.assertEqual(beta, 160.0)  # smallest grid beta > 100
        self.assertEqual(fit_key, (1.0, 1.0))
        self.assertEqual(added, [])
        # control on the fixture: with the held-out queries in, the tuned beta differs
        all_nl = [(c, q) for q in c.qids if V.R.scored(c.queries[q])]
        self.assertNotEqual(V.B.tune_beta(all_nl, V.S.K, sc)[0], beta)


class Gm461ArmMain(TmpCase):
    """main() with the data and fit stubbed; control 2b is forced to fail so the run stops after the
    controls are assembled, and redact_heldout is the identity so control 1 is visible."""

    def run_main(self, *extra, tuned=((160.0, (0.5, 0.25), []), 7)):
        got = {}

        def feats(cs, sc, flags, floors, beta=None):
            got["features"] = (floors, beta)
            return [], []

        def arms(cs, sc, flags, pop_of, beta=None):
            got["arm_outs"] = beta
            return [], []

        def fit(rows, pop, *a):
            r = {"ho": {"F4": {"kept_k": [0, 0], "caught_k": [0, 0]}}, "thresholds": {}, "stopped_arms": [],
                 "control2a_rows_checked": 0, "control2a_mismatch_n": 0, "control_shuffle": {},
                 "differ_vs_F4": {"L-m": 0}}
            r.update({f"model_{k}": {"grad_norm": 0.0} for k in ("full", "C0", "noCE")})
            return r, {"full": {}, "C0shift": {}, "C0": {}, "noCE": {}}, {}, {"C0": None}

        def c2b(rows, int8, model, pinned=True):
            got["pinned"] = pinned
            return {"ok": False}

        def tune(cs, sc):
            got["tuned"] = True
            return tuned

        out = self.tmp / "o.json"
        err = io.StringIO()
        with mock.patch.object(V.G, "setup", return_value=(None, None, None, "sha")), \
                mock.patch.object(V.SFR, "structured_floors", return_value={"go": 0.11}), \
                mock.patch.object(V, "features", side_effect=feats), \
                mock.patch.object(V, "arm_outs", side_effect=arms), \
                mock.patch.object(V, "fit_population", side_effect=fit), \
                mock.patch.object(V, "tune_beta_on_fit", side_effect=tune), \
                mock.patch.object(V.R, "gm434", return_value={"misled": (9, 9)}), \
                mock.patch.object(V.Q5, "fixed_floors_gates", return_value={}), \
                mock.patch.object(V, "c0_d6_check", side_effect=c2b), \
                mock.patch.object(V, "redact_heldout", side_effect=lambda r: r), \
                mock.patch.object(sys, "argv", [V.__file__, "--work", str(self.tmp), "--out", str(out),
                                                *map(str, extra)]), \
                contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(err):
            with self.assertRaises(SystemExit) as cm:
                V.main()
        got["code"], got["err"] = cm.exception.code, err.getvalue()
        got["res"] = json.loads(out.read_text()) if out.exists() else None
        return got

    def floors_file(self):
        p = self.tmp / "floors.json"
        p.write_text(json.dumps(ArmFloors.FLAT))
        return p

    def test_default_is_base_arm(self):
        g = self.run_main()
        res = g["res"]
        self.assertEqual(g["code"], 2)
        self.assertNotIn("arm", res)
        self.assertEqual(g["features"], ({"go": 0.11}, V.S.BETA))
        self.assertEqual(g["arm_outs"], V.S.BETA)
        self.assertTrue(g["pinned"])
        self.assertNotIn("skipped", res["controls"]["1_reproduce"])
        self.assertIs(res["controls"]["1_reproduce"]["ok"], False)
        self.assertTrue(any(s.startswith("control 1") for s in res["stops"]))
        self.assertNotIn("tuned", g)

    def check_non_base(self, g):
        res = g["res"]
        self.assertEqual(g["code"], 2)
        self.assertIn("arm", res)
        self.assertFalse(g["pinned"])
        c1 = res["controls"]["1_reproduce"]
        self.assertIn("skipped", c1)
        self.assertIsNone(c1["ok"])
        self.assertFalse(any(s.startswith("control 1") for s in res["stops"]))
        self.assertIn("control 1 skipped", g["err"])
        return res["arm"]

    def test_floors_json_is_non_base(self):
        g = self.run_main("--floors-json", self.floors_file())
        arm = self.check_non_base(g)
        self.assertEqual(g["features"], (ArmFloors.FLAT, V.S.BETA))
        self.assertEqual(g["res"]["floors"], ArmFloors.FLAT)
        self.assertEqual(arm["floors_json"], str(self.floors_file()))
        self.assertEqual((arm["beta"], arm["beta_tuned_on_fit"], arm["st_runs"]), (V.S.BETA, None, None))

    def test_report_floors_arm_is_forwarded(self):
        p = self.tmp / "rep.json"
        p.write_text(json.dumps(report("arm-j", ArmFloors.FLAT, (1, 2), (3, 4))))
        g = self.run_main("--floors-json", p, "--floors-arm", "arm-j")
        self.assertEqual(self.check_non_base(g)["floors_arm"], "arm-j")
        self.assertEqual(g["features"][0], ArmFloors.FLAT)

    def test_beta_is_non_base(self):
        g = self.run_main("--beta", "5")
        arm = self.check_non_base(g)
        self.assertEqual((g["features"][1], g["arm_outs"], arm["beta"]), (5.0, 5.0, 5.0))
        self.assertNotIn("tuned", g)

    def test_beta_40_is_base(self):
        self.assertNotIn("arm", self.run_main("--beta", "40")["res"])

    def test_st_runs_is_non_base(self):
        self.assertEqual(self.check_non_base(self.run_main("--st-runs", "runs/j"))["st_runs"], "runs/j")

    def test_tune_beta_on_fit_uses_and_records_tuned_beta(self):
        g = self.run_main("--tune-beta-on-fit")
        arm = self.check_non_base(g)
        self.assertEqual((g["features"][1], g["arm_outs"], arm["beta"]), (160.0, 160.0, 160.0))
        self.assertEqual(arm["beta_tuned_on_fit"], {"beta": 160.0, "fit_r10": 0.5, "fit_mrr": 0.25,
                                                    "grid_added": [], "fit_queries": 7})
        self.assertIn("FIT-tuned beta 160", g["err"])

    def test_beta_and_tune_are_exclusive(self):
        g = self.run_main("--beta", "5", "--tune-beta-on-fit")
        self.assertIn("exclusive", str(g["code"]))
        self.assertNotIn("tuned", g)
        self.assertNotIn("features", g)
        self.assertIsNone(g["res"])


class C0D6Pinned(unittest.TestCase):
    """Control 2b: the base arm checks the v2 int8 constants; another arm only its own R.fit_floors."""

    def setUp(self):
        tops = [0.30, 0.35, 0.40, 0.45]  # < 34 rows: D6 index 0 -> floor 0.30 (!= v2 go 0.51)
        self.int8 = [dict(id=f"q{i}", corpus="c", language="go", positive=True, mechanical=False,
                          held_out=False, rank=1, top=t) for i, t in enumerate(tops)]
        self.rows = [{"key": ("c", f"q{i}"), "cos1": t} for i, t in enumerate(tops)]
        self.model = SimpleNamespace(cols=["cos1"], z=lambda X: X[:, 0])  # z monotone in cos1

    def test_pinned_requires_v2_constants(self):
        r = V.c0_d6_check(self.rows, self.int8, self.model)
        self.assertEqual(r["implied"], {"go": 0.30})
        self.assertEqual(r["implied"], r["fit_floors"])
        self.assertEqual(r["v2_refit"], V.V2_D6_INT8)
        self.assertFalse(r["ok"])

    def test_unpinned_checks_own_fit_floors_only(self):
        r = V.c0_d6_check(self.rows, self.int8, self.model, pinned=False)
        self.assertTrue(r["ok"])
        self.assertIn("skipped", r["v2_refit"])
        # still a real check: a z that disagrees with cosine order fails it
        bad = SimpleNamespace(cols=["cos1"], z=lambda X: -X[:, 0])
        self.assertFalse(V.c0_d6_check(self.rows, self.int8, bad, pinned=False)["ok"])


# ---- 5. default output unchanged vs the pre-change scripts --------------------------------------
CHANGED = ("gm697_floor_refit.py", "shipped_floor_rates.py", "gm701_floor_or_blend.py", "gm461_learned_verdict.py")


class DefaultUnchanged(TmpCase):
    def setUp(self):
        super().setUp()
        old = self.tmp / "old"
        oe = old / "eval" / "embedding"
        oe.mkdir(parents=True)
        for d in ("docs", "core"):
            (old / d).symlink_to(REPO / d)
        for f in HERE.glob("*.py"):
            if f.name not in CHANGED:
                (oe / f.name).symlink_to(f)
        for name in CHANGED:
            p = subprocess.run(["git", "-C", str(REPO), "show", f"{BEFORE}:eval/embedding/{name}"],
                               capture_output=True, text=True)
            if p.returncode != 0:
                self.skipTest(f"git show {BEFORE} failed: {p.stderr}")
            (oe / name).write_text(p.stdout)
        self.old = oe
        self.run_dir, self.ev, _ = write_fixture(self.tmp / "fx", full_arm())

    def both(self, script, *args):
        """Runs the old and new script; output files named OUT are compared too."""
        res = []
        for side, d in (("old", self.old), ("new", HERE)):
            outd = self.tmp / f"out-{side}"
            outd.mkdir(exist_ok=True)
            a = [str(x).replace("OUT", str(outd)) for x in args]
            code, out, err = run_script(d / script, *a)
            files = {p.name: p.read_text() for p in sorted(outd.iterdir())}
            shutil.rmtree(outd)
            res.append((code, out, err.replace(str(d), "<HERE>"), files))
        self.assertEqual(res[0], res[1])
        return res[1]

    def test_gm697_default_unchanged(self):
        code, out, _, files = self.both("gm697_floor_refit.py", "--run", self.run_dir, "--eval-dir", self.ev,
                                        "--json", "OUT/o.json", "--table", "OUT/o.md")
        self.assertIn("C1 pooled D6 fit == S19", out)  # controls compare against the S19 constants
        self.assertEqual(set(files), {"o.json", "o.md"})

    def test_sfr_default_unchanged(self):
        for extra in ([], ["--json", "OUT/o.json"], ["--options"], ["--after"], ["--floors", "fitted"],
                      ["--floors", "shipped-int8"], ["--custom", "go=0.5,python=0.6,rust=0.5,typescript=0.5"]):
            with self.subTest(extra=extra):
                self.both("shipped_floor_rates.py", "--run", self.run_dir, "--eval-dir", self.ev, *extra)


if __name__ == "__main__":
    unittest.main()
