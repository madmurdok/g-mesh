"""GM-456 S52: the automatic re-send in `judge.py --run` of a pack whose parse left pairs unjudged.

One pack (2 queries x 2 candidates) driven through the fake `claude` of test_gm456_claude_cli.
The fake serves a rule's steps in call order; a pack call is one `claude` process of kind "pack".
Each test asserts counts and file contents, never an order between processes.
"""
import json
import sys
import unittest
from unittest import mock

import test_gm456_claude_cli as T
import test_gm456_render_export as RE

C, CC, J, X = T.C, T.CC, T.J, T.X
NA = "non-answer"


class ResendBase(T.E2EBase):
    def setUp(self):
        super().setUp()
        self.write_nodes(["r:t1", "r:c1", "r:c2"])
        negs = [{"key": "r:c1", "sources": ["embed"], "sameFile": False},
                {"key": "r:c2", "sources": ["embed"], "sameFile": False}]
        rows = [{"id": f"r-0001-{s}", "repo": "r", "target": "r:t1", "text": f"q {s}", "positives": ["r:t1"],
                 "negatives": negs} for s in ("p1", "p2")]
        C.write_jsonl(self.vdir / "queries.mined.jsonl", rows)

    # --- responses -------------------------------------------------------
    @staticmethod
    def verdicts(*entries):
        return {"verdicts": [{"t": 1, "c": c, "verdicts": v} for c, v in entries]}

    def good(self):
        return T.cli_ok(self.verdicts((1, [NA, NA]), (2, [NA, NA])))

    def short(self):  # c2 has one verdict for two queries: entry dropped, 2 pairs unjudged
        return T.cli_ok(self.verdicts((1, [NA, NA]), (2, [NA])))

    def no_c2(self):  # no entry for candidate 2
        return T.cli_ok(self.verdicts((1, [NA, NA])))

    def empty(self):  # parses, judges nothing: 4 pairs unjudged
        return T.cli_ok(self.verdicts())

    def steps(self, *outs):
        self.set_fake([{"match": "", "steps": [o if "rc" in o else {"stdout": o} for o in outs]}])

    # --- state -----------------------------------------------------------
    @property
    def jdir(self):
        return self.vdir / "judge"

    @property
    def resp(self):
        return self.vdir / "responses" / "judge"

    @property
    def cid(self):
        return C.read_jsonl(self.jdir / "judge-requests.jsonl")[0]["custom_id"]

    def packs(self):
        return len(self.calls("pack"))

    def judged(self):
        return C.read_jsonl(self.vdir / "queries.judged.jsonl")

    def unjudged(self):  # pairs left unjudged: in queries.judged.jsonl plus in the queries the judge dropped
        dropped = C.read_jsonl(self.jdir / "judge-dropped.jsonl") if (self.jdir / "judge-dropped.jsonl").exists() else []
        return len(X.unjudged(self.judged())) + sum(d["unjudgedPairs"] for d in dropped)

    def resent(self):
        return C.read_jsonl(self.jdir / "judge-resent.jsonl")

    def skipped(self):
        return C.read_jsonl(self.jdir / "judge-skipped.jsonl")

    def snapshot(self):
        out, sha, errf = CC.pack_files(self.resp, self.cid)
        return out.read_bytes(), sha.read_bytes(), errf.exists()

    def exit3(self, *extra):
        with self.assertRaises(SystemExit) as cm:
            self.main(J, *extra)
        self.assertEqual(cm.exception.code, 3)


