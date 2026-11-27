//! Corroborate a place introduction with the travel objective before it and
//! two neighboring frames showing the destination as a location afterwards.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::sync::LazyLock;

use anyhow::Result;
use image::{ImageFormat, codecs::jpeg::JpegEncoder, imageops::FilterType};
use regex::Regex;

use crate::media::Frame;

pub struct LocationIntro {
    pub start: u64,
    pub seen: u64,
    pub confirmed: u64,
    pub objective_second: u64,
    pub name: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Region {
    Full,
    Objective,
    Title,
    CenterTitle,
}

fn read(frame: &Frame, region: Region, command: Option<&Path>) -> Result<String> {
    if region == Region::Full {
        return crate::ocr::read(&frame.jpeg, command);
    }
    let image = image::load_from_memory_with_format(&frame.jpeg, ImageFormat::Jpeg)?;
    let (w, h) = (image.width(), image.height());
    let (x, y, width, height, zoom, upscale_limit) = match region {
        Region::Objective => (w / 12, h * 3 / 5, w - 2 * (w / 12), h - h * 3 / 5, 2, 1700),
        Region::Title => (0, 0, w * 3 / 5, h / 2, 2, 1600),
        Region::CenterTitle => (w / 4, 0, w / 2, h / 3, 3, 1600),
        Region::Full => unreachable!(),
    };
    if width == 0 || height == 0 {
        return Ok(String::new());
    }
    let new_width = (width * zoom).min(upscale_limit);
    let crop = image.crop_imm(x, y, width, height).resize_exact(
        new_width,
        (height * new_width / width).max(1),
        FilterType::Lanczos3,
    );
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 92).encode_image(&crop.unsharpen(1.0, 2))?;
    crate::ocr::read(&jpeg, command)
}

fn compact(line: &str) -> String {
    line.chars().filter(|c| !c.is_whitespace()).collect()
}

fn menu(text: &str) -> bool {
    [
        "セーブ",
        "ロード",
        "スロット",
        "装備変更",
        "クエスト一覧",
        "目的地を選択",
        "ファストトラベル",
        "ワールドマップ",
        "メニュー",
        "ジャーナル",
    ]
    .iter()
    .any(|s| text.contains(s))
}

fn dialogue(text: &str) -> bool {
    text.contains(['。', '！', '？', '!', '?'])
        || ["オート", "スキップ", "自動送り", "話者：", "会話ログ"]
            .iter()
            .any(|s| text.contains(s))
}

static MOTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:へ|に)(?:向か|行[くけこっ]|進[むめもん]|移動|たどり着|到着)").unwrap()
});
static FLOOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:地下|地上|第|B)?[0-9０-９一二三四五六七八九十]+(?:階|階層|[FＦ])$").unwrap()
});

fn destination(line: &str, clipped: bool) -> Option<String> {
    let line = compact(line);
    let phrase = match MOTION.find(&line) {
        Some(verb) => &line[..verb.start()],
        None if clipped => line.strip_suffix(['へ', 'に'])?,
        None => return None,
    };
    let mut name = phrase.trim_start_matches(['◆', '◇', '▶', '●', '・']);
    for label in ["現在の目標", "次の目標", "移動目標", "目的地", "目標"] {
        if let Some(rest) = name.strip_prefix(label)
            && let Some(rest) = rest.strip_prefix(['：', ':'])
        {
            name = rest;
            break;
        }
    }
    // A quest can describe where the destination is: 「街の外にある銀の森へ」.
    for separator in ["にある", "に建つ", "に位置する"] {
        if let Some((_, rest)) = name.rsplit_once(separator) {
            name = rest;
        }
    }
    name = name.trim_matches(['「', '」', '『', '』']);
    ((2..=32).contains(&name.chars().count())
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '々' | 'ヶ' | 'ー' | '・' | '-'))
        && !["のもと", "の元", "のところ", "のそば"]
            .iter()
            .any(|s| name.ends_with(s)))
    .then(|| name.to_owned())
}

fn destinations(text: &str, clipped: bool) -> BTreeSet<String> {
    text.lines()
        .filter_map(|line| destination(line, clipped))
        .collect()
}

fn explicit_place(line: &str) -> Option<&str> {
    let (label, name) = line.split_once(['：', ':'])?;
    ["現在地", "場所", "エリア", "地名"]
        .contains(&label)
        .then_some(name)
}

