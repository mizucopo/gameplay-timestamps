//! Recheck the tracker's proposed starts after loading observations. Its overview
//! sees reduced panels and can mistake a speaker's name and scenery for a HUD.
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;
use image::{DynamicImage, ImageFormat, Rgb, RgbImage, codecs::jpeg::JpegEncoder};
use serde::Deserialize;

use super::{compact, valid_name};
use crate::analyze::{Analyzer, Event, save};
use crate::media::Frame;
use crate::speech::frames::sample;

#[derive(Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Role {
    Hud,
    Title,
    Entrance,
    Other,
}

#[derive(Clone, Deserialize, schemars::JsonSchema)]
struct Reading {
    role: Role,
    literal: String,
    #[schemars(range(max = 100))]
    gauge_x: u8,
    #[schemars(range(max = 100))]
    gauge_y: u8,
}

const PROMPT: &str = "この原寸のゲーム画面だけを観察する。hud は実際の対戦相手の名札と、その名札に付いた現在体力の細長いゲージが同時に見える場合。title は対決相手の称号・名前だけを大きく単独で紹介する演出。entrance は戦場に切り替わり相手を見せる対決の導入。話者の名前と台詞枠、目標・メニュー・操作説明、台詞枠の装飾や背景の水平線は other。literal は hud/title の敵名欄に書かれた称号込みの原文だけ、それ以外は空文字。gauge_x, gauge_y は実際の敵HPゲージ中央を画面左上から数えた整数百分率 (0〜100)、hud 以外は両方0。推測で補わない。";

#[derive(Clone, Copy, Debug, PartialEq)]
struct Meter {
    left: u32,
    right: u32,
    y: u32,
    color: Rgb<u8>,
}

const WIDTH: u32 = 384;
const HEIGHT: u32 = 216;

fn difference(a: &Rgb<u8>, b: &Rgb<u8>) -> u8 {
    (0..3).map(|i| a[i].abs_diff(b[i])).max().unwrap()
}

impl Meter {
    fn overlaps(self, other: Self) -> bool {
        let common = self
            .right
            .min(other.right)
            .saturating_sub(self.left.max(other.left));
        self.y.abs_diff(other.y) <= 2 && common >= WIDTH / 8
    }

    fn aligned(self, other: Self) -> bool {
        let common = self
            .right
            .min(other.right)
            .saturating_sub(self.left.max(other.left));
        self.y.abs_diff(other.y) <= 1
            && (self.left.abs_diff(other.left) <= 3 || self.right.abs_diff(other.right) <= 3)
            && common * 5 >= (self.right - self.left).min(other.right - other.left) * 4
            && difference(&self.color, &other.color) <= 36
    }

    fn near(self, reading: &Reading) -> bool {
        let x = u32::from(reading.gauge_x) * WIDTH / 100;
        let y = u32::from(reading.gauge_y) * HEIGHT / 100;
        reading.gauge_x <= 100
            && reading.gauge_y <= 100
            && self.left.saturating_sub(WIDTH / 12) <= x
            && x <= self.right + WIDTH / 12
            && self.y.abs_diff(y) <= HEIGHT / 14
    }
}

// A candidate rail has a continuous fill or empty track and contrast on both
// sides of that track. Its colour and screen quadrant can change between games.
fn meters(image: &RgbImage, located: bool) -> Vec<Meter> {
    let tolerance = if located { 40 } else { 22 };
    let mut out = Vec::new();
    for y in 3..HEIGHT - 3 {
        let mut x = 1;
        while x < WIDTH - 1 {
            let left = x;
            let first = image.get_pixel(x, y);
            // A segmented meter can divide its fill with narrow gaps.
            let mut last_match = x;
            let mut close = 1;
            x += 1;
            while x < WIDTH - 1 && x - last_match <= 5 {
                if difference(first, image.get_pixel(x, y)) < tolerance {
                    last_match = x;
                    close += 1;
                }
                x += 1;
            }
            x = last_match + 1;
            if x - left < WIDTH / 7 || (!located && close * 5 < (x - left) * 4) {
                continue;
            }
            let edge = |row| {
                let count = (left + 2..x - 2)
                    .filter(|&i| difference(image.get_pixel(i, row), image.get_pixel(i, y)) > 30)
                    .count() as u32;
                count * 3 >= (x - left - 4) * 2
            };
            if (1..=6.min(y)).any(|d| edge(y - d))
                && (1..=6.min(HEIGHT - 1 - y)).any(|d| edge(y + d))
            {
                out.push(Meter {
                    left,
                    right: x,
                    y,
                    color: *image.get_pixel((left + x) / 2, y),
                });
            }
        }
    }
    out
}

