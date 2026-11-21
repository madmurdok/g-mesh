"""Unit tests for the GM-452/S2 arm parameterisation of the eval scripts.

Covers gm697_floor_refit.py --report/--report-arm, shipped_floor_rates.py
--after with --floors/--custom, --st-runs in gm701_floor_or_blend.py and
gm461_learned_verdict.py, the paired per-language bootstrap
gm452_paired_bootstrap.py, and that the default (no new flag) output equals
the pre-change scripts (efbdf7ff~).

Synthetic fixtures only: tiny run/eval dirs in a temp dir; g-mesh-data and the
held-out eval data are never read. numpy must import; the regression group
needs git and the efbdf7ff~ commit.

Run: ~/Projects/ClaudeProjects/g-mesh-data/gm436/venv/bin/python -I eval/embedding/test_gm452_arms.py
"""

import hashlib
import json
import shutil
import subprocess
import sys
import tempfile
import unittest
import warnings
from pathlib import Path
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

        def rec(work, cache_dir, st_runs=None):
            got.update(work=work, st_runs=st_runs)
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
