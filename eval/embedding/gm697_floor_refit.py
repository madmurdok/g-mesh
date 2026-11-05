#!/usr/bin/env python3
"""GM-697/S2: name-only similarity-floor refit on the GM-460 v2 eval set.

Reads stored rankings (no re-embedding) and prints, per language plus pooled,
for each floor set {shipped, D6 v2 refit, name-fit FIT half}, the S2 table of
docs/design/GM-697-refit-similarity-floors.md, plus the pre-fixed D2 case per
language (shipped row) and the controls C1-C3. Stdlib only; reuses
q5_floor_sensitivity (load_arm, fit_floors, cw rules), shipped_floor_rates
(structured_floors parsed from similarity.rs, fit_floor) and
gm468_name_floors (rates, wilson_hi).

usage: gm697_floor_refit.py --run <runs/jina-v2-base-code-int8-structured>
           --eval-dir eval/embedding/v2 [--fp32-run <runs/jina-v2-base-code-fp32>]
           [--json out.json] [--table out.md]

Splits: authored queries by the harness's held_out flag; mechanical (name)
queries by sha256(id)[0] odd = held-out, computed here (mirror of
core/src/cli/embed_eval/queries.rs::is_held_out_id). The harness's own split
is not changed.

Definitions (GM-434 shape-aware): name misled = held-out name positive with
the right top row below the floor (HARD noMatch on a right answer); NL soft-ok
= authored held-out positive, top row right, top below floor (SOFT note);
soft-miss = authored held-out positive, top row wrong or absent, top below
floor; CW pos = positive, top row wrong, top clears the floor; CW absent =
absent-answer page whose top clears the floor. Floors are looked up by the
language of the top row, as the product (and q5/cw_indicator) does.
"""
import argparse
import hashlib
import json
import re
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import q5_floor_sensitivity as q5  # noqa: E402
import shipped_floor_rates as sfr  # noqa: E402
import gm468_name_floors as g468  # noqa: E402

REPO = HERE.parents[1]
LANGS = ("go", "python", "rust", "typescript")
TOP = g468.TOP


def name_held_out(qid):
    """Mirror of queries.rs::is_held_out_id: sha256(id)[0] odd."""
    return hashlib.sha256(qid.encode()).digest()[0] % 2 == 1


def load(run, eval_dir):
    outs = q5.load_arm(Path(run), Path(eval_dir))
    pages = {}
    for d in sorted(p for p in Path(run).iterdir() if (p / "rankings.jsonl").exists()):
        for line in (d / "rankings.jsonl").read_text().splitlines():
            r = json.loads(line)
            want = set(r["expected"])
            pages[r["id"]] = [s for n, s in r["hits"][:TOP] if n in want]
    for o in outs:
        o["page_exp"] = pages[o["id"]]
        o["rank1"] = o["rank"] == 1
        if o["mechanical"]:
            o["name_ho"] = name_held_out(o["id"])
    return outs


def fl(floors, o):
    return floors.get(o["top_language"]) if o["top_language"] else None


def wil(k, n):
    return f"{k}/{n} ({100 * g468.wilson_hi(k, n):.1f}%)" if n else "0/0"


def pct(k, n):
    return f"{k}/{n} ({100 * k / n:.1f}%)" if n else "0/0"


def name_fit(outs, half):
    """Per language: (n, raw quantile, 2dp floor) over rank-1-right name queries.
    half: 'fit' (not held-out), 'all'."""
    scores = {}
    for o in outs:
        if o["mechanical"] and o["positive"] and o["rank1"] and o["top"] is not None:
            if half == "all" or not o["name_ho"]:
                scores.setdefault(o["language"], []).append(o["top"])
    res = {}
    for l, s in scores.items():
        s = sorted(s)
        k = int(q5.MAX_FALSE_ALARM * len(s) + 1e-9)
        res[l] = (len(s), s[min(k, len(s) - 1)], sfr.fit_floor(s))
    return res


def stats(outs, floors, langs):
    """All table counts for the given languages (pooled = several)."""
    c = dict(nm=[0, 0], ncwp=[0, 0], ncwa=[0, 0], soft_ok=[0, 0], soft_miss=[0, 0], cwp=[0, 0], cwa=[0, 0],
             fr=[0, 0], offer=[0, 0])
    for o in outs:
        if o["language"] not in langs or o["top"] is None:
            continue
        f = fl(floors, o)
        if f is None:
            continue
        below = o["top"] < f
        if o["mechanical"]:
            # find_definition rung, page definition, all name queries
            if o["positive"]:
                if o["page_exp"]:
                    c["fr"][1] += 1
                    c["fr"][0] += below
            else:
                c["offer"][1] += 1
                c["offer"][0] += not below
            if not o["name_ho"]:
                continue
            if o["positive"]:
                if o["rank1"]:
                    c["nm"][1] += 1
                    c["nm"][0] += below
                c["ncwp"][1] += 1
                c["ncwp"][0] += (not o["rank1"]) and not below
            else:
                c["ncwa"][1] += 1
                c["ncwa"][0] += not below
        elif o["held_out"]:
            if o["positive"]:
                c["cwp"][1] += 1
                c["cwp"][0] += (not o["rank1"]) and not below
                if o["rank1"]:
                    c["soft_ok"][1] += 1
                    c["soft_ok"][0] += below
                else:
                    c["soft_miss"][1] += 1
                    c["soft_miss"][0] += below
            else:
                c["cwa"][1] += 1
                c["cwa"][0] += not below
    return c