fn thumbnail(image: &DynamicImage) -> RgbImage {
    image.thumbnail_exact(WIDTH, HEIGHT).into_rgb8()
}

fn same_name(line: &str, name: &str) -> bool {
    let line = compact(line);
    let literal = line
        .strip_prefix("ボス：")
        .or_else(|| line.strip_prefix("BOSS:"))
        .unwrap_or(&line);
    compact(literal.trim_matches(['「', '」', '『', '』'])) == compact(name)
}

// An uncertain glyph can be resolved only by an exact native OCR reading
// localized to the meter. Numbers, directions and form colours are identities.
fn plausible_transcription(read: &str, name: &str) -> bool {
    let a = compact(read);
    let b = compact(name);
    if a.is_empty() || same_name(&a, &b) {
        return true;
    }
    if a.chars().count() < 6 || a.chars().count() != b.chars().count() {
        return false;
    }
    let mut different = a.chars().zip(b.chars()).filter(|(x, y)| x != y);
    let structural = |c: char| {
        c.is_numeric()
            || "東西南北上下左右前後真新改赤青白黒金銀炎氷雷闇光一二三四五六七八九十".contains(c)
    };
    different
        .next()
        .is_some_and(|(x, y)| !structural(x) && !structural(y))
        && different.next().is_none()
}

fn jpeg(image: &RgbImage) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    JpegEncoder::new_with_quality(&mut bytes, 94).encode_image(image)?;
    Ok(bytes)
}

fn name_near(
    image: &DynamicImage,
    bar: Meter,
    analyzer: &Analyzer<'_>,
    name: &str,
    full_text: &str,
    margin: u32,
) -> Result<bool> {
    let left = bar.left.saturating_sub(WIDTH / 12) * image.width() / WIDTH;
    let right = (bar.right + WIDTH / 12).min(WIDTH) * image.width() / WIDTH;
    let top = bar.y.saturating_sub(margin) * image.height() / HEIGHT;
    let bottom = (bar.y + margin).min(HEIGHT) * image.height() / HEIGHT;
    let has_name = |ocr: &str| ocr.lines().any(|s| same_name(s, name));
    if has_name(full_text) {
        // A narrow crop changes the OCR engine's scale and can lose a rare
        // character. Keep the original canvas and erase only the meter and
        // its adjacent name: an intact speaker or objective remains readable.
        let mut outside = image.to_rgb8();
        for y in top..bottom {
            for x in left..right {
                outside.put_pixel(x, y, Rgb([32; 3]));
            }
        }
        if !has_name(&crate::ocr::read(&jpeg(&outside)?, analyzer.ocr_command)?) {
            return Ok(true);
        }
    }
    let crop = image
        .crop_imm(left, top, (right - left).max(1), (bottom - top).max(1))
        .to_rgb8();
    Ok(has_name(&crate::ocr::read(
        &jpeg(&crop)?,
        analyzer.ocr_command,
    )?))
}

struct Witness<'a> {
    frame: &'a Frame,
    image: DynamicImage,
    bars: Vec<Meter>,
    located: Vec<Meter>,
    text: String,
    read: Option<Reading>,
}

