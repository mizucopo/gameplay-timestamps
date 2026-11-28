"""Score gameplay timestamps against independently reviewed playback intervals."""

import argparse
import csv
import json
import re
import sys
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path


SHEET_COLUMNS = ["line", "time", "heading", "event", "correct", "wrong", "omitted", "note"]
TIMESTAMP = re.compile(r"(\d+):([0-5]\d)(?::([0-5]\d))?\s+(.+)")


def require(condition, message):
    if not condition:
        raise ValueError(message)


def read_json(path):
    try:
        return json.loads(path.read_text(encoding="utf-8-sig"))
    except json.JSONDecodeError as error:
        raise ValueError(f"{path}: {error}") from error


def text(value):
    return isinstance(value, str) and bool(value.strip())


@dataclass(frozen=True)
class Row:
    line: int
    time: str
    heading: str
    second: int


def read_timestamps(path):
    rows = []
    for line, raw in enumerate(path.read_text(encoding="utf-8-sig").splitlines(), 1):
        if not raw.strip():
            continue
        match = TIMESTAMP.fullmatch(raw.strip())
        require(match, f"{path}:{line}: expected M:SS or H:MM:SS and a heading")
        first, second, third, heading = match.groups()
        seconds = int(first) * 60 + int(second)
        stamp = f"{first}:{second}"
        if third is not None:
            seconds = seconds * 60 + int(third)
            stamp += f":{third}"
        rows.append(Row(line, stamp, heading, seconds))
    return rows


def make_sheet(path, output):
    writer = csv.writer(output, delimiter="\t", lineterminator="\n")
    writer.writerow(SHEET_COLUMNS)
    for row in read_timestamps(path):
        writer.writerow([row.line, row.time, row.heading, "", "", "", "", ""])


def read_sheet(path, rows):
    with path.open(encoding="utf-8-sig", newline="") as stream:
        reader = csv.DictReader(stream, delimiter="\t")
        require(reader.fieldnames == SHEET_COLUMNS, f"{path}: unexpected worksheet columns")
        sheet = list(reader)
    require(len(sheet) == len(rows), f"{path}: row count differs from the timestamp file")
    for row, judgment in zip(rows, sheet):
        require(None not in judgment and None not in judgment.values(),
                f"{path}:{row.line}: invalid worksheet column count")
        require((judgment["line"], judgment["time"], judgment["heading"]) ==
                (str(row.line), row.time, row.heading),
                f"{path}:{row.line}: the worksheet belongs to different output")
    return sheet


def span(value, duration, inclusive):
    if not isinstance(value, list) or len(value) != 2 or any(type(n) is not int for n in value):
        return False
    first, last = value
    return 0 <= first <= last < duration if inclusive else 0 <= first < last <= duration


def read_events(blind, verified, duration):
    intervals = blind["intervals"]
    require(isinstance(intervals, list) and
            all(span(pair, duration, False) for pair in intervals),
            "blind.intervals must be a list of [start, end) intervals within the recording")
    intervals = sorted(intervals)
    for current, following in zip(intervals, intervals[1:]):
        require(current[1] <= following[0], "blind.intervals overlap")
    events = {}
    for source, entries in [("blind", blind["events"]), ("verified", verified)]:
        require(isinstance(entries, list), f"{source} events must be a list")
        for event in entries:
            key = event["id"]
            require(text(key) and key not in {"-", "?"} and key not in events, f"invalid or repeated event ID {key!r}")
            first, last = event["start"]
            require(span(event["start"], duration, True), f"{key}: invalid inclusive start window")
            if source == "blind":
                require(any(lo <= first and last < hi for lo, hi in intervals),
                        f"{key}: blind start window must be inside a surveyed interval")
            require(text(event["heading"]) and text(event["evidence"]), f"{key}: missing heading or evidence")
            names, numbers = event.get("names", {}), event.get("numbers", {})
            require(isinstance(names, dict) and isinstance(numbers, dict) and names.keys().isdisjoint(numbers),
                    f"{key}: names and numbers must be mappings with distinct field IDs")
            for field, spelling in {**names, **numbers}.items():
                require(text(field) and field == field.strip() and "|" not in field and text(spelling),
                        f"{key}: invalid spelling field {field!r}")
            events[key] = {**event, "source": source}
    return intervals, events


