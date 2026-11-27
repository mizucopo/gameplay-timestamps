//! A tracked HUD objective can change while the scenery and the active quest
//! remain the same. Read the panel independently of the model's scene choices.
use std::borrow::Cow;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::sync::LazyLock;

use anyhow::Result;
use image::{ImageFormat, codecs::jpeg::JpegEncoder, imageops::FilterType};
use regex::Regex;

use crate::{analyze::Event, media::Frame};

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct Goal {
    title: String,
    action: String,
    slot: Option<&'static str>,
}

#[derive(Ord, PartialOrd, Eq, PartialEq)]
enum Topic {
    Quest(String),
    Active(&'static str),
    Independent(String),
}

impl Goal {
    fn topic(&self) -> Topic {
        if !self.title.is_empty() {
            Topic::Quest(self.title.clone())
        } else if let Some(slot) = self.slot {
            Topic::Active(slot)
        } else {
            Topic::Independent(self.action.clone())
        }
    }

    fn replaced_by(&self, next: &Goal) -> bool {
        self.action != next.action
            && ((!self.title.is_empty() && self.title == next.title)
                || (self.slot.is_some() && self.slot == next.slot))
    }

    fn heading(&self) -> String {
        if self.title.is_empty() {
            format!("目標「{}」", self.action)
        } else {
            format!("{}（{}）", self.title, self.action)
        }
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
struct Reading {
    titles: BTreeSet<String>,
    actions: BTreeSet<String>,
    explicit: bool,
    slots: BTreeSet<&'static str>,
}

static ACTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"^([\p{Han}\p{Hiragana}\p{Katakana}ー・々A-Za-zＡ-Ｚａ-ｚ0-9０-９]{1,36}",
        r"(?:へ|に|を|で|と|まで)(?:向かう|行く|進む|戻る|入る|渡る|目指す|探す|調べる|",
        r"集める|届ける|渡す|手に入れる|倒す|守る|救う|開ける|会う|話す|話しかける|",
        r"見つける|追う|聞く|直す|(?:調査|探索|捜索|発見|入手|納品|報告|討伐|撃破|救助|解放|到達|合流|回収|確認)する))"
    )).unwrap()
});

fn interface(text: &str) -> bool {
    crate::map::cursor_control(text)
        || [
            "セーブスロット",
            "ロードスロット",
            "クエスト一覧",
            "ワールドマップ",
            "ファストトラベル",
            "目的地を選択",
            "装備変更",
            "会話ログ",
            "自動送り",
            "スキップ",
            "オート再生",
        ]
        .iter()
        .any(|flag| text.contains(flag))
}

fn icon_tail(text: &str) -> bool {
    // A controller button or diamond immediately after a HUD label is commonly
    // recognized as a single glyph. Do not strip further words from a sentence.
    let mut letters = text.chars().filter(|c| c.is_alphanumeric());
    matches!(letters.next(), None | Some('く' | 'ロ' | '口'))
        && matches!(letters.next(), None | Some('ロ' | '口'))
        && letters.next().is_none()
}