fn witness<'a>(
    frame: &'a Frame,
    read: Option<&Reading>,
    name: &str,
    analyzer: &Analyzer<'_>,
) -> Result<Option<Witness<'a>>> {
    if read.is_some_and(|r| r.role == Role::Hud && !plausible_transcription(&r.literal, name)) {
        return Ok(None);
    }
    if !analyzer.use_ocr
        && !read.is_some_and(|r| r.role == Role::Hud && same_name(&r.literal, name))
    {
        return Ok(None);
    }
    let image = image::load_from_memory_with_format(&frame.jpeg, ImageFormat::Jpeg)?;
    let thumb = thumbnail(&image);
    let mut bars = if analyzer.use_ocr {
        meters(&thumb, false)
    } else {
        Vec::new()
    };
    let located =
        if let Some(r) = read.filter(|r| r.role == Role::Hud && same_name(&r.literal, name)) {
            meters(&thumb, true)
                .into_iter()
                .filter(|m| m.near(r))
                .collect()
        } else {
            Vec::new()
        };
    if bars.is_empty() && located.is_empty() {
        return Ok(None);
    }
    let mut text = String::new();
    if analyzer.use_ocr {
        let Ok(full) = crate::ocr::read(&frame.jpeg, analyzer.ocr_command) else {
            return Ok(None);
        };
        if !full.lines().any(|s| same_name(s, name))
            && !read.is_some_and(|r| r.role == Role::Hud && same_name(&r.literal, name))
        {
            return Ok(None);
        }
        // The control and the occluded picture must use the same JPEG encoding.
        text = crate::ocr::read(&jpeg(&image.to_rgb8())?, analyzer.ocr_command).unwrap_or_default();
    }
    bars.sort_by_key(|b| {
        (
            !read.is_some_and(|r| b.near(r)),
            std::cmp::Reverse(b.right - b.left),
        )
    });
    Ok(Some(Witness {
        frame,
        image,
        bars,
        located,
        text,
        read: read.cloned(),
    }))
}

// An ornamental rule under the speaker and a real HP bar can satisfy
// exactly the same pixel and OCR checks. Examine the role of each original
// picture separately, without disclosing the proposed enemy.
fn opponent_on_screen(witness: &Witness<'_>, name: &str, analyzer: &Analyzer<'_>) -> bool {
    let read = witness.read.clone().or_else(|| {
        analyzer
            .model
            .infer::<Reading>(PROMPT, &witness.frame.jpeg)
            .ok()
    });
    if let Some(r) = &read
        && r.role == Role::Hud
    {
        return plausible_transcription(&r.literal, name);
    }
    let entrance = read.is_some_and(|r| r.role == Role::Entrance);
    match analyzer
        .model
        .infer::<super::Picture>(super::PROMPT, &witness.frame.jpeg)
    {
        Ok(p) => match p.subject {
            super::Subject::BossHud | super::Subject::EnemyHud | super::Subject::Combat => {
                plausible_transcription(p.literal(), name)
            }
            // An unclassified image cannot corroborate a role of 'other'.
            // A plausible introduction still has to pass the strict paired
            // geometry and native name-location checks above.
            super::Subject::Other => entrance,
            _ => false,
        },
        Err(_) => false,
    }
}