fn field(text: &str) -> bool {
    let clean = compact(text);
    !clean.is_empty()
        && !menu(&clean)
        && !dialogue(&clean)
        && (MOTION.is_match(&clean)
            || ["マップ", "ミニマップ", "目的地", "現在地", "HP", "ＭＡＰ"]
                .iter()
                .any(|s| clean.contains(s))
            || text.lines().any(|line| {
                FLOOR.is_match(&compact(line)) || explicit_place(&compact(line)).is_some()
            })
            || text
                .lines()
                .filter(|line| {
                    let v = line.trim();
                    v.len() >= 2 && v.bytes().all(|b| b.is_ascii_digit())
                })
                .count()
                >= 2)
}

fn label(text: &str, names: &BTreeSet<String>) -> Option<String> {
    let labels: BTreeSet<_> = text
        .lines()
        .filter_map(|line| {
            let line = compact(line);
            let name = explicit_place(&line).unwrap_or(&line);
            names.get(name).cloned().or_else(|| {
                names
                    .iter()
                    .find(|place| {
                        name.strip_prefix(place.as_str())
                            .is_some_and(|suffix| FLOOR.is_match(suffix))
                    })
                    .cloned()
            })
        })
        .collect();
    if labels.len() == 1 {
        labels.into_iter().next()
    } else {
        None
    }
}

pub fn identify(
    start: u64,
    until: u64,
    frames: &VecDeque<Frame>,
    words: &BTreeMap<u64, String>,
    ocr_command: Option<&Path>,
    complete: &mut bool,
) -> Option<LocationIntro> {
    identify_with(start, until, frames, words, |f, region| {
        read(f, region, ocr_command)
            .inspect_err(|error| {
                if *complete {
                    eprintln!("現在地の追加 OCR を読み取れません ({error})");
                }
                *complete = false;
            })
            .ok()
    })
}

