//! Recover named quest notifications which can appear over an unrelated HUD
//! objective or conversation. A reading in two source seconds is required.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use anyhow::{Context, Result};
use image::{
    DynamicImage, GenericImage, ImageFormat, RgbImage, codecs::jpeg::JpegEncoder,
    imageops::FilterType,
};

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

fn crop(frame: &Frame, zoom: bool) -> Result<DynamicImage> {
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
    Ok(region)
}

fn recognize(img: &DynamicImage, command: Option<&Path>) -> Result<String> {
    let mut bytes = Vec::new();
    JpegEncoder::new_with_quality(&mut bytes, 92).encode_image(img)?;
    ocr::read(&bytes, command)
}

// The sheet only nominates source seconds. Its text can never supply a name
// or a phase in an event; those must be read from individual source images.
fn screen_group(
    group: &[(u64, DynamicImage)],
    command: Option<&Path>,
    seeds: &mut BTreeSet<u64>,
) -> Result<()> {
    if group.is_empty() {
        return Ok(());
    }
    let w = group.iter().map(|(_, img)| img.width()).max().unwrap();
    let h = group.iter().map(|(_, img)| img.height()).max().unwrap();
    let cols = group.len().min(2) as u32;
    let rows = (group.len() as u32).div_ceil(cols);
    let mut sheet = RgbImage::new(w * cols, h * rows);
    for (i, (_, region)) in group.iter().enumerate() {
        sheet.copy_from(&region.to_rgb8(), i as u32 % cols * w, i as u32 / cols * h)?;
    }
    if clue(&recognize(&DynamicImage::ImageRgb8(sheet), command)?) {
        seeds.extend(group.iter().map(|(sec, _)| *sec));
    }
    Ok(())
}

