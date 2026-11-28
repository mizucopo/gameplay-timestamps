import csv
import io
import json
import tempfile
import unittest
from pathlib import Path

from tools import evaluate


class EvaluationTest(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.folder = Path(temp.name)

    def write_json(self, name, value):
        path = self.folder / name
        path.write_text(json.dumps(value, ensure_ascii=False), encoding="utf-8")
        return path

    def reviewed(self, name, lines):
        output = self.folder / f"{name}.txt"
        output.write_text("\n".join(line[0] for line in lines), encoding="utf-8")
        blank = io.StringIO()
        evaluate.make_sheet(output, blank)
        records = list(csv.reader(io.StringIO(blank.getvalue()), delimiter="\t"))
        for record, (_, judgments) in zip(records[1:], lines, strict=True):
            for field, value in judgments.items():
                record[records[0].index(field)] = value
        worksheet = self.folder / f"{name}.tsv"
        with worksheet.open("w", encoding="utf-8", newline="") as stream:
            csv.writer(stream, delimiter="\t").writerows(records)
        return {"timestamps": output.name, "review": worksheet.name}

    def test_semantic_matching_ranges_duplicates_and_paired_comparisons(self):
        self.write_json("blind.json", {"intervals": [[0, 15], [3598, 3610]], "events": [
            {"id": "opening", "heading": "開始", "evidence": "0:00 冒頭", "start": [0, 0]},
            {"id": "gate", "heading": "東門へ 2回目", "evidence": "0:08 地名・回数表示", "start": [6, 8],
             "names": {"place": "東門"}, "numbers": {"number": "2"}},
            {"id": "ending", "heading": "終幕", "evidence": "1:00:04 暗転後の会話", "start": [3600, 3603]},
        ]})
        self.write_json("verified.json", [
            {"id": "detour", "heading": "寄り道", "evidence": "0:50 脇道の会話", "start": [50, 52]},
        ])
        wrong_place = {"event": "gate", "wrong": "place", "correct": "number"}
        reference = self.reviewed("original", [("0:12 西門への2回目の訪問", wrong_place)])
        off = self.reviewed("off", [
            ("0:00 オープニング", {"event": "opening"}),
            ("0:04 西門へ2回目", wrong_place),
            ("0:08 西門を再訪（2回目）", wrong_place),
            ("0:09 地図の表示", {"event": "-"}),
            ("0:15 調査範囲の外", {"event": "-"}),
            ("1:00:05 幕切れ", {"event": "ending"}),
        ])
        on = self.reviewed("dictionary", [
            ("0:00 オープニング", {"event": "opening"}),
            ("0:06 東門の3回目", {"event": "gate", "correct": "place", "wrong": "number"}),
            ("1:00:02 終幕へ", {"event": "ending"}),
        ])
        audio = self.reviewed("both", [
            ("0:00 開始", {"event": "opening"}),
            ("0:51 街道を外れる", {"event": "detour"}),
        ])
        case = self.write_json("case.json", {
            "recording": "fixture", "duration_seconds": 3700, "split": "development", "blind": "blind.json",
            "verified": "verified.json", "original": reference["timestamps"], "reference_review": reference["review"],
            "runs": [{"audio": False, "dictionary": False, **off}, {"audio": False, "dictionary": True, **on},
                     {"audio": True, "dictionary": True, **audio}],
        })
        report = evaluate.score_case(case)
        self.assertEqual(report["surveyed_seconds"], 27)
        self.assertEqual(report["original_lines"], 1)
        original = report["reference"]["matched"]["gate"]
        self.assertEqual((original["heading"], original["drift_seconds"]), ("西門への2回目の訪問", 4))
        off_scores = report["runs"]["audio_off_dictionary_off"]
        self.assertEqual(off_scores["matched"]["gate"]["drift_seconds"], 0)
        self.assertEqual(off_scores["duplicates"][0]["drift_seconds"], -2)
        self.assertEqual(off_scores["matched"]["ending"]["drift_seconds"], 2)
        self.assertEqual(off_scores["missed"], ["detour"])
        self.assertEqual([r["second"] for r in off_scores["extra"]], [9])
        self.assertEqual([r["second"] for r in off_scores["unassessed"]], [15])
        self.assertEqual(off_scores["counts"]["wrong_names"], 2)
        dictionary = report["comparisons"]["audio_off_dictionary_off -> audio_off_dictionary_on"]
        self.assertEqual((dictionary["corrected_count"], dictionary["regressed_count"]), (1, 1))
        self.assertEqual(dictionary["corrected"][0]["field"], "place")
        self.assertEqual(dictionary["regressed"][0]["field"], "number")
        speech = report["comparisons"]["audio_off_dictionary_on -> audio_on_dictionary_on"]
        self.assertEqual((speech["gained"], speech["lost"]), (["detour"], ["ending", "gate"]))
        self.assertEqual(report["missing_conditions"], ["audio_on_dictionary_off"])

    def test_input_changed_after_semantic_review(self):
        files = self.reviewed("run", [("0:00 元の行", {"event": "-"})])
        timestamps = self.folder / files["timestamps"]
        timestamps.write_text("0:00 別の実行の行", encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "different output"):
            evaluate.score_lines(timestamps, self.folder / files["review"], 100, {}, [[0, 10]])

    def test_revision_comparison_pairs_starts_and_separates_missing_headings(self):
        events = {
            key: {"id": key, "heading": key, "start": [second, second + 1]}
            for key, second in [("early", 10), ("late", 20), ("lost", 30), ("gained", 40)]
        }
        condition = evaluate.condition(True, True)

        def report(name, rows):
            files = self.reviewed(name, rows)
            run = evaluate.score_lines(self.folder / files["timestamps"], self.folder / files["review"],
                                       60, events, [[0, 60]])
            return {"recording": "fixture", "split": "development", "duration_seconds": 60,
                    "events": events, "intervals": [[0, 60]], "runs": {condition: run}}

        old = report("before", [("0:05 A", {"event": "early"}), ("0:20 B", {"event": "late"}),
                                ("0:34 C", {"event": "lost"}), ("0:45 X", {"event": "-"})])
        new = report("after", [("0:11 A", {"event": "early"}), ("0:25 B", {"event": "late"}),
                               ("0:28 B", {"event": "late"}), ("0:40 D", {"event": "gained"})])
        before, after = {"recordings": [old]}, {"recordings": [new]}
        paired = evaluate.compare_revisions(before, after)["recordings"][0]["conditions"][condition]
        self.assertEqual((paired["gained"], paired["lost"]), (["gained"], ["lost"]))
        self.assertEqual(paired["entered_start_window"], ["early"])
        self.assertEqual(paired["left_start_window"], ["late"])
        self.assertEqual(paired["drift_seconds"], {
            "early": {"before": -5, "after": 0}, "late": {"before": 0, "after": 4},
        })
        self.assertEqual((paired["count_changes"]["extra"], paired["count_changes"]["duplicates"]), (-1, 1))

        with self.assertRaisesRegex(ValueError, "recordings differ"):
            evaluate.compare_revisions(before, {"recordings": []})
        with self.assertRaisesRegex(ValueError, "events"):
            evaluate.compare_revisions(before, {"recordings": [{**new, "events": {}}]})

    def test_uncertain_row_keeps_the_reason_inside_a_surveyed_interval(self):
        files = self.reviewed("run", [
            ("0:04 会話", {"event": "?", "note": "話者と話題を判別できない"}),
            ("0:05 地図", {"event": "-"}),
        ])
        score = evaluate.score_lines(self.folder / files["timestamps"], self.folder / files["review"],
                                     10, {}, [[0, 8]])
        self.assertEqual(score["counts"]["uncertain"], 1)
        self.assertEqual(score["counts"]["extra"], 1)
        self.assertEqual(score["counts"]["unassessed"], 0)
        self.assertEqual(score["uncertain"][0]["note"], "話者と話題を判別できない")

    def test_holdout_without_manual_examples_and_blind_window_validation(self):
        files = self.reviewed("holdout", [("0:09 調査済み", {"event": "-"}), ("0:10 未調査", {"event": "-"})])
        blind = {"intervals": [[0, 10]], "events": []}
        self.write_json("blind.json", blind)
        case = self.write_json("case.json", {"recording": "unseen", "duration_seconds": 11, "split": "holdout",
                                            "blind": "blind.json", "runs": [{"audio": False, "dictionary": False, **files}]})
        report = evaluate.score_case(case)
        self.assertIsNone(report["reference"])
        counts = report["runs"]["audio_off_dictionary_off"]["counts"]
        self.assertEqual((counts["extra"], counts["unassessed"]), (1, 1))
        blind["events"].append({"id": "e", "heading": "到着", "evidence": "0:10 地名表示", "start": [9, 10]})
        self.write_json("blind.json", blind)
        with self.assertRaisesRegex(ValueError, "inside a surveyed interval"):
            evaluate.score_case(case)

    def test_ungraded_field_cannot_be_counted_as_correct(self):
        event = {"names": {"boss": "甲"}, "numbers": {"form": "第二"}}
        with self.assertRaisesRegex(ValueError, "missing spelling"):
            evaluate.spelling_judgments({"correct": "boss", "wrong": "", "omitted": ""}, event, "row")


if __name__ == "__main__":
    unittest.main()