class Resend(ResendBase):
    def test_short_then_good_sends_twice_and_judges_every_pair(self):  # behaviours 1, 13
        self.steps(self.short(), self.good())
        self.main(J)
        self.assertEqual(self.packs(), 2)
        self.assertEqual(self.unjudged(), 0)
        self.assertEqual(self.skipped(), [])
        self.assertEqual(self.resent(), [{"customId": self.cid, "unjudgedBefore": 2, "unjudgedAfter": 0,
                                          "kept": "resent", "resend": "success"}])

    def test_still_short_after_the_resend_is_not_looped(self):  # behaviour 2
        self.steps(self.short())  # the fake repeats its last step
        self.main(J)
        self.assertEqual(self.packs(), 2)
        self.assertEqual(self.unjudged(), 2)
        self.assertEqual([s["reason"] for s in self.skipped()],
                         ["t1 c2: 1 verdicts for 2 queries; entry dropped"])
        row = self.resent()[0]
        self.assertEqual((row["unjudgedBefore"], row["unjudgedAfter"]), (2, 2))
        self.assertEqual(self.judged(), [])

    def test_missing_candidate_entry_is_resent(self):  # behaviour 3
        self.steps(self.no_c2(), self.good())
        self.main(J)
        self.assertEqual(self.packs(), 2)
        self.assertEqual(self.unjudged(), 0)
        self.assertEqual((self.resent()[0]["unjudgedBefore"], self.resent()[0]["unjudgedAfter"]), (2, 0))

    def test_duplicate_entry_with_every_pair_judged_is_not_resent(self):  # behaviour 4
        self.steps(T.cli_ok(self.verdicts((1, [NA, NA]), (1, [NA, NA]), (2, [NA, NA]))), self.empty())
        self.main(J)
        self.assertEqual(self.packs(), 1)
        self.assertEqual(self.unjudged(), 0)
        self.assertEqual(self.resent(), [])

    def test_good_first_response_writes_an_empty_resent_file_with_one_call(self):  # behaviour 9
        self.steps(self.good(), self.empty())
        self.main(J)
        self.assertEqual(self.packs(), 1)
        self.assertTrue((self.jdir / "judge-resent.jsonl").exists())
        self.assertEqual(self.resent(), [])

    def test_resume_after_a_successful_resend_makes_no_calls(self):  # behaviour 5
        self.steps(self.short(), self.good(), self.empty())
        self.main(J)
        n = self.packs()
        self.main(J)
        self.assertEqual(self.packs(), n)
        self.assertEqual(self.resent(), [])
        self.assertEqual(self.unjudged(), 0)

    def test_resume_with_a_still_bad_response_resends_once_per_invocation(self):  # behaviour 6
        self.steps(self.short())
        self.main(J)
        self.assertEqual(self.packs(), 2)
        self.main(J)
        self.assertEqual(self.packs(), 3)
        self.assertEqual(len(self.resent()), 1)
        self.main(J)
        self.assertEqual(self.packs(), 4)

    def test_pack_with_a_recorded_failure_is_not_resent(self):  # behaviour 8
        self.steps(T.cli_err(None, "boom"), self.good())  # malformed: one call, .err.json
        with self.assertRaises(SystemExit) as cm:  # a recorded failure stops judge for a --retry
            self.main(J)
        self.assertIn("--retry", str(cm.exception.code))
        self.assertEqual(self.packs(), 1)
        self.assertTrue(CC.pack_files(self.resp, self.cid)[2].exists())
        self.assertEqual(self.resent(), [])
        self.assertEqual(self.unjudged(), 4)

    def test_retry_named_pack_with_a_short_response_is_not_resent_in_the_same_run(self):  # behaviour 16
        self.steps(self.good())
        self.main(J)
        self.assertEqual(self.packs(), 1)
        self.steps(self.short(), self.good())
        self.main(J, "--retry", self.cid)
        self.assertEqual(self.packs(), 2)  # 1 first run + 1 retry, no auto re-send
        self.assertEqual(self.resent(), [])
        self.assertEqual(self.unjudged(), 2)

    def test_dry_run_and_stub_verdict_call_nothing_and_write_no_resent_file(self):  # behaviour 10
        for extra in ((), ("--stub-verdict",)):
            argv = ["judge.py", "--version", self.version, "--dry-run", *extra]
            with mock.patch.object(sys, "argv", argv), mock.patch("builtins.print"):
                J.main()
            self.assertEqual(self.n_calls(), 0)
            self.assertFalse((self.jdir / "judge-resent.jsonl").exists())
        self.assertEqual(self.unjudged(), 0)  # the stub judged every pair