fn read_lines(text: &str) -> Reading {
    let mut read = Reading::default();
    if interface(text) {
        return read;
    }
    for raw in text.lines() {
        let joined: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
        let line = joined.trim_start_matches(|c: char| !c.is_alphanumeric());
        let mut value = line;
        let mut marker_name = None;
        for marker in [
            "現在の目標",
            "次の目標",
            "探索目標",
            "サブ目標",
            "副目標",
            "目標",
            "目的地",
            "任務",
            "クエスト",
        ] {
            if let Some(rest) = line.strip_prefix(marker) {
                if !rest.is_empty() && !rest.starts_with(['：', ':']) {
                    break;
                }
                read.explicit = true;
                if matches!(marker, "現在の目標" | "次の目標" | "目的地") {
                    read.slots.insert(marker);
                }
                if let Some(rest) = rest.strip_prefix(['：', ':']) {
                    value = rest;
                    marker_name = Some(marker);
                }
                break;
            }
        }
        if let Some(found) = ACTION.captures(value) {
            let action = found.get(1).unwrap();
            if icon_tail(&value[action.end()..])
                && !["マップを", "メニューを", "ボタンを", "カメラを"]
                    .iter()
                    .any(|word| action.as_str().contains(word))
            {
                read.actions.insert(action.as_str().to_owned());
            }
        } else if let Some((place, tail)) = value.rsplit_once('へ')
            && (2..=30).contains(&place.chars().count())
            && place
                .chars()
                .all(|c| c.is_alphanumeric() || "ー・々".contains(c))
            && crate::analyze::japanese(place)
            && icon_tail(tail)
        {
            read.titles.insert(format!("{place}へ"));
        } else if let Some(marker) = marker_name
            && (2..=30).contains(&value.chars().count())
            && value
                .chars()
                .all(|c| c.is_alphanumeric() || "ー・々".contains(c))
            && crate::analyze::japanese(value)
        {
            if marker == "目的地" {
                read.actions.insert(format!("{value}へ"));
            } else {
                read.titles.insert(value.to_owned());
            }
        }
    }
    read
}

impl Reading {
    fn useful(&self) -> bool {
        self.explicit || !self.actions.is_empty() || !self.titles.is_empty()
    }

    fn goal(&self) -> Option<Goal> {
        if self.actions.len() != 1
            || self.titles.len() > 1
            || (self.titles.is_empty() && !self.explicit)
        {
            return None;
        }
        Some(Goal {
            title: self.titles.first().cloned().unwrap_or_default(),
            action: self.actions.first()?.clone(),
            slot: if self.titles.is_empty() && self.slots.len() == 1 {
                self.slots.first().copied()
            } else {
                None
            },
        })
    }

    fn add(&mut self, other: Self) {
        self.titles.extend(other.titles);
        self.actions.extend(other.actions);
        self.explicit |= other.explicit;
        self.slots.extend(other.slots);
    }
}

// Mirror the same panels along both edges. The tighter view enlarges small
// title and action text while excluding unrelated map icons or screen captions.
#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct Panel {
    right: bool,
    top: u32,
}

const PANELS: [Panel; 6] = [
    Panel {
        right: true,
        top: 30,
    },
    Panel {
        right: false,
        top: 30,
    },
    Panel {
        right: true,
        top: 0,
    },
    Panel {
        right: false,
        top: 0,
    },
    Panel {
        right: true,
        top: 60,
    },
    Panel {
        right: false,
        top: 60,
    },
];

fn panel_read(frame: &Frame, panel: Panel, command: Option<&Path>) -> Result<Reading> {
    let img = image::load_from_memory_with_format(&frame.jpeg, ImageFormat::Jpeg)?;
    let (w, h) = (img.width(), img.height());
    let mut read = Reading::default();
    for (width_pct, y_offset, height_pct, zoom) in [(35, 0, 30, 1), (27, 8, 20, 3)] {
        let width = (w * width_pct / 100).max(1);
        let height = (h * height_pct / 100).max(1);
        let x = if panel.right { w - width } else { 0 };
        let y = h * (panel.top + y_offset) / 100;
        let crop = img.crop_imm(x, y, width, height);
        let target = (width * zoom).min(1500);
        let crop = if width == target {
            crop
        } else {
            crop.resize_exact(
                target,
                (height * target / width).max(1),
                FilterType::Lanczos3,
            )
        };
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 92).encode_image(&crop)?;
        let text = crate::ocr::read(&jpeg, command)?;
        if interface(&text) || crate::quest::is_offer_screen(&text) {
            return Ok(Reading::default());
        }
        read.add(read_lines(&text));
        if read.goal().is_some() {
            break;
        }
    }
    Ok(read)
}

#[derive(Default)]
pub struct Tracker {
    events: Vec<Event>,
    current: BTreeMap<Topic, (u64, String)>,
    preferred: Option<Panel>,
    next_sweep: u64,
    unavailable: bool,
}

struct ScreenText<'a> {
    words: &'a BTreeMap<u64, String>,
    is_cursor: &'a dyn Fn(u64) -> bool,
    read_full: &'a dyn Fn(&Frame) -> Result<String>,
}

