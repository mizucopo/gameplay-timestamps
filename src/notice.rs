//! Recover named quest notifications which can appear over an unrelated HUD
//! objective or conversation. A reading in two source seconds is required.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use anyhow::{Context, Result};
use image::{ImageFormat, codecs::jpeg::JpegEncoder, imageops::FilterType};
use sha2::{Digest, Sha256};

use crate::{
    analyze::{Analyzer, Event, Scan, japanese, save},
    media::{self, Frame},
    ocr, speech,
};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Status {
    noun: &'static str,
    phase: &'static str,
}

const HEADERS: &[(&str, &str, &str)] = &[
    ("クエスト開始", "クエスト", "開始"),
    ("クエスト受注", "クエスト", "開始"),
    ("クエスト完了", "クエスト", "完了"),
    ("クエスト達成", "クエスト", "完了"),
    ("クエストクリア", "クエスト", "完了"),
    ("クエスト更新", "クエスト", "更新"),
    ("ミッション開始", "ミッション", "開始"),
    ("ミッション完了", "ミッション", "完了"),
    ("ミッション更新", "ミッション", "更新"),
    ("QUESTSTART", "クエスト", "開始"),
    ("QUESTACCEPTED", "クエスト", "開始"),
    ("QUESTCLEAR", "クエスト", "完了"),
    ("QUESTCOMPLETE", "クエスト", "完了"),
    ("QUESTUPDATED", "クエスト", "更新"),
    ("MISSIONSTART", "ミッション", "開始"),
    ("MISSIONCOMPLETE", "ミッション", "完了"),
];

fn compact(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_uppercase)
        .collect::<String>()
        .replace("クェスト", "クエスト")
}

fn status(text: &str) -> Option<Status> {
    let key = compact(text);
    HEADERS
        .iter()
        .find_map(|&(header, noun, phase)| (key == header).then_some(Status { noun, phase }))
}

fn clue(text: &str) -> bool {
    let key = compact(text);
    HEADERS.iter().any(|(header, _, _)| key.contains(header)) || key.contains("クエスト追跡")
}

fn name(raw: &str) -> Option<String> {
    let text = raw.split_whitespace().collect::<String>();
    let text = text.trim_matches(['「', '」', '『', '』', '◆', '◇']);
    ((2..=36).contains(&text.chars().count())
        && japanese(text)
        && !matches!(
            text,
            "完了" | "開始" | "達成" | "追跡" | "更新" | "クエスト" | "ミッション" | "クリア"
        )
        && !text.ends_with(['・', '：', ':', '、'])
        && !text.contains(['。', '/', '／', '…'])
        && ![
            "クエスト追跡",
            "目標を表示",
            "目的地を設定",
            "冒険手帳を開く",
            "クエストを開く",
            "閉じる",
            "ボタン",
            "メニュー",
        ]
        .iter()
        .any(|ui| text.contains(ui))
        && status(text).is_none())
    .then(|| text.to_owned())
}

#[derive(Default)]
struct Reading {
    source: [u8; 32],
    names: BTreeSet<String>,
    paired: BTreeMap<(Status, String), String>,
}

impl Reading {
    fn add(&mut self, text: &str, localized: bool) {
        let lines: Vec<_> = text
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if localized {
            self.names.extend(lines.iter().filter_map(|s| name(s)));
        }
        for adjacent in lines.windows(2) {
            if let Some(phase) = status(adjacent[0])
                && let Some(title) = name(adjacent[1])
                // A full-screen OCR may place an unrelated HUD goal immediately
                // after the banner. The title itself must be in the central crop.
                && self.names.contains(&title)
            {
                self.paired
                    .entry((phase, title))
                    .or_insert_with(|| adjacent[0].to_string());
            }
        }
    }
}

fn crop(frame: &Frame, zoom: bool) -> Result<Vec<u8>> {
    let img = image::load_from_memory_with_format(&frame.jpeg, ImageFormat::Jpeg)?;
    let (w, h) = (img.width(), img.height());
    // The full-size band preserves broad titles, the tighter enlargement
    // reads small subtitles and chapter numbers below the status line.
    let (x, y, width, height) = if zoom {
        (25, 11, 50, 18)
    } else {
        (22, 8, 56, 30)
    };
    let mut region = img.crop_imm(
        w * x / 100,
        h * y / 100,
        (w * width / 100).max(1),
        (h * height / 100).max(1),
    );
    if zoom {
        let target = region.width().saturating_mul(2).min(1500);
        region = region
            .resize_exact(
                target,
                (region.height() * target / region.width()).max(1),
                FilterType::Lanczos3,
            )
            .unsharpen(0.8, 3);
    }
    let mut bytes = Vec::new();
    JpegEncoder::new_with_quality(&mut bytes, 92).encode_image(&region)?;
    Ok(bytes)
}