def spelling_judgments(judgment, event, location):
    statuses = {}
    expected = set(event.get("names", {})) | set(event.get("numbers", {}))
    for status in ["correct", "wrong", "omitted"]:
        for field in filter(None, (part.strip() for part in judgment[status].split("|"))):
            require(field in expected and field not in statuses, f"{location}: unexpected or repeated field {field!r}")
            statuses[field] = status
    require(statuses.keys() == expected, f"{location}: missing spelling judgments for {sorted(expected - statuses.keys())}")
    return statuses


def score_lines(timestamp_path, review_path, duration, events, intervals):
    rows = read_timestamps(timestamp_path)
    matches = defaultdict(list)
    extra, unassessed, uncertain = [], [], []
    for row, judgment in zip(rows, read_sheet(review_path, rows)):
        location = f"{review_path} (line {row.line})"
        key = judgment["event"].strip()
        require(key in {"-", "?"} or key in events, f"{location}: set event to a reviewed ID, '-' or '?'")
        details = vars(row).copy()
        if judgment["note"]:
            details["note"] = judgment["note"]
        if key == "?":
            spelling_judgments(judgment, {}, location)
            require(text(judgment["note"]), f"{location}: explain the uncertain judgment in note")
            uncertain.append(details)
            continue
        if key == "-":
            spelling_judgments(judgment, {}, location)
            inside = any(first <= row.second < last for first, last in intervals)
            (extra if inside else unassessed).append(details)
            continue
        event = events[key]
        first, last = event["start"]
        drift = row.second - min(max(row.second, first), last)
        details.update(drift_seconds=drift, fields=spelling_judgments(judgment, event, location))
        matches[key].append(details)

    primary, duplicates = {}, []
    for key, group in matches.items():
        # A premature repeat must not hide a later, correctly timed occurrence.
        best = min(group, key=lambda item: (abs(item["drift_seconds"]), item["line"]))
        primary[key] = best
        duplicates.extend({"event": key, **item} for item in group if item is not best)
    missed = [key for key in events if key not in primary]
    counts = dict(lines=len(rows), matched=len(primary), missed=len(missed), extra=len(extra),
                  duplicates=len(duplicates), unassessed=len(unassessed), uncertain=len(uncertain),
                  out_of_recording=sum(row.second >= duration for row in rows),
                  outside_start_window=sum(item["drift_seconds"] != 0 for item in primary.values()))
    for kind in ("names", "numbers"):
        counts[f"wrong_{kind}"] = sum(item["fields"][field] == "wrong" for key, group in matches.items()
                                     for item in group for field in events[key].get(kind, {}))
    return dict(counts=counts, matched=primary, missed=missed, extra=extra, duplicates=duplicates,
                unassessed=unassessed, uncertain=uncertain)


def compare(off, on, events, compare_spelling):
    before, after = off["matched"], on["matched"]
    common = before.keys() & after.keys()
    result = dict(gained=sorted(after.keys() - before.keys()), lost=sorted(before.keys() - after.keys()),
                  common=len(common), count_changes={k: v - off["counts"][k] for k, v in on["counts"].items()})
    if compare_spelling:
        for label, old, new in [("corrected", "wrong", "correct"), ("regressed", "correct", "wrong")]:
            changes = [{"event": key, "field": field, "expected": spelling}
                       for key in sorted(common)
                       for field, spelling in {**events[key].get("names", {}), **events[key].get("numbers", {})}.items()
                       if before[key]["fields"][field] == old and after[key]["fields"][field] == new]
            result[label] = changes
            result[f"{label}_count"] = len(changes)
    return result


def compare_revisions(before, after):
    def by_title(study):
        reports = {record["recording"]: record for record in study["recordings"]}
        require(len(reports) == len(study["recordings"]), "duplicate recording title in score report")
        return reports

    old_records, new_records = by_title(before), by_title(after)
    require(old_records.keys() == new_records.keys() and old_records, "recordings differ or are empty")
    results = []
    for title, old in old_records.items():
        new = new_records[title]
        for field in ("split", "duration_seconds", "intervals"):
            require(old[field] == new[field], f"{title}: {field} differs between revisions")
        require(old["events"] == new["events"], f"{title}: reference events differ between revisions")
        conditions = {}
        for label in sorted(old["runs"].keys() & new["runs"].keys()):
            previous, current = old["runs"][label], new["runs"][label]
            delta = compare(previous, current, old["events"], compare_spelling=False)
            common = sorted(previous["matched"].keys() & current["matched"].keys())
            drift = {key: {"before": previous["matched"][key]["drift_seconds"],
                           "after": current["matched"][key]["drift_seconds"]} for key in common}
            delta["drift_seconds"] = drift
            delta["entered_start_window"] = [key for key, d in drift.items() if d["before"] != 0 and d["after"] == 0]
            delta["left_start_window"] = [key for key, d in drift.items() if d["before"] == 0 and d["after"] != 0]
            conditions[label] = delta
        require(conditions, f"{title}: no shared audio/dictionary condition")
        results.append({"recording": title, "event_count": len(old["events"]), "conditions": conditions})
    return {"recordings": results}


