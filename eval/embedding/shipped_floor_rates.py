#!/usr/bin/env python3
"""GM-434: false-alarm and confident-wrong rates of the SHIPPED similarity floors.

Reads the stored rankings of a GM-398 run (no re-embedding) and judges each
query exactly as `core/src/cli/embed_eval/metrics.rs` does, but at fixed
floors instead of re-fitted ones:

- false alarm: positive query, top hit is an expected symbol, top score below
  the floor of the top hit's language (search_code would say "no match").
- confident wrong: top score at or above that floor, and the top hit is not
  expected (positive) or the query has no answer (absent).

Control: `--floors fitted` must reproduce the report's `falseAlarmHeldOut` and
confident-wrong counts for the reference arm (report-phaseB.json).

GM-434/S2: `--options` evaluates the candidate below-floor behaviours (keep,
NL re-fit, soft wording, marked rows, and combinations) as page signals an
agent acts on; see `evaluate_options` and docs/results/gm-434-floor-nl-queries.md.

Usage:
  python3 eval/embedding/shipped_floor_rates.py --run <runs/jina-v2-base-code-fp32> [--floors shipped|fitted]
  python3 eval/embedding/shipped_floor_rates.py --run <runs/jina-v2-base-code-fp32> --options
"""
import argparse
import hashlib
import json
from collections import defaultdict
from pathlib import Path

EVAL = Path(__file__).resolve().parent
# core/src/mcp/similarity.rs::floor, release-3.16.0 (read, not remembered).
SHIPPED = {"go": 0.59, "python": 0.57, "rust": 0.55, "typescript": 0.50}
# report-phaseB.json, arms["jina-v2-base-code-fp32"].floors.floors (GM-398 D6).
FITTED = {"go": 0.56, "python": 0.58, "rust": 0.56, "typescript": 0.55}
DEFAULT_FLOOR = 0.50


def held_out(qid: str, mechanical: bool) -> bool:
    return not mechanical and hashlib.sha256(qid.encode()).digest()[0] % 2 == 1


def load_queries(corpus: str):
    out = {}
    for rel, mech in ((f"queries/{corpus}.jsonl", False), (f"queries/mechanical/{corpus}.jsonl", True)):
        p = EVAL / rel
        if not p.exists():
            continue
        for line in p.read_text().splitlines():
            if line.strip():
                q = json.loads(line)
                q["_mech"] = mech
                out[q["id"]] = q
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--run", required=True)
    ap.add_argument("--floors", choices=["shipped", "fitted"], default="shipped")
    ap.add_argument("--json")
    ap.add_argument("--options", action="store_true", help="GM-434/S2: evaluate the below-floor options")
    ap.add_argument("--after", action="store_true", help="GM-434/S7: score similarity.rs's shipped verdict rule")
    a = ap.parse_args()
    floors = SHIPPED if a.floors == "shipped" else FITTED

    rows = []
    for d in sorted(Path(a.run).iterdir()):
        if not (d / "rankings.jsonl").exists():
            continue
        man = json.loads((d / "manifest.json").read_text())
        for rel, sha in man["queryFiles"]:
            got = hashlib.sha256((EVAL / rel).read_bytes()).hexdigest()
            if got != sha:
                raise SystemExit(f"{rel} changed since the run ({got} != {sha})")
        qs = load_queries(man["corpus"])
        for line in (d / "rankings.jsonl").read_text().splitlines():
            r = json.loads(line)
            q = qs[r["id"]]
            hits = r["hits"]
            rank = next((i + 1 for i, (nid, _) in enumerate(hits) if nid in r["expected"]), None)
            top = hits[0][1] if hits else None
            tl = r.get("topLanguage")
            rows.append(dict(
                id=r["id"], corpus=man["corpus"], lang=q["language"], mech=q["_mech"],
                pos=q["kind"] == "positive", held=held_out(r["id"], q["_mech"]),
                rank1=rank == 1, clears=None if top is None else top >= floors.get(tl, DEFAULT_FLOOR),
                rank=rank, top=top, tl=tl, prose=any(c.isspace() for c in q["text"].strip()),
                text=q["text"],
            ))

    if a.after:
        evaluate_after(rows)
        return

    if a.options:
        evaluate_options(rows)
        return

    def rates(sel):
        fa = [r for r in sel if r["pos"] and r["rank1"] and r["clears"] is not None]
        cwp = [r for r in sel if r["pos"] and r["clears"] is not None]
        cwa = [r for r in sel if not r["pos"] and r["clears"] is not None]
        return dict(
            fa=(sum(not r["clears"] for r in fa), len(fa)),
            cw_pos=(sum(r["clears"] and not r["rank1"] for r in cwp), len(cwp)),
            cw_abs=(sum(r["clears"] for r in cwa), len(cwa)),
            n_pos=sum(r["pos"] for r in sel), n_abs=sum(not r["pos"] for r in sel),
        )

    sets = {
        "NL held-out": lambda r: not r["mech"] and r["held"],
        "NL all": lambda r: not r["mech"],
        "name (mechanical)": lambda r: r["mech"],
    }
    langs = sorted({r["lang"] for r in rows})
    result = {}
    def pct(t):
        return f"{100*t[0]/t[1]:.1f}% ({t[0]}/{t[1]})" if t[1] else "-"
    print(f"floors={a.floors} {floors}")
    print("| set | language | floor | false alarm | confident wrong (positives) | confident wrong (absent) | n pos / n absent |")
    print("|---|---|---|---|---|---|---|")
    for name, f in sets.items():
        for lang in langs + ["all"]:
            sel = [r for r in rows if f(r) and (lang == "all" or r["lang"] == lang)]
            x = rates(sel)
            result[f"{name}/{lang}"] = x
            fl = floors.get(lang, "-") if lang != "all" else "-"
            print(f"| {name} | {lang} | {fl} | {pct(x['fa'])} | {pct(x['cw_pos'])} | {pct(x['cw_abs'])} | {x['n_pos']} / {x['n_abs']} |")
    if a.json:
        Path(a.json).write_text(json.dumps(result, indent=1))


