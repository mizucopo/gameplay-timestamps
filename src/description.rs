//! Write a pasteable description and keep every confirmed item when it overflows.
use std::cmp::Reverse;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::Result;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

use crate::{analyze::Event, clock, editor};

const LIMIT: usize = 5000;

// YouTube's Japanese Studio help expresses its limit in half-width characters.
// Count line endings and keep a UTF-16 unit even for a zero-width character.
fn text_cost(text: &str) -> usize {
    text.chars()
        .map(|c| match c {
            '<' | '>' => 2,
            _ => c.width_cjk().unwrap_or(1).max(c.len_utf16()),
        })
        .sum()
}

fn line_cost(event: &Event) -> usize {
    clock(event.start).len() + 2 + text_cost(&event.heading)
}

fn shorten(event: &Event) -> String {
    let mut remaining = LIMIT - clock(event.start).len() - 2 - text_cost("…");
    let mut title = String::new();
    for part in event.heading.graphemes(true).take(36) {
        let cost = text_cost(part);
        if cost > remaining {
            break;
        }
        title.push_str(part);
        remaining -= cost;
    }
    if title.is_empty() {
        return "場面の記録".to_owned();
    }
    title.push('…');
    title
}

fn write_pair<'a>(
    dir: &Path,
    suffix: &str,
    events: impl IntoIterator<Item = &'a Event>,
) -> Result<()> {
    let mut text = BufWriter::new(File::create(dir.join(format!("timestamps{suffix}.txt")))?);
    let mut evidence = BufWriter::new(File::create(dir.join(format!("evidence{suffix}.jsonl")))?);
    for event in events {
        writeln!(text, "{} {}", clock(event.start), event.heading)?;
        serde_json::to_writer(&mut evidence, event)?;
        writeln!(evidence)?;
    }
    text.flush()?;
    evidence.flush()?;
    Ok(())
}

fn select(events: &[Event]) -> Vec<bool> {
    let mut keep = vec![false; events.len()];
    let costs: Vec<_> = events.iter().map(line_cost).collect();
    let mut resume = None;
    let priorities: Vec<_> = events
        .iter()
        .map(|event| {
            if editor::resumes_play(event) {
                resume = Some(event.start);
                return 3;
            }
            // Keep an immediate, independently witnessed introduction after a
            // resume even when there are many later on-screen names.
            let priority = editor::chapter_priority(event);
            if priority > 0
                && !event.basis.trim().is_empty()
                && resume.is_some_and(|t| event.start.abs_diff(t) <= 10)
            {
                return 3;
            }
            priority
        })
        .collect();
    let mut distances = vec![u64::MAX; events.len()];
    let mut remaining = LIMIT;
    // Keep the ends of the recording. For entries of equal importance, fill
    // parts of the timeline that are farthest from an already selected entry.
    let mut ends = [0, events.len() - 1].into_iter();
    loop {
        let fits = |&i: &usize| !keep[i] && costs[i] <= remaining;
        let next = ends.find(fits).or_else(|| {
            (0..events.len())
                .filter(fits)
                .max_by_key(|&i| (priorities[i], distances[i], Reverse(i)))
        });
        let Some(i) = next else { break };
        keep[i] = true;
        remaining -= costs[i];
        for (distance, event) in distances.iter_mut().zip(events) {
            *distance = (*distance).min(event.start.abs_diff(events[i].start));
        }
    }
    keep
}