fn read(frame: &Frame, command: Option<&Path>) -> Result<Reading> {
    let mut reading = Reading {
        source: Sha256::digest(&frame.jpeg).into(),
        ..Default::default()
    };
    for zoom in [false, true] {
        reading.add(&ocr::read(&crop(frame, zoom)?, command)?, true);
    }
    if !reading.names.is_empty() {
        let full = ocr::read(&frame.jpeg, command)?;
        if full.lines().any(|s| {
            matches!(
                s.trim(),
                "クエスト一覧" | "ミッション一覧" | "クエスト履歴" | "完了一覧"
            )
        }) {
            return Ok(Reading::default());
        }
        reading.add(&full, false);
    }
    Ok(reading)
}

#[derive(PartialEq, Eq)]
struct Card {
    status: Status,
    title: String,
    header: String,
    first: u64,
    seen: u64,
    corroborated: u64,
}

// A clipped stage suffix is common while a notice fades in. A recognized
// numeric separator is required so "探検1" is never a prefix match for "探検12".
fn numbered_title(short: &str, full: &str) -> bool {
    full.strip_prefix(short)
        .and_then(|s| s.strip_prefix(['・', '：', ':', '、']))
        .is_some_and(|n| !n.is_empty() && n.chars().all(char::is_numeric))
}

fn cards(readings: &BTreeMap<u64, Reading>) -> Vec<Card> {
    let mut found: Vec<Card> = Vec::new();
    for (&seen, reading) in readings {
        for ((status, title), header) in &reading.paired {
            let corroborated = readings
                .range(seen.saturating_sub(3)..=seen.saturating_add(3))
                .filter(|(t, r)| {
                    **t != seen
                        && r.source != reading.source
                        && r.names.contains(title)
                        && !r.paired.keys().any(|(s, n)| n == title && s != status)
                })
                .min_by_key(|(t, _)| t.abs_diff(seen))
                .map(|(&t, _)| t);
            let Some(corroborated) = corroborated else {
                continue;
            };
            let card = Card {
                status: *status,
                title: title.clone(),
                header: header.clone(),
                first: seen,
                seen,
                corroborated,
            };
            if let Some(last) = found
                .iter_mut()
                .rev()
                .find(|c| c.status == *status && c.title == *title && seen - c.seen <= 5)
            {
                last.seen = seen;
                last.corroborated = corroborated;
                last.header = header.clone();
            } else {
                found.push(card);
            }
        }
    }
    // A fading number can yield both the title and its visibly longer
    // numbered form in the same card. Prefer the independently corroborated
    // full reading; never combine two different numbers or two phases.
    let chosen: Vec<_> = found
        .iter()
        .map(|c| {
            !found.iter().any(|other| {
                c.status == other.status
                    && c.first <= other.seen.saturating_add(2)
                    && other.first <= c.seen.saturating_add(2)
                    && numbered_title(&c.title, &other.title)
            })
        })
        .collect();
    found
        .into_iter()
        .zip(chosen)
        .filter_map(|(c, keep)| keep.then_some(c))
        .collect()
}

fn seed_seconds(events: &[Event]) -> BTreeSet<u64> {
    let mut seconds = BTreeSet::new();
    for e in events {
        for line in e.ocr.lines().filter(|line| clue(line)) {
            // Several observations retain OCR from neighboring frames. Read its
            // source second rather than the timestamp of the unrelated heading.
            let second = line
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .filter(|t| (e.start.saturating_sub(3)..=e.seen.saturating_add(3)).contains(t))
                .unwrap_or(e.seen);
            seconds.insert(second);
        }
    }
    seconds
}