fn identify_with(
    start: u64,
    until: u64,
    frames: &VecDeque<Frame>,
    words: &BTreeMap<u64, String>,
    mut recognize: impl FnMut(&Frame, Region) -> Option<String>,
) -> Option<LocationIntro> {
    // Start with the most recent travel instruction in an already-read field
    // frame. Ordinary dialogue scenes incur no additional OCR.
    let (&anchor, anchor_text) = words
        .range(start.saturating_sub(12)..=start)
        .rev()
        .find(|(_, text)| field(text) && MOTION.is_match(&compact(text)))?;
    let raw_anchor = destinations(anchor_text, false);
    if raw_anchor.len() != 1 || words.range(anchor..=start).any(|(_, t)| menu(&compact(t))) {
        return None;
    }
    let raw_name = raw_anchor.first()?;
    let mut targets: BTreeMap<String, BTreeSet<u64>> = BTreeMap::new();
    for frame in frames
        .iter()
        .filter(|f| f.second.abs_diff(anchor) <= 1 && f.second <= start)
    {
        let full = words.get(&frame.second).map(String::as_str).unwrap_or("");
        if menu(&compact(full)) || dialogue(&compact(full)) {
            continue;
        }
        let raw = destinations(full, false);
        if raw.len() > 1 {
            return None;
        }
        for name in raw {
            targets.entry(name).or_default().insert(frame.second);
        }
        if let Some(crop) = recognize(frame, Region::Objective) {
            let spellings = destinations(&crop, true);
            if spellings.len() > 1 {
                return None;
            }
            for name in spellings {
                targets.entry(name).or_default().insert(frame.second);
            }
        }
    }
    let names: BTreeSet<String> = targets
        .iter()
        .filter(|(candidate, times)| {
            times.len() >= 2 && times.contains(&anchor) && compatible_reading(candidate, raw_name)
        })
        .map(|(name, _)| name.clone())
        .collect();
    if names.is_empty() {
        return None;
    }
    let end = until.min(start.saturating_add(180));
    if end <= start {
        return None;
    }
    let later = words.range(start + 1..=end);
    let mut probes = Vec::new();
    let mut last_field = None;
    let mut last_probe = None;
    for (&time, text) in later {
        let normalized = compact(text);
        if menu(&normalized) {
            break;
        }
        if !field(text) {
            continue;
        }
        if text
            .lines()
            .map(compact)
            .filter_map(|l| explicit_place(&l).map(str::to_owned))
            .any(|s| !names.contains(&s))
        {
            break;
        }
        let known = label(text, &names).is_some();
        let floor = text.lines().any(|line| FLOOR.is_match(&compact(line)));
        // Prefer exact text and a newly returned HUD. Sample persistent HUDs
        // sparingly rather than cropping every second of a long recording.
        let returned = last_field.is_none_or(|t| time - t > 5);
        if known || floor || returned || last_probe.is_none_or(|t| time - t >= 8) {
            probes.push((!known, !floor, !returned, time));
            last_probe = Some(time);
        }
        last_field = Some(time);
    }
    probes.sort_unstable();
    let mut observed = BTreeMap::new();
    for (_, _, _, time) in probes.into_iter().take(12) {
        let Some(index) = frames.iter().position(|f| f.second == time) else {
            continue;
        };
        let mut check = |f: &Frame| -> Option<String> {
            observed
                .entry(f.second)
                .or_insert_with(|| {
                    let full = match words.get(&f.second) {
                        Some(raw) => raw.clone(),
                        None => recognize(f, Region::Full)?,
                    };
                    let current_goal = destinations(&full, false);
                    // The probe has a field HUD. Its neighbor can still show
                    // the same title when whole-frame OCR reads only a digit.
                    if menu(&compact(&full)) || dialogue(&compact(&full)) {
                        return None;
                    }
                    label(&full, &names)
                        .or_else(|| {
                            let snippet = recognize(f, Region::Title)?;
                            if menu(&compact(&snippet)) || dialogue(&compact(&snippet)) {
                                return None;
                            }
                            label(&snippet, &names).or_else(|| {
                                // A floor label can be easier to read than the adjacent place
                                // name. Magnify the centered title once when the HUD supplies
                                // this specific reason to recheck, even if the name is missing.
                                if !snippet.lines().any(|l| FLOOR.is_match(&compact(l))) {
                                    return None;
                                }
                                let detail = recognize(f, Region::CenterTitle)?;
                                (!menu(&compact(&detail)) && !dialogue(&compact(&detail)))
                                    .then(|| label(&detail, &names))
                                    .flatten()
                            })
                        })
                        .filter(|name| !current_goal.contains(name))
                })
                .clone()
        };
        let Some(name) = check(&frames[index]) else {
            continue;
        };
        for near in [index.checked_sub(1), index.checked_add(1)]
            .into_iter()
            .flatten()
            .filter_map(|i| frames.get(i))
        {
            if near.second > start
                && near.second <= end
                && near.second.abs_diff(time) <= 3
                && check(near).as_deref() == Some(&name)
            {
                return Some(LocationIntro {
                    start,
                    seen: time.min(near.second),
                    confirmed: time.max(near.second),
                    objective_second: anchor,
                    name,
                });
            }
        }
    }
    None
}