class ResendKeepsEarlier(ResendBase):
    def test_failed_resend_restores_the_earlier_response(self):  # behaviour 11
        self.steps(self.short(), T.TRANSIENT)  # transient twice -> recorded failure
        self.main(J)
        self.assertEqual(self.packs(), 3)
        out, sha, errf = CC.pack_files(self.resp, self.cid)
        self.assertFalse(errf.exists())
        self.assertEqual(CC.state(C.read_jsonl(self.jdir / "judge-requests.jsonl")[0], self.resp), "done")
        self.assertTrue((self.resp / "resend-prev" / f"{self.cid}.resend.err.json").exists())
        self.assertEqual(self.unjudged(), 2)
        row = self.resent()[0]
        self.assertEqual((row["kept"], row["unjudgedAfter"]), ("previous", 2))
        self.assertIn("transient", row["resend"])

    def test_worse_successful_resend_restores_the_earlier_response(self):  # behaviour 12
        self.steps(self.short(), self.empty())
        self.main(J)
        self.assertEqual(self.packs(), 2)
        self.assertEqual(self.unjudged(), 2)
        row = self.resent()[0]
        self.assertEqual((row["kept"], row["unjudgedBefore"], row["unjudgedAfter"]), ("previous", 2, 2))
        self.assertEqual(row["resend"], "success with 4 unjudged pairs")
        worse = json.load(open(self.resp / "resend-prev" / f"{self.cid}.resend.json"))
        self.assertEqual(worse["structured_output"], {"verdicts": []})
        out = json.load(open(CC.pack_files(self.resp, self.cid)[0]))
        self.assertEqual(len(out["structured_output"]["verdicts"]), 2)

    def test_unblind_stop_during_the_resend_exits_3_with_the_earlier_response_restored(self):  # behaviour 14
        first = self.short()
        self.steps(first, T.cli_ok({}, permission_denials=[{"t": 1}]))
        self.exit3()
        out, sha, errf = CC.pack_files(self.resp, self.cid)
        self.assertFalse(errf.exists())
        self.assertEqual(json.load(open(out)), first)
        self.assertEqual(CC.state(C.read_jsonl(self.jdir / "judge-requests.jsonl")[0], self.resp), "done")
        self.assertTrue((self.resp / "resend-prev" / f"{self.cid}.resend.err.json").exists())
        self.assertFalse((self.vdir / "queries.judged.jsonl").exists())
        self.assertFalse((self.jdir / "judge-resent.jsonl").exists())

    def test_limit_during_the_resend_exits_3_and_leaves_stored_files_byte_identical(self):  # behaviours 7, 15
        self.steps(self.short(), T.LIMIT)
        # first invocation: the first send is short, the re-send hits the limit
        self.exit3()
        first = self.snapshot()
        self.assertFalse(first[2])
        self.assertFalse((self.vdir / "queries.judged.jsonl").exists())
        self.assertFalse((self.jdir / "judge-resent.jsonl").exists())
        self.assertFalse((self.jdir / "judge-skipped.jsonl").exists())
        n = self.packs()
        # a resume under a still-active limit changes nothing either
        self.exit3()
        self.assertEqual(self.snapshot(), first)
        self.assertFalse((self.jdir / "judge-resent.jsonl").exists())
        self.assertEqual(self.packs(), n + 1)
        # the limit is over: one more re-send, resolved
        self.steps(self.good())
        self.main(J)
        self.assertEqual(self.packs(), n + 2)
        self.assertEqual(self.unjudged(), 0)
        self.assertEqual(self.resent()[0]["unjudgedAfter"], 0)


class DropBase(ResendBase):
    def dropped(self):
        return C.read_jsonl(self.jdir / "judge-dropped.jsonl")

    def drop_record(self):
        return json.load(open(self.jdir / "judge-drop.json"))

    def run_capture(self, *extra):
        """judge --run with stdout captured: returns the printed lines."""
        argv = ["judge.py", "--version", self.version, "--run", "--cwd-root", str(self.cwd_root), "--jobs", "1", *extra]
        with mock.patch.object(sys, "argv", argv), mock.patch("builtins.print") as p:
            J.main()
        return [" ".join(str(a) for a in c.args) for c in p.call_args_list]

    def multi(self, n):
        """n targets r:t1..r:tn with one query each (text qtext-i) and the candidate r:c1; 5 targets to a pack."""
        self.write_nodes([f"r:t{i}" for i in range(1, n + 1)] + ["r:c1"])
        negs = [{"key": "r:c1", "sources": ["embed"], "sameFile": False}]
        C.write_jsonl(self.vdir / "queries.mined.jsonl", [
            {"id": f"r-{i:04d}-p1", "repo": "r", "target": f"r:t{i}", "text": f"qtext-{i}", "positives": [f"r:t{i}"],
             "negatives": negs} for i in range(1, n + 1)])

    @staticmethod
    def entries(ts, bad=()):
        """Verdict entries for targets `ts` (one query, one candidate each); a target in `bad` gets 2 verdicts."""
        return {"verdicts": [{"t": t, "c": 1, "verdicts": [NA, NA] if t in bad else [NA]} for t in ts]}