# ---------------------------------------------------------------- GM-434/S2

MAX_FALSE_ALARM = 0.03  # core/src/cli/embed_eval/metrics.rs::MAX_FALSE_ALARM


def fit_floor(scores, max_rate=MAX_FALSE_ALARM):
    """core/src/cli/embed_eval/metrics.rs::fit_floor, line for line."""
    if not scores:
        return None
    s = sorted(scores)
    k = int(max_rate * len(s) + 1e-9)
    return int(s[min(k, len(s) - 1)] * 100 + 1e-9) / 100


def evaluate_options(rows):
    """Each option maps a page (its top score, top language, query shape) to
    the signal the agent receives: HARD (noMatch: "ignore the rows, fall
    back"), SOFT ("low similarity; the top row may still be right - one
    confirming read") or NONE (an ordinary page, with the "one confirming
    read and stop" hint). Outcome classes, per query:

      misled     HARD on a page whose top row is right: the agent is told to
                 discard the answer it was given (the false alarm).
      cw         NONE on a page whose top row is wrong (positive) or that has
                 no answer (absent): the agent is invited to trust it.
      soft-ok    SOFT and the top row is right: one read, answer found.
      soft-miss  SOFT and the top row is wrong or absent: one wasted read
                 before falling back - protection that depends on the agent
                 actually doing the read sceptically.
      hard-ok    HARD and the top row is wrong or absent: the verdict working.

    NL floors are fitted on the NL fit half only (queries with no held-out
    bit), per top-hit language and pooled, with metrics.rs's rule, so NL
    numbers are reported on the held-out half; name numbers on all name
    queries.
    """
    rows = [r for r in rows if r["top"] is not None]
    langs = sorted({r["tl"] for r in rows})
    fit = [r for r in rows if not r["mech"] and not r["held"] and r["pos"] and r["rank1"]]
    nl_fit = {l: fit_floor([r["top"] for r in fit if r["tl"] == l]) for l in langs}
    nl_pooled = fit_floor([r["top"] for r in fit])
    print("NL fit-half rank-1 n per language:", {l: sum(r["tl"] == l for r in fit) for l in langs})
    print("NL re-fit floors (per language):", nl_fit, " pooled:", nl_pooled)
    print("shipped floors:", SHIPPED)

    F = lambda r: SHIPPED.get(r["tl"], DEFAULT_FLOOR)
    NLF = lambda r: nl_fit.get(r["tl"]) or DEFAULT_FLOOR
    LOW = lambda r: min(NLF(r), F(r))

    def tiers(hard_below, soft_below):
        def sig(r):
            if r["top"] < hard_below(r):
                return "HARD"
            if r["top"] < soft_below(r):
                return "SOFT"
            return "NONE"
        return sig

    none = lambda r: -1.0
    options = {
        "a keep": tiers(F, F),
        "b NL re-fit, per language": tiers(NLF, NLF),
        "b' NL re-fit, pooled": tiers(lambda r: nl_pooled, lambda r: nl_pooled),
        "c soft wording": tiers(none, F),
        "d marked rows, no verdict (mark ignored)": tiers(none, none),
        "e shape-split floors (prose: NL re-fit)": lambda r: tiers(NLF, NLF)(r) if r["prose"] else tiers(F, F)(r),
        "f shape-split wording (prose: soft)": lambda r: tiers(none, F)(r) if r["prose"] else tiers(F, F)(r),
        "g two-tier (hard < NL re-fit, soft < shipped)": tiers(LOW, F),
        "h = f+g (prose: two-tier; name: keep)": lambda r: tiers(LOW, F)(r) if r["prose"] else tiers(F, F)(r),
    }

    def tally(sel, sig):
        c = defaultdict(int)
        for r in sel:
            s = sig(r)
            right = r["pos"] and r["rank1"]
            c["R"] += right
            c["pos"] += r["pos"]
            c["abs"] += not r["pos"]
            if s == "HARD":
                c["misled" if right else "hard_ok"] += 1
                c["hard_on_page5"] += r["pos"] and r["rank"] is not None and 2 <= r["rank"] <= 5
            elif s == "SOFT":
                c["soft_ok" if right else "soft_miss"] += 1
            else:
                if not right:
                    c["cw_pos" if r["pos"] else "cw_abs"] += 1
        return c

    def f(n, d):
        return f"{100*n/d:.1f}% ({n}/{d})" if d else "-"

    sets = {
        "NL held-out": lambda r: not r["mech"] and r["held"],
        # Out of sample only for the options with no NL-fitted constant (a, c, d, f).
        "NL all": lambda r: not r["mech"],
        "name": lambda r: r["mech"],
    }
    print()
    print("| option | set | misled (HARD, top right) | confident wrong, positives | confident wrong, absent | soft-ok | soft-miss (extra read) | hard-ok | HARD with answer at rank 2-5 |")
    print("|---|---|---|---|---|---|---|---|---|")
    for name, sig in options.items():
        for sname, sf in sets.items():
            c = tally([r for r in rows if sf(r)], sig)
            print(f"| {name} | {sname} | {f(c['misled'], c['R'])} | {f(c['cw_pos'], c['pos'])} | {f(c['cw_abs'], c['abs'])} "
                  f"| {c['soft_ok']} | {c['soft_miss']} | {c['hard_ok']} | {c['hard_on_page5']} |")
    print()
    print("Per language, NL held-out:")
    print("| option | language | misled | confident wrong, positives | confident wrong, absent | soft-ok | soft-miss | hard-ok |")
    print("|---|---|---|---|---|---|---|---|")
    for name in ("a keep", "b NL re-fit, per language", "g two-tier (hard < NL re-fit, soft < shipped)"):
        for l in langs:
            c = tally([r for r in rows if not r["mech"] and r["held"] and r["tl"] == l], options[name])
            print(f"| {name} | {l} | {f(c['misled'], c['R'])} | {f(c['cw_pos'], c['pos'])} | {f(c['cw_abs'], c['abs'])} | {c['soft_ok']} | {c['soft_miss']} | {c['hard_ok']} |")
    # Prose/name predicate check: every authored query must be prose, every
    # mechanical one a name, or the shape-split options measure the wrong split.
    mis = sum(r["prose"] == r["mech"] for r in rows)
    print(f"\nshape predicate disagreements with the eval's own split: {mis} of {len(rows)}")