pub fn write(dir: &Path, mut events: Vec<Event>) -> Result<()> {
    let overflow = events.iter().map(line_cost).sum::<usize>() > LIMIT;
    if overflow
        || events
            .iter()
            .any(|event| event.heading.contains(['<', '>']))
    {
        write_pair(dir, "-full", &events)?;
    }
    for event in &mut events {
        // The ASCII angle brackets are reserved in video descriptions.
        if event.heading.contains(['<', '>']) {
            event.heading = event.heading.replace('<', "＜").replace('>', "＞");
        }
        if line_cost(event) > LIMIT {
            // A backend may ignore the requested 36-character heading schema.
            event.heading = shorten(event);
        }
    }
    if !overflow {
        return write_pair(dir, "", &events);
    }
    let keep = select(&events);
    let count = keep.iter().filter(|&&item| item).count();
    eprintln!(
        "概要欄に収まる {count} 行を出力します。全 {} 行は timestamps-full.txt に保存します",
        events.len()
    );
    write_pair(
        dir,
        "",
        events
            .iter()
            .zip(keep)
            .filter_map(|(event, keep)| keep.then_some(event)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn event(start: u64, heading: &str) -> Event {
        Event {
            start,
            seen: start,
            heading: heading.into(),
            basis: "画面表示".into(),
            start_image: Some(format!("frames/{start:08}.jpg")),
            evidence_image: format!("frames/{start:08}.jpg"),
            screen_label: None,
            ocr: String::new(),
        }
    }

    #[test]
    fn exact_limit_and_reserved_punctuation_keep_text_and_evidence_in_sync() -> Result<()> {
        let exact = tempdir()?;
        let heading = format!("{}<2>", "あ".repeat(2494));
        let first = event(0, &heading);
        assert_eq!(line_cost(&first), LIMIT - 1);
        let heading = format!("{heading}!");
        assert_eq!(line_cost(&event(0, &heading)), LIMIT);
        write(exact.path(), vec![event(0, &heading)])?;
        let normalized = heading.replace("<2>", "＜2＞");
        assert_eq!(
            fs::read_to_string(exact.path().join("timestamps.txt"))?,
            format!("0:00 {normalized}\n")
        );
        let evidence: Event =
            serde_json::from_str(&fs::read_to_string(exact.path().join("evidence.jsonl"))?)?;
        assert_eq!(evidence.heading, normalized);
        assert_eq!(
            fs::read_to_string(exact.path().join("timestamps-full.txt"))?,
            format!("0:00 {heading}\n")
        );
        let raw: Event = serde_json::from_str(&fs::read_to_string(
            exact.path().join("evidence-full.jsonl"),
        )?)?;
        assert_eq!(raw.heading, heading);
        Ok(())
    }

    #[test]
    fn shortening_keeps_graphemes_whole_and_always_leaves_room_for_time() -> Result<()> {
        let dir = tempdir()?;
        let prefix = format!("{}か\u{3099}", "章".repeat(35));
        let heading = format!("{prefix}{}", "雪".repeat(2500));
        let large_cluster = format!("字{}", "\u{3099}".repeat(5000));
        write(
            dir.path(),
            vec![event(0, &heading), event(1, &large_cluster)],
        )?;
        let paste = fs::read_to_string(dir.path().join("timestamps.txt"))?;
        assert_eq!(paste, format!("0:00 {prefix}…\n0:01 場面の記録\n"));
        assert!(
            fs::read_to_string(dir.path().join("timestamps-full.txt"))?.contains(&large_cluster)
        );
        Ok(())
    }

    #[test]
    fn overflowing_description_spans_the_recording_and_keeps_independent_close_events() -> Result<()>
    {
        let mut events: Vec<_> = (0..300)
            .map(|i| {
                let name = format!("星の記録の第{i}巻");
                let mut item = event(i * 120, &format!("{name}を手に取る"));
                item.screen_label = Some(name);
                item
            })
            .collect();
        let mut place = event(73, "北の砦の紹介");
        place.basis = "場面が切り替わり、砦の入口が映る".into();
        events.splice(1..1, [event(69, "プレイ再開"), place]);
        let dir = tempdir()?;
        write(dir.path(), events)?;
        let paste = fs::read_to_string(dir.path().join("timestamps.txt"))?;
        let raw = fs::read_to_string(dir.path().join("timestamps-full.txt"))?;
        assert_eq!(raw.lines().count(), 302);
        assert!(paste.lines().count() < raw.lines().count());
        assert!(paste.contains("1:09 プレイ再開\n1:13 北の砦の紹介\n"));
        assert_eq!(paste.lines().next(), raw.lines().next());
        assert_eq!(paste.lines().last(), raw.lines().last());
        for (suffix, text) in [("", paste), ("-full", raw)] {
            let saved = fs::read_to_string(dir.path().join(format!("evidence{suffix}.jsonl")))?;
            let rows: Vec<Event> = saved
                .lines()
                .map(serde_json::from_str)
                .collect::<Result<_, _>>()?;
            assert_eq!(text.lines().count(), rows.len());
            assert!(rows.windows(2).all(|w| w[0].start <= w[1].start));
            if suffix.is_empty() {
                assert!(rows.iter().map(line_cost).sum::<usize>() <= LIMIT);
            }
            for (line, row) in text.lines().zip(rows) {
                assert_eq!(line, format!("{} {}", clock(row.start), row.heading));
            }
        }
        Ok(())
    }
}