class Drop(DropBase):
    def test_query_left_unjudged_after_the_resend_is_removed_and_recorded(self):  # S56-1, S56-3
        self.steps(self.short())
        self.main(J)
        self.assertEqual(self.judged(), [])
        rows = self.dropped()
        self.assertEqual([(d["id"], d["customId"], d["unjudgedPairs"]) for d in rows],
                         [("r-0001-p1", self.cid, 1), ("r-0001-p2", self.cid, 1)])
        self.assertEqual({d["reason"] for d in rows}, {"t1 c2: 1 verdicts for 2 queries; entry dropped"})
        rec = self.drop_record()
        self.assertEqual((rec["queriesIn"], rec["queriesDropped"], rec["pairsUnjudged"]), (2, 2, 2))
        self.assertEqual(rec["judgedSha256"], C.sha256_file(self.vdir / "queries.judged.jsonl"))

    def test_only_the_query_with_an_unjudged_negative_is_removed(self):  # S56-2
        self.multi(2)  # one pack, targets t1 and t2; t1's only entry has the wrong length
        self.steps(T.cli_ok(self.entries([1, 2], bad=[1])))
        self.main(J)
        self.assertEqual(self.packs(), 2)
        self.assertEqual([r["id"] for r in self.judged()], ["r-0002-p1"])
        self.assertEqual([d["id"] for d in self.dropped()], ["r-0001-p1"])
        self.assertEqual(self.drop_record()["queriesIn"], 2)
        self.assertEqual(self.drop_record()["queriesDropped"], 1)

    def test_one_stdout_line_reports_the_drop(self):  # S56 stdout line
        self.steps(self.short())
        lines = self.run_capture()
        self.assertEqual(sum(1 for line in lines if "dropped 2 of 2 queries with unjudged negatives" in line), 1)
        self.assertEqual(sum(1 for line in lines if "judge-dropped.jsonl" in line), 1)

    def test_a_clean_run_writes_empty_drop_files(self):
        self.steps(self.good())
        self.main(J)
        self.assertEqual(self.dropped(), [])
        self.assertEqual(self.drop_record()["queriesDropped"], 0)
        self.assertEqual(len(self.judged()), 2)

    def test_limit_during_the_first_send_drops_nothing(self):  # S56-8
        self.steps(T.LIMIT)
        self.exit3()
        for name in ("judge-dropped.jsonl", "judge-drop.json"):
            self.assertFalse((self.jdir / name).exists(), name)
        self.assertFalse((self.vdir / "queries.judged.jsonl").exists())

    def test_limit_during_the_resend_leaves_earlier_outputs_untouched(self):  # S56-8
        self.jdir.mkdir(parents=True, exist_ok=True)
        sentinel = {self.jdir / "judge-dropped.jsonl": b"old dropped\n", self.jdir / "judge-drop.json": b"{}\n",
                    self.vdir / "queries.judged.jsonl": b"old judged\n"}
        for path, data in sentinel.items():
            path.write_bytes(data)
        self.steps(self.short(), T.LIMIT)  # first send is short, the re-send hits the limit
        self.exit3()
        for path, data in sentinel.items():
            self.assertEqual(path.read_bytes(), data, path.name)

    def test_recorded_failure_keeps_its_queries_and_exits_1_naming_the_retry(self):  # S62-1, S62-2
        self.steps(T.cli_err(None, "boom"))  # malformed -> .err.json
        with self.assertRaises(SystemExit) as cm:
            self.main(J)
        self.assertIsInstance(cm.exception.code, str)  # exit status 1, not the resumable 3
        self.assertIn(f"--retry {self.cid}", cm.exception.code)
        self.assertEqual(self.packs(), 1)
        self.assertEqual(len(self.judged()), 2)  # kept, negatives unjudged
        self.assertTrue(all(n["judge"] is None for r in self.judged() for n in r["negatives"]))
        self.assertEqual(self.dropped(), [])
        self.assertEqual(self.drop_record()["queriesDropped"], 0)
        with mock.patch.object(sys, "argv", ["export.py", "--version", self.version]):  # S62-5
            with self.assertRaises(SystemExit) as ex:
                X.main()
        self.assertIn("4 unjudged negatives", str(ex.exception.code))

    def test_failed_pack_is_kept_while_a_short_pack_is_dropped(self):  # S62-3
        self.multi(6)  # pack 1: t1-t5, pack 2: t6
        self.set_fake([{"match": "qtext-6", "steps": [{"stdout": T.cli_err(None, "boom")}]},
                       {"match": "qtext-1", "steps": [{"stdout": T.cli_ok(self.entries([1, 2, 3, 4, 5], bad=[1]))}]}])
        with self.assertRaises(SystemExit) as cm:
            self.main(J)
        self.assertIsInstance(cm.exception.code, str)
        self.assertIn("--retry judge-r-0006", cm.exception.code)
        self.assertNotIn("judge-r-0001", cm.exception.code)
        self.assertEqual(self.packs(), 3)  # pack 1 twice (first send, one re-send), pack 2 once
        self.assertEqual([d["id"] for d in self.dropped()], ["r-0001-p1"])
        self.assertEqual([r["id"] for r in self.judged()], [f"r-{i:04d}-p1" for i in range(2, 7)])
        self.assertIsNone(next(r for r in self.judged() if r["id"] == "r-0006-p1")["negatives"][0]["judge"])

    def test_failed_resend_is_not_a_recorded_failure_and_the_short_queries_are_dropped(self):  # S62-4
        self.steps(self.short(), T.TRANSIENT)
        self.main(J)  # exit 0
        self.assertEqual(self.judged(), [])
        self.assertEqual(len(self.dropped()), 2)
        self.assertEqual(self.resent()[0]["kept"], "previous")

    def test_stub_verdict_removes_the_drop_record_and_plain_dry_run_keeps_it(self):  # S62-6, S56-9
        self.steps(self.short())
        self.main(J)
        before = self.packs()
        names = ("judge-dropped.jsonl", "judge-drop.json")
        argv = ["judge.py", "--version", self.version, "--dry-run"]
        with mock.patch.object(sys, "argv", argv), mock.patch("builtins.print"):
            J.main()
        self.assertTrue(all((self.jdir / n).exists() for n in names))
        self.assertEqual(self.judged(), [])
        with mock.patch.object(sys, "argv", argv + ["--stub-verdict"]), mock.patch("builtins.print"):
            J.main()
        self.assertFalse(any((self.jdir / n).exists() for n in names))
        self.assertEqual(len(self.judged()), 2)
        self.assertEqual(self.packs(), before)