fn merge(events: &mut Vec<Event>, notice: Event, card: &Card) {
    let phase: &[&str] = match card.status.phase {
        "完了" => &["完了", "達成", "クリア"],
        "開始" => &["開始", "受注", "引き受け"],
        _ => &["更新"],
    };
    let mut replacement = notice;
    events.retain(|e| {
        let quoted = e
            .heading
            .split_once(['「', '『'])
            .and_then(|(_, s)| s.split_once(['」', '』']))
            .map(|(s, _)| s);
        let named = match quoted {
            Some(title) => title == card.title || numbered_title(title, &card.title),
            None => e.heading.split_once(&card.title).is_some_and(|(_, rest)| {
                let rest = rest.trim_start();
                rest.is_empty()
                    || rest.starts_with(['を', 'が', 'の', 'は', 'に'])
                    || phase.iter().any(|verb| rest.starts_with(verb))
            }),
        };
        let generic = ["依頼", "クエスト", "任務", "ミッション"]
            .iter()
            .any(|noun| {
                e.heading
                    .strip_prefix(noun)
                    .is_some_and(|s| phase.contains(&s.trim_start_matches('の')))
            });
        let matches = e.seen >= card.first.saturating_sub(3)
            && e.seen <= card.seen.saturating_add(3)
            && (generic || named)
            && phase.iter().any(|s| e.heading.contains(s));
        if matches && e.start <= replacement.start && e.start_image.is_some() {
            replacement.start = e.start;
            replacement.start_image = e.start_image.clone();
        }
        !matches
    });
    events.push(replacement);
}