// A crop may resolve one OCR glyph. A different direction or numbered
// destination is meaningful even when the remainder of its name is identical.
fn compatible_reading(candidate: &str, raw: &str) -> bool {
    if candidate == raw {
        return true;
    }
    let length = raw.chars().count();
    if length < 3 || candidate.chars().count() != length {
        return false;
    }
    let mut differences = candidate.chars().zip(raw.chars()).filter(|(a, b)| a != b);
    let Some((a, b)) = differences.next() else {
        return true;
    };
    let distinct =
        |c: char| c.is_numeric() || "東西南北上下左右前後一二三四五六七八九十".contains(c);
    !distinct(a) && !distinct(b) && differences.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames() -> VecDeque<Frame> {
        (0..160)
            .map(|second| Frame {
                second,
                jpeg: Vec::new(),
            })
            .collect()
    }

    fn words(entries: &[(u64, &str)]) -> BTreeMap<u64, String> {
        entries
            .iter()
            .map(|(s, text)| (*s, (*text).to_owned()))
            .collect()
    }

    #[test]
    fn links_two_independent_location_labels_to_a_preceding_travel_objective() {
        let screens = words(&[
            (16, "マップ"),
            (17, "高原にある霞の水問へ向かう"),
            (23, "スキップ"),
            (25, "商人\n昨日の話をしよう。"),
            (51, "マップ\n次の目標：隊長のもとへ向かう"),
            (52, "120"),
        ]);
        let found = identify_with(20, 100, &frames(), &screens, |frame, region| {
            match (frame.second, region) {
                (16 | 17, Region::Objective) => Some("高原にある霞の水門へ".into()),
                (51, Region::Title) => Some("地下2階".into()),
                (51, Region::CenterTitle) => Some("霞の水門地下2階".into()),
                (52, Region::Title) => Some("霞の水門\n地下2階".into()),
                _ => None,
            }
        })
        .unwrap();
        assert_eq!(found.name, "霞の水門");
        assert_eq!(
            (
                found.start,
                found.objective_second,
                found.seen,
                found.confirmed
            ),
            (20, 17, 51, 52)
        );
    }

    #[test]
    fn dialogue_menu_another_destination_and_a_single_frame_cannot_establish_a_place() {
        let objective = "目標：夕凪港へ向かう";
        let label = "現在地：夕凪港";
        for (a, b, c) in [
            ("オート\n夕凪港\n……。", "スキップ\n夕凪港", ""),
            ("ワールドマップ\n現在地：夕凪港", label, label),
            ("現在地：昼凪港", label, label),
            ("マップ\n夕凪港へ向かう", "マップ\n夕凪港へ向かう", ""),
            (label, "HP 100/100", ""),
        ] {
            let screens = words(&[(13, objective), (14, objective), (30, a), (31, b), (32, c)]);
            assert!(
                identify_with(18, 90, &frames(), &screens, |f, region| {
                    (region == Region::Title
                        && screens.get(&f.second).is_some_and(|v| v.contains("夕凪港")))
                    .then(|| "夕凪港".to_owned())
                })
                .is_none(),
                "accepted {a}, {b}, {c}"
            );
        }
    }

    #[test]
    fn ignores_unrelated_tracked_goals_and_ambiguous_future_areas() {
        let screens = words(&[
            (11, "目標：東の砦へ向かう"),
            (12, "目標：東の砦へ向かう"),
            (21, "現在地：西の砦"),
            (22, "現在地：西の砦"),
        ]);
        assert!(
            identify_with(15, 90, &frames(), &screens, |_, region| (region
                == Region::Objective)
                .then(|| "西の砦へ".into()))
            .is_none()
        );
        let ambiguous = words(&[(12, "目標：夕凪港へ向かう\n目標：東の砦へ向かう")]);
        assert!(
            identify_with(15, 90, &frames(), &ambiguous, |_, _| panic!(
                "ambiguous goals"
            ))
            .is_none()
        );
    }

    #[test]
    fn skips_ocr_without_a_travel_objective_and_bounds_optional_image_work() {
        let talk = words(&[(11, "オート\n霧の森へ向かう。"), (12, "マップ")]);
        assert!(identify_with(15, 140, &frames(), &talk, |_, _| panic!("dialogue")).is_none());
        let mut screens = words(&[(9, "目的地：霧の森へ向かう"), (10, "霧の森へ向かう")]);
        screens.extend((25..150).map(|s| (s, "マップ".to_owned())));
        let mut reads = 0;
        assert!(
            identify_with(15, 140, &frames(), &screens, |_, _| {
                reads += 1;
                None
            })
            .is_none()
        );
        assert!(reads <= 15, "{reads} OCR calls");
    }

    #[test]
    fn failed_region_read_marks_the_scan_incomplete_without_stopping_it() -> Result<()> {
        let mut complete = true;
        let screens = words(&[(9, "目的地：霧の森へ向かう"), (10, "霧の森へ向かう")]);
        let mut jpeg = Vec::new();
        JpegEncoder::new(&mut jpeg).encode_image(&image::RgbImage::new(32, 24))?;
        let frames = (0..26)
            .map(|second| Frame {
                second,
                jpeg: jpeg.clone(),
            })
            .collect();
        let dir = tempfile::tempdir()?;
        let missing = dir.path().join("unavailable-ocr");
        assert!(identify(15, 25, &frames, &screens, Some(&missing), &mut complete).is_none());
        assert!(!complete);

        let mut complete = true;
        assert!(
            identify(
                15,
                25,
                &frames,
                &BTreeMap::new(),
                Some(&missing),
                &mut complete
            )
            .is_none()
        );
        assert!(complete);
        Ok(())
    }
}