fn shared_meter(
    a: &Witness<'_>,
    b: &Witness<'_>,
    name: &str,
    analyzer: &Analyzer<'_>,
) -> Result<Option<Meter>> {
    // Agreement of two blind native readings supplies an independent
    // location. It can corroborate a divided gauge or a label just outside
    // the narrow strip used for unguided image search.
    for &first in &a.located {
        for &second in b.located.iter().filter(|m| first.overlaps(**m)) {
            if !analyzer.use_ocr
                || (name_near(&a.image, first, analyzer, name, &a.text, HEIGHT / 14)
                    .unwrap_or(false)
                    && name_near(&b.image, second, analyzer, name, &b.text, HEIGHT / 14)
                        .unwrap_or(false))
            {
                return Ok(Some(first));
            }
        }
    }
    let matches = a
        .bars
        .iter()
        .filter_map(|x| {
            b.bars
                .iter()
                .filter(|y| x.aligned(**y))
                .min_by_key(|y| x.y.abs_diff(y.y))
                .map(|y| (*x, *y))
        })
        .collect::<Vec<_>>();
    let mut checked: Vec<Meter> = Vec::new();
    for &(first, second) in &matches {
        // A lone horizontal edge in scenery can coincide with a nearby
        // speaker. Require a stable band of neighbouring filled scanlines.
        if analyzer.use_ocr
            && !matches.iter().any(|(x, y)| {
                first.y != x.y && second.y != y.y && first.aligned(*x) && second.aligned(*y)
            })
        {
            continue;
        }
        if checked.iter().any(|m| {
            m.y == first.y && m.left.abs_diff(first.left) <= 3 && m.right.abs_diff(first.right) <= 3
        }) {
            continue;
        }
        checked.push(first);
        if !analyzer.use_ocr
            || (name_near(&a.image, first, analyzer, name, &a.text, HEIGHT / 25).unwrap_or(false)
                && name_near(&b.image, second, analyzer, name, &b.text, HEIGHT / 25)
                    .unwrap_or(false))
        {
            return Ok(Some(first));
        }
    }
    Ok(None)
}