class DropExport(RE.ExportBase):
    def setUp(self):
        super().setUp()
        self.write_judged(self.rows)

    def record(self, **over):
        rec = {"judgedSha256": C.sha256_file(self.vdir / "queries.judged.jsonl"), "queriesIn": 9,
               "queriesDropped": 7, "pairsUnjudged": 4}
        rec.update(over)
        C.write_json(self.vdir / "judge" / "judge-drop.json", rec)
        return rec

    def run_export(self):
        self.export(self.fake_guard(0, lambda d: self.guard_report("clean", C.sha256_file(d))))

    def test_record_is_read_not_recomputed_into_manifest_and_card(self):  # S56-4
        rec = self.record()
        self.run_export()
        self.assertEqual(json.load(open(self.vdir / "manifest.json"))["judgeDrop"], rec)
        self.assertIn("Judge drop: dropped 7 of 9 queries with unjudged negatives (4 pairs;",
                      (self.vdir / "card.md").read_text())

    def test_no_record_says_not_recorded(self):  # S56-6
        self.run_export()
        self.assertIsNone(json.load(open(self.vdir / "manifest.json"))["judgeDrop"])
        self.assertIn("Judge drop: not recorded.", (self.vdir / "card.md").read_text())

    def test_record_of_another_judged_file_stops_the_export(self):  # S56-5
        self.record(judgedSha256="0" * 64)
        with self.assertRaises(SystemExit) as cm:
            self.run_export()
        self.assertIn("judge-drop.json", str(cm.exception.code))
        self.assertEqual(self.guard_calls, [])
        self.assert_nothing_exported()

    def test_unjudged_gate_runs_before_the_record_check(self):  # S56-7
        self.record()
        self.rows[1] = RE.row(2, "mini-redis", judged=False)
        self.write_judged(self.rows)  # the record's sha no longer matches either
        with self.assertRaises(SystemExit) as cm:
            self.run_export()
        self.assertIn("1 unjudged negatives", str(cm.exception.code))
        self.assert_nothing_exported()


if __name__ == "__main__":
    unittest.main()