# ---------------------------------------------------------------- GM-434/S7

# Rust's char::is_whitespace / str::trim: the Unicode White_Space property.
# Python's str.isspace / str.strip add U+001C-U+001F, which White_Space lacks.
RUST_WS = set("\t\n\x0b\x0c\r \x85\xa0\u1680\u2028\u2029\u202f\u205f\u3000") | {chr(c) for c in range(0x2000, 0x200B)}


def rust_trim(t):
    i, j = 0, len(t)
    while i < j and t[i] in RUST_WS:
        i += 1
    while j > i and t[j - 1] in RUST_WS:
        j -= 1
    return t[i:j]


def rust_is_prose(t):
    """similarity.rs::is_prose_query: query.trim().chars().any(char::is_whitespace)."""
    return any(c in RUST_WS for c in rust_trim(t))


def rust_is_specifier(t):
    """similarity.rs::is_specifier_query."""
    t = rust_trim(t)
    return bool(t) and not rust_is_prose(t) and (t.startswith("@") or "/" in t)


def rust_signal(r):
    """similarity.rs::verdict + low_similarity on a first page (cursor None).

    HARD = noMatch (specifier, or a name query below its floor), SOFT =
    lowSimilarity (a prose query below its floor), NONE = nothing. The page
    is judged by its top row against the top row's language: the stored
    rankings carry no per-row language, and on a score-sorted
    single-language page that equals below_floor's every-row rule.
    """
    if rust_is_specifier(r["text"]):
        return "HARD"
    if r["top"] is None:
        return "NONE"
    if r["top"] >= SHIPPED.get(r["tl"], DEFAULT_FLOOR):
        return "NONE"
    return "SOFT" if rust_is_prose(r["text"]) else "HARD"