fn standalone_title(frame: &Frame, name: &str, analyzer: &Analyzer<'_>) -> Result<bool> {
    let source = crate::ocr::read(&frame.jpeg, analyzer.ocr_command)?;
    let mut words = source
        .lines()
        .filter(|s| s.chars().any(char::is_alphanumeric));
    if !words.next().is_some_and(|s| same_name(s, name)) || words.next().is_some() {
        return Ok(false);
    }
    let image = image::load_from_memory_with_format(&frame.jpeg, ImageFormat::Jpeg)?.to_rgb8();
    // An isolated large title straddles both sides of the centre, which a
    // short speaker label near a corner or beside the portrait does not.
    for pct in [44, 56] {
        let mut masked = image.clone();
        for y in 0..image.height() {
            for x in image.width() * (pct - 1) / 100..image.width() * (pct + 1) / 100 {
                masked.put_pixel(x, y, Rgb([32; 3]));
            }
        }
        if crate::ocr::read(&jpeg(&masked)?, analyzer.ocr_command)?
            .lines()
            .any(|s| same_name(s, name))
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn candidate(event: &Event) -> Option<&str> {
    let title = event.heading.strip_suffix("戦開始")?;
    event.screen_label.as_deref().filter(|name| {
        valid_name(name)
            && (same_name(title, name) || event.basis.contains("秒の敵名・体力ゲージを照合"))
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HistoryScreen {
    Other,
    Result,
    LoadQuestion,
    Loading,
}

fn history_screen(ocr: String) -> HistoryScreen {
    let words = compact(&ocr);
    if words.contains("オートセーブから再開") && words.contains("タイトルへ") {
        return HistoryScreen::Result;
    }
    if words.contains("このデータから再開しますか") {
        return HistoryScreen::LoadQuestion;
    }
    if ocr.lines().any(|l| {
        [
            "データ読み込み中",
            "データを読み込んでいます",
            "データをロードしています",
            "セーブデータをロード中",
        ]
        .iter()
        .any(|line| compact(l).contains(line))
    }) {
        return HistoryScreen::Loading;
    }
    // A completed escape ends the encounter just as a victory or defeat
    // does. Ignore escape options and quotes in a dialogue or records menu.
    if ![
        "会話ログ",
        "戦績",
        "履歴",
        "図鑑",
        "スキップ",
        "オート",
        "「",
        "」",
    ]
    .iter()
    .any(|s| words.contains(s))
        && ocr.lines().any(|s| {
            matches!(
                compact(s)
                    .trim_end_matches(['。', '！', '!'])
                    .to_ascii_uppercase()
                    .as_str(),
                "逃走成功" | "逃走に成功した" | "撤退成功" | "ESCAPED" | "ESCAPESUCCESSFUL"
            )
        })
    {
        return HistoryScreen::Result;
    }
    let event = Event {
        start: 0,
        seen: 0,
        heading: String::new(),
        basis: String::new(),
        ocr,
        start_image: None,
        evidence_image: String::new(),
        screen_label: None,
    };
    if super::battle_boundary(&event) {
        HistoryScreen::Result
    } else {
        HistoryScreen::Other
    }
}

pub(super) fn recorded_boundary(event: &Event) -> bool {
    crate::editor::resumes_play(event)
        || (!crate::editor::conversation(event)
            && history_screen(crate::dictionary::Evidence::new(event.seen, &event.ocr, "").ocr)
                == HistoryScreen::Result)
}

fn restart_since(
    video: &Path,
    previous: u64,
    start: u64,
    analyzer: &Analyzer<'_>,
    history: &mut BTreeMap<u64, HistoryScreen>,
) -> Result<bool> {
    let lower = previous.saturating_add(1);
    if lower >= start {
        return Ok(false);
    }
    let confirms = |time: u64, screen: HistoryScreen, known: &BTreeMap<u64, HistoryScreen>| {
        screen == HistoryScreen::Result
            || (screen == HistoryScreen::Loading
                && time > lower
                && known.get(&(time - 1)) == Some(&HistoryScreen::LoadQuestion))
            || (screen == HistoryScreen::LoadQuestion
                && time.saturating_add(1) < start
                && known.get(&(time + 1)) == Some(&HistoryScreen::Loading))
    };
    if history
        .range(lower..start)
        .any(|(&t, &v)| confirms(t, v, history))
    {
        return Ok(true);
    }
    // Read a bounded batch, but cover the entire interval since the last
    // witnessed HUD. Results can overlay a still-visible enemy health bar.
    let mut end = start;
    let mut error = None;
    while end > lower {
        let from = end.saturating_sub(45).max(lower);
        let seconds = (from..end)
            .filter(|s| !history.contains_key(s))
            .collect::<Vec<_>>();
        for (t, frame) in sample(video, seconds)?.into_iter().rev() {
            let result = if analyzer.use_ocr {
                crate::ocr::read(&frame.jpeg, analyzer.ocr_command).map(history_screen)
            } else {
                analyzer
                    .model
                    .infer::<super::Picture>(super::PROMPT, &frame.jpeg)
                    .map(|read| {
                        if read.subject == super::Subject::Result {
                            HistoryScreen::Result
                        } else {
                            HistoryScreen::Other
                        }
                    })
            };
            match result {
                Ok(v) => {
                    history.insert(t, v);
                    if confirms(t, v, history) {
                        return Ok(true);
                    }
                }
                Err(e) => error = Some(e),
            }
        }
        end = from;
    }
    error.map_or(Ok(false), Err)
}

fn unrelated_opening(text: &str) -> bool {
    let words = compact(text);
    crate::map::cursor_control(text)
        || crate::quest::is_offer_screen(text)
        || history_screen(text.into()) != HistoryScreen::Other
        || [
            "所持品",
            "装備一覧",
            "装備選択",
            "装備を選択",
            "図鑑",
            "ステータス画面",
            "会話ログ",
            "会話を進める",
            "オート送り",
            "ロード画面",
            "セーブデータ",
            "スキップ",
        ]
        .iter()
        .any(|s| words.contains(s))
        || (words.contains("装備") && (words.contains("選択") || words.contains("閉じる")))
        || text.lines().any(|line| {
            line.split_once('「').is_some_and(|(speaker, quote)| {
                valid_name(speaker.trim())
                    && quote
                        .trim_end()
                        .strip_suffix('」')
                        .is_some_and(|speech| speech.ends_with(['。', '！', '？', '!', '?']))
            })
        })
}

// A title and a field introduction must both survive the original UI check.
// A battle banner may be punctuated just like speech. Use visible controls
// or a speaker with quoted dialogue to identify an unrelated screen.
fn opening_screen(frame: &Frame, analyzer: &Analyzer<'_>) -> Result<bool> {
    if !analyzer.use_ocr {
        return Ok(true);
    }
    Ok(!unrelated_opening(&crate::ocr::read(
        &frame.jpeg,
        analyzer.ocr_command,
    )?))
}

fn opening(
    frame: &Frame,
    before: Option<&Frame>,
    name: &str,
    analyzer: &Analyzer<'_>,
) -> Result<bool> {
    let Ok(read) = analyzer.model.infer::<Reading>(PROMPT, &frame.jpeg) else {
        return Ok(false);
    };
    if !matches!(read.role, Role::Title | Role::Entrance)
        || (read.role == Role::Title && !same_name(&read.literal, name))
        || !opening_screen(frame, analyzer)?
    {
        return Ok(false);
    }
    if read.role == Role::Title || !analyzer.use_ocr {
        return Ok(true);
    }
    let Some(before) = before else {
        return Ok(false);
    };
    let previous = crate::scene::Signal::measure(before, None)?;
    let field = crate::scene::Signal::measure(frame, Some(&previous))?;
    Ok((previous.dark || previous.mean < 20) && field.mean > previous.mean + 24)
}

fn confirm(
    event: &mut Event,
    name: &str,
    analyzer: &Analyzer<'_>,
    video: &Path,
    previous: &mut Option<u64>,
    breaks: &BTreeSet<u64>,
    history: &mut BTreeMap<u64, HistoryScreen>,
) -> Result<Option<Meter>> {
    let mut seconds = vec![event.start.saturating_sub(1), event.start, event.seen];
    // The observation retains the original OCR witness seconds, which may
    // be farther apart than two adjacent VFR playback samples.
    for line in event.ocr.lines() {
        if let Some((second, raw)) = line.split_once(' ')
            && let Ok(second) = second.parse::<u64>()
            && event.seen < second
            && second - event.seen <= 8
            && same_name(raw, name)
        {
            seconds.push(second);
        }
    }
    for delta in 1..=2 {
        seconds.push(event.seen.saturating_add(delta));
    }
    let mut frames = sample(video, seconds)?;
    let Some(first) = frames.get(&event.seen) else {
        return Ok(None);
    };
    let first_reading = analyzer.model.infer::<Reading>(PROMPT, &first.jpeg).ok();
    let first_role = first_reading.as_ref().map(|r| r.role);
    let named_title = if first_role == Some(Role::Title) {
        first_reading
            .as_ref()
            .is_some_and(|r| same_name(&r.literal, name))
    } else {
        analyzer.use_ocr
            && !matches!(first_role, Some(Role::Hud | Role::Other))
            && standalone_title(first, name, analyzer).unwrap_or(false)
    };
    let title = named_title && opening_screen(first, analyzer).unwrap_or(false);
    let mut onset = event.seen;
    let mut intro = title;
    if event.start < event.seen
        && let Some(start) = frames.get(&event.start)
        && opening(
            start,
            frames.get(&event.start.saturating_sub(1)),
            name,
            analyzer,
        )
        .unwrap_or(false)
    {
        onset = event.start;
        intro = true;
    }
    if title {
        // The title card has no HP. Wait through a short intro or tutorial
        // and record the first independently confirmed HUD as the proof.
        let end = event.seen.saturating_add(18);
        let limit = breaks
            .range(event.seen.saturating_add(1)..=end)
            .next()
            .copied()
            .unwrap_or(end.saturating_add(1));
        frames.extend(sample(
            video,
            (event.seen.saturating_add(1)..limit).collect(),
        )?);
    }
    let mut prior: Option<Witness<'_>> = None;
    let mut initial = first_reading;
    for (&second, frame) in frames.range(event.seen..) {
        if prior.as_ref().is_some_and(|a| a.frame.jpeg == frame.jpeg) {
            continue;
        }
        let reading = if second == event.seen {
            initial.take()
        } else if !analyzer.use_ocr || matches!(first_role, Some(Role::Hud | Role::Title)) {
            analyzer.model.infer::<Reading>(PROMPT, &frame.jpeg).ok()
        } else {
            None
        };
        let Some(current) = witness(frame, reading.as_ref(), name, analyzer)? else {
            continue;
        };
        if let Some(earlier) = &prior
            && second - earlier.frame.second <= 8
            && breaks
                .range(earlier.frame.second.saturating_add(1)..=second)
                .next()
                .is_none()
            && let Some(shared) = shared_meter(earlier, &current, name, analyzer)?
            && opponent_on_screen(earlier, name, analyzer)
            && opponent_on_screen(&current, name, analyzer)
            && !restart_since(video, earlier.frame.second, second, analyzer, history)?
        {
            let a = earlier.frame;
            // A HUD can leave the enemy name as the only readable text. It
            // supplies a lead-in only when a separate picture showed the
            // introduction before the first corroborated health meter.
            if title && onset == event.seen && a.second == onset {
                intro = false;
            }
            if !intro
                || breaks.range(onset..a.second).next().is_some()
                || restart_since(video, onset, a.second, analyzer, history)?
            {
                onset = a.second;
                intro = false;
            }
            if let Some(last) = *previous {
                if onset <= last {
                    return Ok(None);
                }
                if !intro
                    && breaks
                        .range(last.saturating_add(1)..=onset)
                        .next()
                        .is_none()
                    && !restart_since(video, last, onset, analyzer, history)?
                {
                    *previous = Some(second);
                    return Ok(None);
                }
            }
            *previous = Some(second);
            event.start = onset;
            event.seen = a.second;
            event.start_image = Some(save(analyzer.dir, &frames[&onset])?);
            event.evidence_image = save(analyzer.dir, a)?;
            event.basis = format!(
                "{onset}秒から対決が始まり、{}秒と{second}秒の異なる元画像で敵名「{name}」と隣接した体力ゲージを確認",
                a.second
            );
            for t in [a.second, second] {
                if !event.ocr.lines().any(|s| {
                    s.split_once(' ').is_some_and(|(prefix, raw)| {
                        prefix.parse::<u64>() == Ok(t) && same_name(raw, name)
                    })
                }) {
                    event.ocr.push_str(&format!("\n{t} {name}"));
                }
            }
            return Ok(Some(shared));
        }
        prior = Some(current);
    }
    Ok(None)
}

pub(super) fn verify(
    events: &mut Vec<Event>,
    analyzer: &Analyzer<'_>,
    video: &Path,
    breaks: &BTreeSet<u64>,
) {
    let mut last = BTreeMap::new();
    let mut history = BTreeMap::new();
    let mut occupied = BTreeMap::<String, Meter>::new();
    events.sort_by_key(|e| e.start);
    events.retain_mut(|event| {
        let Some(name) = candidate(event).map(str::to_owned) else {
            return true;
        };
        match confirm(
            event,
            &name,
            analyzer,
            video,
            last.entry(name.clone()).or_default(),
            breaks,
            &mut history,
        ) {
            Ok(Some(meter)) => {
                // A newly established encounter can replace the occupant of
                // the same HUD slot. Simultaneous named opponents stay active.
                let text = crate::dictionary::Evidence::new(event.seen, &event.ocr, "").ocr;
                last.retain(|other, seen| {
                    other == &name
                        || !seen.is_some_and(|s| s < event.start)
                        || !occupied.get(other).is_some_and(|b| b.overlaps(meter))
                        || text.lines().any(|s| same_name(s, other))
                });
                occupied.insert(name, meter);
                true
            }
            Ok(None) => false,
            Err(error) => {
                eprintln!("{} 秒の戦闘開始を再確認できません ({error})", event.seen);
                false
            }
        }
    });
    events.sort_by_key(|e| e.start);
}