fn read(frame: &Frame, command: Option<&Path>) -> Result<Reading> {
    let mut reading = Reading::default();
    for zoom in [false, true] {
        reading.add(&recognize(&crop(frame, zoom)?, command)?, true);
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
                        && r.names.contains(title)
                        && !readings
                            .range(seen.min(**t)..=seen.max(**t))
                            .any(|(_, other)| {
                                other.paired.keys().any(|(s, n)| n == title && s != status)
                            })
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
            if let Some(last) = found.iter_mut().rev().find(|c| c.title == *title)
                && last.status == *status
                && seen - last.seen <= 5
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

fn describes(heading: &str, card: &Card) -> bool {
    let phase: &[&str] = match card.status.phase {
        "完了" => &["完了", "達成", "クリア"],
        "開始" => &["開始", "受注", "引き受け"],
        _ => &["更新"],
    };
    if let Some((title, rest)) = heading
        .split_once(['「', '『'])
        .and_then(|(_, s)| s.split_once(['」', '』']))
    {
        return (title == card.title || numbered_title(title, &card.title))
            && phase.iter().any(|word| rest.contains(word));
    }
    if let Some((_, rest)) = heading.split_once(&card.title) {
        let rest = rest.trim_start();
        return (rest.starts_with(['を', 'が', 'の', 'は', 'に'])
            || phase.iter().any(|verb| rest.starts_with(verb)))
            && phase.iter().any(|verb| rest.contains(verb));
    }
    ["依頼", "クエスト", "任務", "ミッション"]
        .iter()
        .any(|noun| {
            heading
                .strip_prefix(noun)
                .is_some_and(|s| phase.contains(&s.trim_start_matches('の')))
        })
}

fn merge(events: &mut Vec<Event>, mut notices: Vec<(Card, Event)>) {
    events.retain(|e| {
        // Associate each pre-existing row with its nearest matching card.
        // A new row can never consume another independently confirmed notice.
        let closest = notices
            .iter_mut()
            .filter(|(card, _)| {
                (card.first.saturating_sub(3)..=card.seen.saturating_add(3)).contains(&e.seen)
                    && describes(&e.heading, card)
            })
            .min_by_key(|(card, _)| e.seen.abs_diff(e.seen.clamp(card.first, card.seen)));
        let Some((_, replacement)) = closest else {
            return true;
        };
        if e.start <= replacement.start && e.start_image.is_some() {
            replacement.start = e.start;
            replacement.start_image = e.start_image.clone();
        }
        false
    });
    events.extend(notices.into_iter().map(|(_, e)| e));
}

pub fn supplement(a: &Analyzer<'_>, video: &Path, step: u32, scan: &mut Scan) -> Result<()> {
    if !a.use_ocr {
        return Ok(());
    }
    let mut seeds = seed_seconds(&scan.events);
    let mut group = Vec::with_capacity(4);
    let mut previous = None;
    let sweep = media::scan(video, step, |frame| {
        // Anchor the groups on the playback grid so nominations elsewhere
        // cannot change how later, unrelated frames are presented to OCR.
        let block = frame.second / (4 * u64::from(step));
        if previous.replace(block).is_some_and(|prev| prev != block) {
            screen_group(&group, a.ocr_command, &mut seeds)?;
            group.clear();
        }
        if !seeds.contains(&frame.second) && !scan.cursor_seconds.contains(&frame.second) {
            group.push((frame.second, crop(&frame, false)?));
        }
        Ok(())
    })
    .and_then(|()| screen_group(&group, a.ocr_command, &mut seeds));
    if let Err(error) = sweep {
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
    let mut notices = Vec::new();
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
                "{}秒の通知に「{}」「{}」を表示、{}秒にも同名を表示",
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
        notices.push((card, event));
    }
    merge(&mut scan.events, notices);
    scan.events.sort_by_key(|e| e.start);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(crop: &str, full: &str) -> Reading {
        let mut r = Reading::default();
        r.add(crop, true);
        r.add(full, false);
        r
    }

    fn event(start: u64, seen: u64, heading: &str) -> Event {
        Event {
            start,
            seen,
            heading: heading.into(),
            basis: String::new(),
            start_image: Some(format!("frames/{start:08}.jpg")),
            evidence_image: format!("frames/{seen:08}.jpg"),
            screen_label: None,
            ocr: String::new(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_silent_card_outside_an_unrelated_observation_is_recovered() -> Result<()> {
        use std::{fs, os::unix::fs::PermissionsExt, process::Command};

        let tmp = tempfile::tempdir()?;
        let video = tmp.path().join("game.mkv");
        let run = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=160x90:r=1:d=22",
                "-vf",
                "drawbox=x=44:y=15:w=50:h=8:color=blue:t=fill:enable='between(t,14,15)'",
                "-c:v",
                "ffv1",
            ])
            .arg(&video)
            .status()?;
        assert!(run.success());
        let adapter = tmp.path().join("read-color");
        fs::write(
            &adapter,
            r#"#!/usr/bin/env python3
import subprocess, sys
pixels = subprocess.check_output(['ffmpeg', '-v', 'error', '-i', 'pipe:0', '-vf', 'scale=48:30',
    '-frames:v', '1', '-pix_fmt', 'rgb24', '-f', 'rawvideo', 'pipe:1'], input=sys.stdin.buffer.read())
if any(b > 150 and b > r + 60 for r, g, b in zip(pixels[::3], pixels[1::3], pixels[2::3])):
    print('クエスト完了\n北の渡し')
"#,
        )?;
        fs::set_permissions(&adapter, fs::Permissions::from_mode(0o755))?;
        fs::create_dir(tmp.path().join("frames"))?;
        let model = crate::model::Model::connect("http://localhost", Some("unused".into()), None)?;
        let analyzer = Analyzer {
            game: "",
            edition: "",
            model: &model,
            dir: tmp.path(),
            use_ocr: true,
            ocr_command: Some(&adapter),
        };
        let mut scan = Scan {
            events: vec![event(0, 0, "動画開始"), event(5, 5, "石碑を発見")],
            cursor_seconds: BTreeSet::new(),
            complete: true,
        };
        supplement(&analyzer, &video, 1, &mut scan)?;
        let card = scan.events.last().unwrap();
        assert_eq!(
            (card.start, card.seen, card.heading.as_str()),
            (14, 15, "クエスト「北の渡し」完了")
        );
        assert!(tmp.path().join(&card.evidence_image).is_file());
        assert_eq!(scan.events[1].heading, "石碑を発見");
        Ok(())
    }

    #[test]
    fn requires_a_title_by_the_status_and_another_source_second() {
        let reads = BTreeMap::from([
            (18, screen("北の灯台", "")),
            (
                19,
                screen("QUEST CLEAR\n北の灯台", "QUEST CLEAR\n西へ向かう"),
            ),
            (
                20,
                screen("クエスト開始\nクエスト追跡", "クエスト開始\n南の街道"),
            ),
            (21, screen("クエスト完了時の報酬\n南の街道", "")),
            (22, screen("クエスト完了\n孤島の謎・3", "")),
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
            (100, screen("クエスト開始\n島の調査・2", "")),
            (101, screen("クエスト開始\n島の調査・2", "")),
            (104, screen("クェスト完了\n島の調査・2", "")),
            (105, screen("島の調査・2", "")),
            (200, screen("クエスト開始\n島の調査・2", "")),
            (201, screen("島の調査・2", "")),
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
    fn a_title_containing_the_phase_word_and_a_close_restart_keep_their_own_proofs() {
        let mut reads = BTreeMap::new();
        for (second, phase) in [
            (14, "開始"),
            (15, "開始"),
            (16, "完了"),
            (17, "完了"),
            (18, "開始"),
            (19, "開始"),
        ] {
            reads.insert(
                second,
                screen(&format!("クエスト{phase}\n任務完了の証"), ""),
            );
        }
        let mut events = vec![
            event(11, 14, "クエスト「任務完了の証」開始"),
            event(18, 19, "任務完了の証を受注"),
        ];
        let notices = cards(&reads)
            .into_iter()
            .map(|c| {
                assert!((c.first..=c.seen).contains(&c.corroborated));
                let e = event(
                    c.first,
                    c.seen,
                    &format!("クエスト「{}」{}", c.title, c.status.phase),
                );
                (c, e)
            })
            .collect();
        merge(&mut events, notices);
        assert_eq!(
            events
                .iter()
                .map(|e| (e.start, e.seen, e.heading.as_str()))
                .collect::<Vec<_>>(),
            [
                (11, 15, "クエスト「任務完了の証」開始"),
                (16, 17, "クエスト「任務完了の証」完了"),
                (18, 19, "クエスト「任務完了の証」開始"),
            ]
        );
    }

    #[test]
    fn the_numbered_card_replaces_its_clipped_or_generic_completion_only() {
        let reads = BTreeMap::from([
            (47, screen("クエスト完了\n島の調査", "")),
            (
                48,
                screen("島の調査\n島の調査・2", "QUEST CLEAR\n島の調査・2"),
            ),
            (49, screen("QUEST CLEAR\n島の調査・2", "")),
        ]);
        let found = cards(&reads);
        assert_eq!(found.len(), 1);
        let card = found.into_iter().next().unwrap();
        assert_eq!(card.title, "島の調査・2");
        assert_eq!(card.header, "QUEST CLEAR");
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
            vec![(card, event(48, 49, "クエスト「島の調査・2」完了"))],
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