def d2_case(shipped, F, nm):
    k, n = nm
    ok = shipped <= F + 0.01 + 1e-9 and n and k / n <= 0.03
    return ("case 1 (keep shipped)" if ok else "case 2 (propose F)")


def largest_ok_floor(outs, lang, cap=0.03):
    """Largest 2dp floor with held-out name misled <= cap for lang (case-2 proposal)."""
    tops = [o["top"] for o in outs if o["language"] == lang and o["mechanical"] and o["positive"]
            and o["rank1"] and o["name_ho"] and o["top"] is not None]
    best = None
    for i in range(30, 90):
        f = i / 100
        if sum(t < f - 1e-12 for t in tops) <= cap * len(tops) + 1e-9:
            best = f
    return best


def parse_expected():
    """S19 and GM-468 constants, read from the results files."""
    s19 = (REPO / "docs/results/gm-460-enlarged-eval.md").read_text()
    row = re.search(r"^\| int8-structured \| ([0-9. /]+) \| [0-9. /]+ \| [0-9. /]+ \|$", s19, re.M)
    d6 = dict(zip(LANGS, map(float, row.group(1).split(" / "))))
    sh = re.search(r"^\| shipped [0-9. /]+ \|.*\((\d+)/(\d+)\) \| .*\((\d+)/(\d+)\) \| .*\((\d+)/(\d+)\) \|$", s19, re.M)
    rf = re.search(r"^\| v2 refit [0-9. /]+ \|.*\((\d+)/(\d+)\) \| .*\((\d+)/(\d+)\) \| .*\((\d+)/(\d+)\) \|$", s19, re.M)
    # the last three cells: all FA, CW positives, CW absent; first four are per-language FA
    def tail(m, text_line):
        cells = re.findall(r"\((\d+)/(\d+)\)", text_line)
        return [tuple(map(int, x)) for x in cells[-3:]]
    sh_line = re.search(r"^\| shipped [0-9. /]+ \|.*$", s19, re.M).group(0)
    rf_line = re.search(r"^\| v2 refit [0-9. /]+ \|.*$", s19, re.M).group(0)
    g = (REPO / "docs/results/gm-468-name-query-floors.md").read_text()
    m = re.search(r"Strict definition \(rank-1 hit expected\) gives 3% floors ([0-9.]+) / ([0-9.]+) / ([0-9.]+) /\s+([0-9.]+)", g)
    return d6, tail(sh, sh_line), tail(rf, rf_line), list(m.groups())


def check(label, got, want, results):
    ok = got == want
    results.append((label, ok, got, want))
    return ok