def evaluate_after(rows):
    """Before (S1, option a: every below-floor page is noMatch) against after
    (the shipped Rust rule), per query language, plus the rule check against
    S2's simulated option f."""
    F = lambda r: SHIPPED.get(r["tl"], DEFAULT_FLOOR)

    def sim_f(r):  # S2 option f, exactly as evaluate_options scores it
        if r["top"] is None:
            return None
        if r["prose"]:
            return "SOFT" if r["top"] < F(r) else "NONE"
        return "HARD" if r["top"] < F(r) else "NONE"

    def before(r):
        return "HARD" if r["top"] < F(r) else "NONE"

    empty = [r for r in rows if r["top"] is None]
    ws_diff = [r for r in rows if r["prose"] != rust_is_prose(r["text"])]
    spec = [r for r in rows if rust_is_specifier(r["text"])]
    sig_diff = [r for r in rows if r["top"] is not None and sim_f(r) != rust_signal(r)]
    exotic = [r for r in rows if any(c.isspace() and c not in " " for c in r["text"])]
    print(f"queries: {len(rows)}")
    print(f"prose predicate (python strip/isspace vs rust trim/is_whitespace) disagreements: {len(ws_diff)}")
    print(f"queries with whitespace other than U+0020: {len(exotic)} {sorted({repr(c) for r in exotic for c in r['text'] if c.isspace() and c != ' '})}")
    print(f"specifier queries (rust verdict regardless of score): {len(spec)} {[r['id'] for r in spec][:20]}")
    print(f"empty pages (rust: no verdict; S2 excluded them): {len(empty)}")
    print(f"signal disagreements, rust vs simulated f, over pages with rows: {len(sig_diff)} {[r['id'] for r in sig_diff][:20]}")

    rows = [r for r in rows if r["top"] is not None]

    def tally(sel, sig):
        c = defaultdict(int)
        for r in sel:
            s = sig(r)
            right = r["pos"] and r["rank1"]
            c["R"] += right
            c["pos"] += r["pos"]
            c["abs"] += not r["pos"]
            c[s] += 1
            if s == "HARD" and right:
                c["misled"] += 1
            if s == "NONE" and not right:
                c["cw_pos" if r["pos"] else "cw_abs"] += 1
        return c

    def f(n, d):
        return f"{100*n/d:.1f}% ({n}/{d})" if d else "-"

    sets = {
        "NL held-out": lambda r: not r["mech"] and r["held"],
        "NL all": lambda r: not r["mech"],
        "name": lambda r: r["mech"],
    }
    langs = sorted({r["lang"] for r in rows})
    print()
    print("| set | language | misled before | misled after | confident wrong, absent before | after | confident wrong, pos before | after | noMatch pages before | after | lowSimilarity pages after |")
    print("|---|---|---|---|---|---|---|---|---|---|---|")
    for sname, sf in sets.items():
        for lang in langs + ["all"]:
            sel = [r for r in rows if sf(r) and (lang == "all" or r["lang"] == lang)]
            b, x = tally(sel, before), tally(sel, rust_signal)
            print(f"| {sname} | {lang} | {f(b['misled'], b['R'])} | {f(x['misled'], x['R'])} "
                  f"| {f(b['cw_abs'], b['abs'])} | {f(x['cw_abs'], x['abs'])} "
                  f"| {f(b['cw_pos'], b['pos'])} | {f(x['cw_pos'], x['pos'])} "
                  f"| {b['HARD']} | {x['HARD']} | {x['SOFT']} |")
    print()
    print("Option f reproduced with the Rust rule (compare evaluate_options' f rows):")
    for sname, sf in sets.items():
        sel = [r for r in rows if sf(r)]
        c = tally(sel, rust_signal)
        soft_ok = sum(rust_signal(r) == "SOFT" and r["pos"] and r["rank1"] for r in sel)
        hard_ok = sum(rust_signal(r) == "HARD" and not (r["pos"] and r["rank1"]) for r in sel)
        print(f"| f (rust) | {sname} | {f(c['misled'], c['R'])} | {f(c['cw_pos'], c['pos'])} | {f(c['cw_abs'], c['abs'])} "
              f"| {soft_ok} | {c['SOFT'] - soft_ok} | {hard_ok} |")


if __name__ == "__main__":
    main()