def condition(audio, dictionary):
    return f"audio_{'on' if audio else 'off'}_dictionary_{'on' if dictionary else 'off'}"


def score_case(path):
    case = read_json(path)
    folder = path.parent
    require(text(case["recording"]), "recording must identify the title and recording")
    require(case["split"] in ("development", "validation", "holdout"), "unknown dataset split")
    duration = case["duration_seconds"]
    require(type(duration) is int and duration > 0, "duration_seconds must be positive")
    extra_events = read_json(folder / case["verified"]) if case.get("verified") else []
    intervals, events = read_events(read_json(folder / case["blind"]), extra_events, duration)

    def evaluate_run(timestamps, review):
        return score_lines(folder / timestamps, folder / review, duration, events, intervals)

    original, reference_review = case.get("original"), case.get("reference_review")
    require(bool(original) == bool(reference_review), "provide original and reference_review together")
    require(original or case["split"] == "holdout", "a development or validation recording needs an original sample")
    reference = evaluate_run(original, reference_review) if original else None
    used_outputs = {(folder / original).resolve()} if original else set()
    scores = {}
    for run in case["runs"]:
        audio, dictionary = run["audio"], run["dictionary"]
        require(type(audio) is bool and type(dictionary) is bool, "audio and dictionary must be JSON booleans")
        label = condition(audio, dictionary)
        require(label not in scores, f"repeated run {label}")
        output = (folder / run["timestamps"]).resolve()
        require(output not in used_outputs, f"{label}: each condition needs its own saved output")
        used_outputs.add(output)
        scores[label] = evaluate_run(run["timestamps"], run["review"])
    comparisons = {}
    for fixed in (False, True):
        for control in ("audio", "dictionary"):
            keys = [condition(value, fixed) if control == "audio" else condition(fixed, value)
                    for value in (False, True)]
            if all(key in scores for key in keys):
                comparisons[f"{keys[0]} -> {keys[1]}"] = compare(scores[keys[0]], scores[keys[1]], events,
                                                                 control == "dictionary")
    require(scores, "provide at least one generated run")
    missing = [condition(a, d) for a in (False, True) for d in (False, True) if condition(a, d) not in scores]
    return dict(recording=case["recording"], split=case["split"], duration_seconds=duration, intervals=intervals,
                surveyed_seconds=sum(hi - lo for lo, hi in intervals), original_file=original,
                original_lines=reference["counts"]["lines"] if reference else 0, events=events, reference=reference,
                runs=scores, missing_conditions=missing, comparisons=comparisons)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    sheet = commands.add_parser("sheet", help="print a blank semantic review worksheet as TSV")
    sheet.add_argument("timestamps", type=Path)
    score = commands.add_parser("score", help="print per-recording scores and matched comparisons as JSON")
    score.add_argument("cases", nargs="+", type=Path)
    revisions = commands.add_parser("compare-revisions", help="compare score reports against the same reference events")
    revisions.add_argument("before", type=Path)
    revisions.add_argument("after", type=Path)
    args = parser.parse_args()
    try:
        if args.command == "sheet":
            make_sheet(args.timestamps, sys.stdout)
            return
        if args.command == "compare-revisions":
            result = compare_revisions(read_json(args.before), read_json(args.after))
        else:
            reports = []
            for path in args.cases:
                try:
                    report = score_case(path)
                    require(all(other["recording"] != report["recording"] for other in reports),
                            f"recording supplied twice: {report['recording']}")
                except (KeyError, TypeError, ValueError) as error:
                    raise ValueError(f"{path}: {error}") from error
                reports.append(report)
            result = {"original_lines": sum(r["original_lines"] for r in reports), "recordings": reports}
        json.dump(result, sys.stdout, ensure_ascii=False, indent=2)
        print()
    except (OSError, ValueError, KeyError, TypeError, csv.Error) as error:
        parser.exit(2, f"{parser.prog}: {error}\n")


if __name__ == "__main__":
    main()