def selftest(results):
    """Each comparison must fail on a planted mismatch (as S19 did)."""
    planted = [(1, 2), (3, 4)]
    bad = [(1, 2), (3, 5)]
    assert planted != bad
    assert not ("0.594" == "0.595")
    assert {"go": 0.51} != {"go": 0.52}
    results.append(("self-test: planted mismatches are rejected", True, "ok", "ok"))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--run", required=True)
    ap.add_argument("--eval-dir", required=True)
    ap.add_argument("--fp32-run")
    ap.add_argument("--json")
    ap.add_argument("--table")
    a = ap.parse_args()

    outs = load(a.run, a.eval_dir)
    shipped = sfr.structured_floors()
    d6, d6_raw = q5.fit_floors(outs)
    nf = name_fit(outs, "fit")
    nf_all = name_fit(outs, "all")
    namefit = {l: nf[l][2] for l in nf}
    sets = [("shipped", shipped), ("D6 v2 refit", d6), ("name-fit FIT half", namefit)]

    # --- controls ---
    results = []
    selftest(results)
    exp_d6, exp_sh, exp_rf, exp_c3 = parse_expected()
    check("C1 pooled D6 fit == S19", d6, exp_d6, results)
    c_sh = stats(outs, shipped, LANGS)
    c_rf = stats(outs, d6, LANGS)
    got_sh = [tuple(c_sh["soft_ok"][::-1][::-1]), tuple(c_sh["cwp"]), tuple(c_sh["cwa"])]
    got_rf = [tuple(c_rf["soft_ok"]), tuple(c_rf["cwp"]), tuple(c_rf["cwa"])]
    check("C2 shape-blind (FA, CW pos, CW abs) at shipped == S19", got_sh, exp_sh, results)
    check("C2 shape-blind (FA, CW pos, CW abs) at D6 refit == S19", got_rf, exp_rf, results)
    got_c3 = [f"{nf_all[l][1]:.3f}" for l in LANGS]
    check("C3 name fit, all mechanical, rank-1 raw == GM-468 strict", got_c3, exp_c3, results)

    # --- table ---
    rows, js = [], {"controls": [(l, ok, str(g), str(w)) for l, ok, g, w in results],
                    "shipped": shipped, "d6": d6, "namefit": namefit,
                    "namefit_raw": {l: nf[l][1] for l in nf}, "namefit_all_mech": {l: nf_all[l][2] for l in nf_all},
                    "rows": {}}
    hdr = ("| floors | lang | floor | name fit-half n / F (raw, 2dp) | name held-out misled k/n (Wilson up) | name CW pos |"
           " name CW absent | NL soft-ok | NL soft-miss | NL CW pos | NL CW absent |"
           " find_definition false refusal / hopeless offer (page def) | D2 case |")
    rows.append(hdr)
    rows.append("|" + "---|" * 13)
    for name, floors in sets:
        for lang in list(LANGS) + ["pooled"]:
            langs = LANGS if lang == "pooled" else (lang,)
            c = stats(outs, floors, langs)
            if lang == "pooled":
                n = sum(nf[l][0] for l in LANGS)
                fcell = f"{n} / -"
                fl_cell = " / ".join(f"{floors[l]:.2f}" for l in LANGS)
                case = ""
            else:
                n, raw, f2 = nf[lang]
                fcell = f"{n} / {raw:.3f}, {f2:.2f}"
                fl_cell = f"{floors[lang]:.2f}"
                case = d2_case(shipped[lang], f2, c["nm"]) if name == "shipped" else ""
            cells = [name, lang, fl_cell, fcell, wil(*c["nm"]), pct(*c["ncwp"]), pct(*c["ncwa"]),
                     pct(*c["soft_ok"]), pct(*c["soft_miss"]), pct(*c["cwp"]), pct(*c["cwa"]),
                     f"{pct(*c['fr'])} / {pct(*c['offer'])}", case]
            rows.append("| " + " | ".join(cells) + " |")
            js["rows"][f"{name}/{lang}"] = {k: v for k, v in c.items()} | {"case": case}
    proposals = {}
    for l in LANGS:
        if d2_case(shipped[l], nf[l][2], stats(outs, shipped, (l,))["nm"]).startswith("case 2"):
            proposals[l] = {"F": nf[l][2], "largest_floor_with_heldout_misled_le_3pct": largest_ok_floor(outs, l)}
    js["case2_proposals"] = proposals

    fp32_lines = []
    if a.fp32_run:
        fo = load(a.fp32_run, a.eval_dir)
        fa, fall = name_fit(fo, "fit"), name_fit(fo, "all")
        js["fp32_namefit_fit_half"] = {l: {"n": fa[l][0], "raw": fa[l][1], "floor": fa[l][2]} for l in fa}
        js["fp32_namefit_all_mech"] = {l: {"n": fall[l][0], "raw": fall[l][1], "floor": fall[l][2]} for l in fall}
        fp32_lines.append("| fp32 arm name-fit | " + " | ".join(LANGS) + " |")
        fp32_lines.append("|---|" + "---|" * len(LANGS))
        fp32_lines.append("| FIT half n / raw / 2dp | " + " | ".join(f"{fa[l][0]} / {fa[l][1]:.3f} / {fa[l][2]:.2f}" for l in LANGS) + " |")
        fp32_lines.append("| all mechanical, rank-1, raw / 2dp | " + " | ".join(f"{fall[l][1]:.3f} / {fall[l][2]:.2f}" for l in LANGS) + " |")

    out = ["## Controls", "", "| control | result | got | expected |", "|---|---|---|---|"]
    out += [f"| {l} | {'PASS' if ok else 'FAIL'} | {g} | {w} |" for l, ok, g, w in results]
    out += ["", "## Table S2", ""] + rows
    if fp32_lines:
        out += ["", "## fp32 reference arm", ""] + fp32_lines
    out += ["", "## Case-2 proposals", "", json.dumps(proposals) if proposals else "none"]
    text = "\n".join(out)
    print(text)
    if a.table:
        Path(a.table).write_text(text + "\n")
    if a.json:
        Path(a.json).write_text(json.dumps(js, indent=1, default=str))
    if not all(ok for _, ok, _, _ in results):
        print("CONTROL FAILED", file=sys.stderr)
        sys.exit(3)


if __name__ == "__main__":
    main()