impl ScreenText<'_> {
    fn excluded(&self, second: u64) -> bool {
        (self.is_cursor)(second)
            || self
                .words
                .get(&second)
                .is_some_and(|text| interface(text) || crate::quest::is_offer_screen(text))
    }
}

impl Tracker {
    pub fn observe(
        &mut self,
        from: u64,
        frames: &VecDeque<Frame>,
        words: &BTreeMap<u64, String>,
        maps: &crate::map::Tracker,
        analyzer: &crate::analyze::Analyzer<'_>,
        complete: &mut bool,
    ) -> Result<()> {
        let screen = ScreenText {
            words,
            is_cursor: &|second| maps.is_cursor(second),
            read_full: &|frame| crate::ocr::read(&frame.jpeg, analyzer.ocr_command),
        };
        self.observe_with(
            from,
            frames,
            screen,
            analyzer.dir,
            complete,
            |frame, panel| panel_read(frame, panel, analyzer.ocr_command),
        )
    }

    fn observe_with(
        &mut self,
        from: u64,
        frames: &VecDeque<Frame>,
        screen: ScreenText<'_>,
        dir: &Path,
        complete: &mut bool,
        mut recognize: impl FnMut(&Frame, Panel) -> Result<Reading>,
    ) -> Result<()> {
        if self.unavailable {
            return Ok(());
        }
        let failed = Cell::new(false);
        let mut readings: BTreeMap<(usize, Panel), Reading> = BTreeMap::new();
        // Playback start and two distinct seconds supporting the complete label.
        let mut seen: BTreeMap<(u64, Goal), (u64, u64)> = BTreeMap::new();
        let mut last_clue = None;
        let mut inspect = |index: usize, panel: Panel| -> Option<Reading> {
            if failed.get() {
                return None;
            }
            let read = readings.entry((index, panel)).or_insert_with(|| {
                recognize(&frames[index], panel).unwrap_or_else(|error| {
                    eprintln!("目標欄の追加 OCR を利用できません ({error})");
                    failed.set(true);
                    Reading::default()
                })
            });
            (!failed.get()).then(|| read.clone())
        };
        // The first two independent observations can straddle a scan window.
        for (index, frame) in frames
            .iter()
            .enumerate()
            .filter(|(_, f)| f.second >= from.saturating_sub(8))
        {
            let text = screen
                .words
                .get(&frame.second)
                .map(String::as_str)
                .unwrap_or("");
            if screen.excluded(frame.second) {
                continue;
            }
            let clue = read_lines(text);
            if clue.explicit
                && let Some(goal) = clue.goal()
                && let Some((second, _)) = (frame.second.saturating_add(1)
                    ..=frame.second.saturating_add(8))
                    .take_while(|&second| !screen.excluded(second))
                    .filter_map(|second| {
                        let other = read_lines(screen.words.get(&second)?);
                        let observed = other.goal()?;
                        other.explicit.then_some((second, observed))
                    })
                    .take_while(|(_, next)| !goal.replaced_by(next))
                    .find(|(_, next)| *next == goal)
            {
                seen.entry((frame.second, goal))
                    .or_insert((frame.second, second));
            }
            let periodic = frame.second >= self.next_sweep;
            if !periodic
                && (!clue.useful()
                    || last_clue
                        .as_ref()
                        .is_some_and(|(t, previous)| frame.second - t <= 2 && previous == &clue))
            {
                continue;
            }
            if periodic {
                self.next_sweep = frame.second.saturating_add(20);
            }
            let preferred = self.preferred;
            let panels = preferred
                .into_iter()
                .chain(PANELS.into_iter().filter(|p| Some(*p) != preferred));
            let mut corroborated = false;
            for panel in panels {
                let Some(seed) = inspect(index, panel) else {
                    break;
                };
                if !seed.useful() {
                    continue;
                }
                let mut previous = BTreeMap::<Goal, usize>::new();
                let mut matched = Vec::new();
                for (near, f) in frames
                    .iter()
                    .enumerate()
                    .take(index + 4)
                    .skip(index.saturating_sub(3))
                {
                    let second = f.second;
                    if second.abs_diff(frame.second) > 8 || screen.excluded(second) {
                        previous.clear();
                        continue;
                    }
                    let Some(read) = inspect(near, panel) else {
                        break;
                    };
                    if let Some(goal) = read.goal() {
                        previous.retain(|prior, _| !prior.replaced_by(&goal));
                        if let Some(first) = previous.insert(goal.clone(), near) {
                            matched.push((goal, first, near));
                        }
                    }
                }
                let mut confirmed = false;
                for (goal, first, proof) in matched {
                    // Work back through this display when small type was first
                    // noticed at a periodic probe. Do not bridge a changed or hidden HUD.
                    let mut start = frames[first].second;
                    for (i, old) in frames.iter().enumerate().take(first).rev() {
                        if frames[first].second - old.second > 20 || screen.excluded(old.second) {
                            break;
                        }
                        let Some(read) = inspect(i, panel) else {
                            break;
                        };
                        if !read.actions.contains(&goal.action)
                            || read.actions.len() != 1
                            || (!read.titles.is_empty() && !read.titles.contains(&goal.title))
                        {
                            break;
                        }
                        start = old.second;
                    }
                    confirmed |= clue.actions.is_empty() || clue.actions.contains(&goal.action);
                    seen.entry((start, goal))
                        .or_insert((frames[first].second, frames[proof].second));
                }
                if confirmed {
                    corroborated = true;
                    self.preferred = Some(panel);
                    if !periodic {
                        break;
                    }
                }
            }
            // An unsuccessful sweep at a menu exit must not suppress the
            // first actionable clue on the very next frame.
            if corroborated && clue.useful() {
                last_clue = Some((frame.second, clue));
            }
            if failed.get() {
                break;
            }
        }
        if failed.get() {
            *complete = false;
            self.unavailable = true;
        }
        // Some small text was detectable only in a crop. Complete the source
        // context of accepted goals without changing the base model's inputs.
        let mut originals = Cow::Borrowed(screen.words);
        for ((mut start, goal), (first_seen, proof)) in seen {
            let key = goal.topic();
            if self
                .current
                .get(&key)
                .is_some_and(|(previous, action)| start <= *previous || action == &goal.action)
            {
                continue;
            }
            for second in crate::analyze::evidence_seconds(start, first_seen, proof) {
                if !self.unavailable
                    && !originals.contains_key(&second)
                    && let Some(frame) = frames.iter().find(|f| f.second == second)
                {
                    match (screen.read_full)(frame) {
                        Ok(text) => {
                            originals.to_mut().insert(second, text);
                        }
                        Err(error) => {
                            eprintln!("目標の元画面 OCR を利用できません ({error})");
                            *complete = false;
                            self.unavailable = true;
                        }
                    }
                }
            }
            let menu = |second| {
                originals
                    .get(&second)
                    .is_some_and(|text| interface(text) || crate::quest::is_offer_screen(text))
            };
            if menu(first_seen) || menu(proof) {
                continue;
            }
            if menu(start) {
                start = first_seen;
            }
            let weak_copy = matches!(key, Topic::Independent(_))
                && self.current.iter().any(|(topic, (_, action))| {
                    matches!(topic, Topic::Active(_)) && action == &goal.action
                });
            self.current.insert(key, (start, goal.action.clone()));
            let label = goal.heading();
            // The full image and the narrower crop can show the same action
            // with or without its active-slot label. Keep both pieces of state
            // while publishing their overlapping proof only once.
            if weak_copy
                || self
                    .events
                    .iter()
                    .any(|e| e.heading == label && e.start <= proof && start <= e.seen)
            {
                continue;
            }
            let first = frames.iter().find(|f| f.second == start).unwrap();
            let last = frames.iter().find(|f| f.second == proof).unwrap();
            self.events.push(Event {
                start,
                seen: proof,
                heading: label.clone(),
                basis: format!("{first_seen} 秒と {proof} 秒の目標欄に「{label}」を確認。開始は画面で追える最初の {start} 秒"),
                start_image: Some(crate::analyze::save(dir, first)?),
                evidence_image: crate::analyze::save(dir, last)?,
                ocr: crate::analyze::event_ocr(
                    &originals, start, first_seen, proof, format!("{} {}",goal.title,goal.action).trim(),
                ),
                screen_label: Some(label),
            });
        }
        Ok(())
    }