pub fn supplement(a: &Analyzer<'_>, video: &Path, step: u32, scan: &mut Scan) -> Result<()> {
    if !a.use_ocr {
        return Ok(());
    }
    let mut seeds = seed_seconds(&scan.events);
    let mut visited = BTreeSet::new();
    for event in &scan.events {
        if !visited.insert(event.seen)
            || seeds
                .range(event.seen.saturating_sub(2)..=event.seen.saturating_add(2))
                .next()
                .is_some()
        {
            continue;
        }
        let frame = Frame {
            second: event.seen,
            jpeg: fs::read(a.dir.join(&event.evidence_image))?,
        };
        let result = ocr::read(&crop(&frame, false)?, a.ocr_command);
        match result {
            Ok(text) if text.lines().any(|line| status(line).is_some()) => {
                seeds.insert(frame.second);
            }
            Ok(_) => {}
            Err(error) => {
                eprintln!("依頼通知を再読できません ({error})");
                return Ok(());
            }
        }
    }
    // A silent recording can have no proposed event at all. In that case
    // there are no evidence images to revisit beyond the opening. Search
    // the recording on the CLI's sampling grid for the same visual marker.
    if scan.events.len() == 1
        && let Err(error) = media::scan(video, step, |frame| {
            let text = ocr::read(&crop(&frame, false)?, a.ocr_command)?;
            if text.lines().any(|line| status(line).is_some()) {
                seeds.insert(frame.second);
            }
            Ok(())
        })
    {
        eprintln!("依頼通知の追加走査を続けられません ({error})");
        return Ok(());
    }
    let seconds: Vec<_> = seeds
        .iter()
        .flat_map(|t| t.saturating_sub(3)..=t.saturating_add(3))
        .filter(|t| !scan.cursor_seconds.contains(t))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    // Release the images in bounded batches. Their small timestamped readings
    // must remain together to confirm a pair straddling two decode batches.
    let mut readings = BTreeMap::new();
    for chunk in seconds.chunks(48) {
        for (second, frame) in speech::frames::sample(video, chunk.to_vec())? {
            match read(&frame, a.ocr_command) {
                Ok(reading) => {
                    readings.insert(second, reading);
                }
                Err(error) => {
                    eprintln!("依頼通知を再読できません ({error})");
                    return Ok(());
                }
            }
        }
    }
    for card in cards(&readings) {
        let frames = speech::frames::sample(video, vec![card.first, card.seen])?;
        let first = frames
            .get(&card.first)
            .context("依頼通知の最初の画面がありません")?;
        let proof = frames
            .get(&card.seen)
            .context("依頼通知の根拠画面がありません")?;
        let event = Event {
            start: card.first,
            seen: card.seen,
            heading: format!(
                "{}「{}」{}",
                card.status.noun, card.title, card.status.phase
            ),
            basis: format!(
                "{}秒の通知に「{}」「{}」を表示、{}秒の別画面でも同名を確認",
                card.seen, card.header, card.title, card.corroborated
            ),
            start_image: Some(save(a.dir, first)?),
            evidence_image: save(a.dir, proof)?,
            screen_label: Some(card.title.clone()),
            ocr: format!(
                "{} {}\n{} {}\n{} {}",
                card.seen, card.header, card.seen, card.title, card.corroborated, card.title
            ),
        };
        merge(&mut scan.events, event, &card);
    }
    scan.events.sort_by_key(|e| e.start);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(source: u8, crop: &str, full: &str) -> Reading {
        let mut r = Reading {
            source: [source; 32],
            ..Default::default()
        };
        r.add(crop, true);
        r.add(full, false);
        r
    }

    #[test]
    fn requires_a_title_by_the_status_and_another_source_second() {
        let reads = BTreeMap::from([
            (18, screen(1, "北の灯台", "")),
            (
                19,
                screen(2, "QUEST CLEAR\n北の灯台", "QUEST CLEAR\n西へ向かう"),
            ),
            (
                20,
                screen(3, "クエスト開始\nクエスト追跡", "クエスト開始\n南の街道"),
            ),
            (21, screen(4, "クエスト完了時の報酬\n南の街道", "")),
            (22, screen(5, "クエスト完了\n孤島の謎・3", "")),
            // A held VFR picture is still only one source image.
            (23, screen(5, "クエスト完了\n孤島の謎・3", "")),
        ]);
        let found = cards(&reads);
        assert_eq!(found.len(), 1);
        assert_eq!(
            (&*found[0].title, found[0].seen, found[0].corroborated),
            ("北の灯台", 19, 18)
        );
    }

    #[test]
    fn repeated_cards_keep_started_and_completed_phases_and_a_later_repeat() {
        let reads = BTreeMap::from([
            (100, screen(1, "クエスト開始\n島の調査・2", "")),
            (101, screen(2, "クエスト開始\n島の調査・2", "")),
            (104, screen(3, "クェスト完了\n島の調査・2", "")),
            (105, screen(4, "島の調査・2", "")),
            (200, screen(5, "クエスト開始\n島の調査・2", "")),
            (201, screen(6, "島の調査・2", "")),
        ]);
        let found = cards(&reads);
        assert_eq!(
            found
                .iter()
                .map(|c| (c.first, c.status.phase))
                .collect::<Vec<_>>(),
            [(100, "開始"), (104, "完了"), (200, "開始")]
        );
    }

    #[test]
    fn the_numbered_card_replaces_its_clipped_or_generic_completion_only() {
        let reads = BTreeMap::from([
            (47, screen(1, "クエスト完了\n島の調査", "")),
            (
                48,
                screen(2, "島の調査\n島の調査・2", "QUEST CLEAR\n島の調査・2"),
            ),
            (49, screen(3, "QUEST CLEAR\n島の調査・2", "")),
        ]);
        let found = cards(&reads);
        assert_eq!(found.len(), 1);
        let card = &found[0];
        assert_eq!(card.title, "島の調査・2");
        assert_eq!(card.header, "QUEST CLEAR");
        let event = |start, seen, heading: &str| Event {
            start,
            seen,
            heading: heading.into(),
            basis: String::new(),
            start_image: Some(format!("frames/{start:08}.jpg")),
            evidence_image: format!("frames/{seen:08}.jpg"),
            screen_label: None,
            ocr: String::new(),
        };
        let mut events = vec![
            event(25, 48, "クエスト「島の調査」完了"),
            event(48, 48, "依頼完了"),
            event(48, 49, "島の調査・2を達成"),
            event(46, 48, "クエスト「島の調査・1」完了"),
            event(47, 48, "クエスト「島の調査・2」開始"),
            event(47, 49, "島の調査・20を完了"),
            event(85, 87, "クエスト「島の調査」完了"),
        ];
        merge(
            &mut events,
            event(48, 49, "クエスト「島の調査・2」完了"),
            card,
        );
        assert_eq!(events.len(), 5);
        assert_eq!((events[4].start, events[4].seen), (25, 49));
        assert_eq!(
            events[4].start_image.as_deref(),
            Some("frames/00000025.jpg")
        );
        assert_eq!(events[4].evidence_image, "frames/00000049.jpg");
        assert_eq!(events[2].heading, "島の調査・20を完了");
        assert_eq!(
            (events[0].start, events[1].start, events[3].start),
            (46, 47, 85)
        );
    }
}