    pub fn merge(self, events: &mut Vec<Event>) {
        for goal in self.events {
            // An arrival or a conversation at this place is still a separate event.
            events.retain(|e| {
                !(e.start.abs_diff(goal.start) <= 5
                    && e.screen_label.is_none()
                    && (e.heading == goal.heading
                        || (e.heading.contains("目標")
                            && goal.heading.contains(
                                e.heading
                                    .trim_start_matches("目標")
                                    .trim_matches(['「', '」', '：', ':']),
                            ))))
            });
            events.push(goal);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screens(words: &BTreeMap<u64, String>) -> ScreenText<'_> {
        ScreenText {
            words,
            is_cursor: &|_| false,
            read_full: &|_| Ok(String::new()),
        }
    }

    #[test]
    fn separates_a_destination_from_its_changing_action_and_ignores_dialogue_and_controls() {
        let goal = |s| read_lines(s).goal();
        let first = goal("暁の灯台へ ◇\n海岸を調査する〈ロ").unwrap();
        let second = goal("暁の灯台へ\n西の波止場に向かう").unwrap();
        assert_eq!(first.title, second.title);
        assert_ne!(first.action, second.action);
        assert_eq!(goal("オート再生\n暁の灯台へ\n海岸を調査する"), None);
        assert_eq!(goal("西の波止場に向かうつもりだ。"), None);
        assert_eq!(goal("暁の灯台へ行ってみよう\n海岸を調査する"), None);
        assert_eq!(
            goal("目標：ランタンを届ける").unwrap().action,
            "ランタンを届ける"
        );
        assert_eq!(goal("目的地：北の関所").unwrap().action, "北の関所へ");
    }

    #[test]
    fn follows_the_first_visible_frame_and_keeps_parallel_goals_and_new_steps() -> Result<()> {
        let frames: VecDeque<_> = (0..48)
            .map(|second| Frame {
                second,
                jpeg: vec![second as u8],
            })
            .collect();
        let words = BTreeMap::from([
            (11, "海岸を調査する".into()),
            (16, "鐘を鳴らす".into()),
            (26, "海岸を調査する".into()),
            (32, "西の波止場に向かう".into()),
            (44, "次の目標".into()),
        ]);
        let root = tempfile::tempdir()?;
        std::fs::create_dir(root.path().join("frames"))?;
        let mut tracker = Tracker::default();
        let right = PANELS[0];
        let left = PANELS[1];
        let mut complete = true;
        tracker.observe_with(
            0,
            &frames,
            screens(&words),
            root.path(),
            &mut complete,
            |frame, panel| {
                let result = match (panel, frame.second) {
                    (p, 4..=18 | 24..=27) if p == right => "暁の灯台へ\n海岸を調査する",
                    (p, 16..=21) if p == left => "目標：古い鐘を探す",
                    (p, 28..=36) if p == right => "暁の灯台へ\n西の波止場に向かう",
                    (p, 44) if p == right => "闇の塔へ\n扉を開ける",
                    _ => "",
                };
                Ok(read_lines(result))
            },
        )?;
        assert!(complete);
        let rows: Vec<_> = tracker
            .events
            .iter()
            .map(|e| (e.start, e.heading.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                (4, "暁の灯台へ（海岸を調査する）"),
                (16, "目標「古い鐘を探す」"),
                (28, "暁の灯台へ（西の波止場に向かう）")
            ]
        );
        for event in tracker.events {
            assert!(event.seen > event.start);
            assert_eq!(
                std::fs::read(root.path().join(event.start_image.unwrap()))?,
                [event.start as u8]
            );
            assert_eq!(
                std::fs::read(root.path().join(event.evidence_image))?,
                [event.seen as u8]
            );
        }
        Ok(())
    }

    #[test]
    fn explicit_centered_goal_can_be_confirmed_across_windows() -> Result<()> {
        let frames: VecDeque<_> = (290..=310)
            .map(|second| Frame {
                second,
                jpeg: vec![1],
            })
            .collect();
        let dir = tempfile::tempdir()?;
        std::fs::create_dir(dir.path().join("frames"))?;
        let words = BTreeMap::from([
            (299, "現在の目標：東の砦を守る".into()),
            (300, "現在の目標：東の砦を守る".into()),
        ]);
        let mut tracker = Tracker::default();
        let mut complete = true;
        tracker.observe_with(
            300,
            &frames,
            screens(&words),
            dir.path(),
            &mut complete,
            |_, _| Ok(Reading::default()),
        )?;
        assert!(complete);
        assert_eq!(tracker.events.len(), 1);
        assert_eq!(
            (tracker.events[0].start, tracker.events[0].seen),
            (299, 300)
        );
        Ok(())
    }

    #[test]
    fn replacing_an_active_objective_can_restore_it_while_side_goals_stay_independent() -> Result<()>
    {
        let frames: VecDeque<_> = (0..24)
            .map(|second| Frame {
                second,
                jpeg: vec![second as u8],
            })
            .collect();
        let words = BTreeMap::from([
            (2, "現在の目標：北門へ向かう".into()),
            (3, "現在の目標：北門へ向かう".into()),
            (4, "副目標：銀貨を集める".into()),
            (5, "副目標：銀貨を集める".into()),
            (8, "現在の目標：南門へ向かう".into()),
            (9, "現在の目標：南門へ向かう".into()),
            (11, "副目標：古い鐘を探す".into()),
            (12, "副目標：古い鐘を探す".into()),
            (15, "現在の目標：北門へ向かう".into()),
            (16, "現在の目標：北門へ向かう\n地区：白波".into()),
            (17, "隣の画面の名前".into()),
            (19, "副目標：銀貨を集める".into()),
            (20, "副目標：銀貨を集める".into()),
        ]);
        let dir = tempfile::tempdir()?;
        std::fs::create_dir(dir.path().join("frames"))?;
        let mut tracker = Tracker::default();
        tracker.observe_with(
            0,
            &frames,
            screens(&words),
            dir.path(),
            &mut true,
            |frame, panel| {
                // A tighter crop can lose "現在の" while showing the same
                // physical label that the full frame identifies as active.
                if panel == PANELS[0]
                    && let Some(line) = words.get(&frame.second)
                    && line.starts_with("現在の目標")
                {
                    Ok(read_lines(line.trim_start_matches("現在の")))
                } else {
                    Ok(Reading::default())
                }
            },
        )?;
        assert_eq!(
            tracker.events.iter().map(|e| e.start).collect::<Vec<_>>(),
            [2, 4, 8, 11, 15]
        );
        let restored = tracker.events.last().unwrap();
        assert_eq!(restored.seen, 16);
        let proof = crate::dictionary::Evidence::new(restored.seen, &restored.ocr, &restored.basis);
        assert!(proof.ocr.contains("地区：白波"));
        assert!(!proof.ocr.contains("隣の画面の名前"));
        assert!(restored.ocr.contains("17 隣の画面の名前"));
        Ok(())
    }

    #[test]
    fn a_blind_sweep_on_return_to_play_does_not_consume_the_next_actionable_clue() -> Result<()> {
        let frames: VecDeque<_> = (0..20)
            .map(|second| Frame {
                second,
                jpeg: vec![1],
            })
            .collect();
        let words = BTreeMap::from([(12, "現在地：霞の広場".into()), (13, "扉を開ける".into())]);
        let dir = tempfile::tempdir()?;
        std::fs::create_dir(dir.path().join("frames"))?;
        let screen = ScreenText {
            words: &words,
            is_cursor: &|s| s < 12,
            read_full: &|_| Ok(String::new()),
        };
        let mut tracker = Tracker::default();
        tracker.observe_with(0, &frames, screen, dir.path(), &mut true, |f, p| {
            let text = if p == PANELS[0] {
                match f.second {
                    12 | 13 => "扉を開ける",
                    14 | 16 => "霧の庭へ\n扉を開ける",
                    _ => "",
                }
            } else {
                ""
            };
            Ok(read_lines(text))
        })?;
        assert_eq!(tracker.events.len(), 1);
        assert_eq!((tracker.events[0].start, tracker.events[0].seen), (12, 16));
        Ok(())
    }

    #[test]
    fn crop_only_objective_keeps_full_proof_context_at_the_original_second() -> Result<()> {
        let frames: VecDeque<_> = (0..5)
            .map(|second| Frame {
                second,
                jpeg: vec![1],
            })
            .collect();
        let words = BTreeMap::new();
        let dir = tempfile::tempdir()?;
        std::fs::create_dir(dir.path().join("frames"))?;
        let screen = ScreenText {
            words: &words,
            is_cursor: &|_| false,
            read_full: &|frame| Ok(format!("地区：{}区", frame.second)),
        };
        let mut tracker = Tracker::default();
        tracker.observe_with(0, &frames, screen, dir.path(), &mut true, |f, p| {
            Ok(if p == PANELS[0] && f.second <= 2 {
                read_lines("目的地：雪の港")
            } else {
                Reading::default()
            })
        })?;
        assert_eq!(tracker.events.len(), 1);
        let goal = &tracker.events[0];
        let proof = crate::dictionary::Evidence::new(goal.seen, &goal.ocr, &goal.basis);
        assert!(proof.ocr.contains(&format!("地区：{}区", goal.seen)));
        assert!(proof.ocr.contains("雪の港へ"));
        assert!(!proof.ocr.contains("地区：2区"));
        assert!(goal.ocr.contains("2 地区：2区"));
        Ok(())
    }

    #[test]
    fn map_selection_cannot_supply_an_objective_even_when_a_tight_crop_shows_only_the_place()
    -> Result<()> {
        let frames: VecDeque<_> = (0..6)
            .map(|second| Frame {
                second,
                jpeg: vec![1],
            })
            .collect();
        let words = BTreeMap::from([
            (0, "地図を閉じる\n目的地：白波の港".into()),
            (1, "カーソルリセット\n目的地：白波の港".into()),
            (2, "目的地：白波の港".into()),
            (3, "目的地：白波の港".into()),
            (4, "現在の目標：鐘を探す".into()),
            (5, "現在の目標：鐘を探す".into()),
        ]);
        let dir = tempfile::tempdir()?;
        std::fs::create_dir(dir.path().join("frames"))?;
        let mut tracker = Tracker::default();
        let screen = ScreenText {
            words: &words,
            is_cursor: &|sec| (2..=3).contains(&sec),
            read_full: &|_| Ok(String::new()),
        };
        tracker.observe_with(0, &frames, screen, dir.path(), &mut true, |frame, _| {
            assert!(frame.second >= 4);
            Ok(Reading::default())
        })?;
        assert_eq!(tracker.events.len(), 1);
        assert_eq!(tracker.events[0].heading, "目標「鐘を探す」");
        assert!(
            read_lines("目的地：白波の港\nマップを閉じる")
                .goal()
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn failed_panel_ocr_falls_back_to_existing_events_and_disables_cache_publication() -> Result<()>
    {
        let frames = [Frame {
            second: 0,
            jpeg: vec![],
        }]
        .into();
        let mut tracker = Tracker::default();
        let mut complete = true;
        let dir = tempfile::tempdir()?;
        tracker.observe_with(
            0,
            &frames,
            screens(&BTreeMap::new()),
            dir.path(),
            &mut complete,
            |_, _| anyhow::bail!("OCR unavailable"),
        )?;
        assert!(!complete);
        tracker.observe_with(
            0,
            &frames,
            screens(&BTreeMap::new()),
            dir.path(),
            &mut complete,
            |_, _| panic!("failed adapter used again"),
        )?;
        assert!(tracker.events.is_empty());
        Ok(())
    }
}
