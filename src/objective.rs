//! A tracked HUD objective can change while the scenery and the active quest
//! remain the same. Read the panel independently of the model's scene choices.
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
}

impl Goal {
    fn heading(&self) -> String {
        if self.title.is_empty() {
            format!("目標「{}」", self.action)
        } else {
            format!("{}（{}）", self.title, self.action)
        }
    }
}

#[derive(Clone, Default)]
struct Reading {
    titles: BTreeSet<String>,
    actions: BTreeSet<String>,
    explicit: bool,
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
    [
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
            "目標",
            "目的地",
            "任務",
            "クエスト",
        ] {
            if let Some(rest) = line.strip_prefix(marker) {
                if rest.is_empty() {
                    read.explicit = true;
                } else if let Some(rest) = rest.strip_prefix(['：', ':']) {
                    read.explicit = true;
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
        })
    }

    fn add(&mut self, other: Self) {
        self.titles.extend(other.titles);
        self.actions.extend(other.actions);
        self.explicit |= other.explicit;
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
        read.add(read_lines(&crate::ocr::read(&jpeg, command)?));
        if read.goal().is_some() {
            break;
        }
    }
    Ok(read)
}

#[derive(Default)]
pub struct Tracker {
    events: Vec<Event>,
    current: BTreeMap<String, (u64, String)>,
    preferred: Option<Panel>,
    next_sweep: u64,
    unavailable: bool,
}

impl Tracker {
    pub fn observe(
        &mut self,
        from: u64,
        frames: &VecDeque<Frame>,
        words: &BTreeMap<u64, String>,
        command: Option<&Path>,
        dir: &Path,
        complete: &mut bool,
    ) -> Result<()> {
        self.observe_with(from, frames, words, dir, complete, |frame, panel| {
            panel_read(frame, panel, command)
        })
    }

    fn observe_with(
        &mut self,
        from: u64,
        frames: &VecDeque<Frame>,
        words: &BTreeMap<u64, String>,
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
            let text = words.get(&frame.second).map(String::as_str).unwrap_or("");
            if interface(text) || crate::quest::is_offer_screen(text) {
                continue;
            }
            let clue = read_lines(text);
            if clue.explicit
                && let Some(goal) = clue.goal()
                && let Some((&second, _)) = words
                    .range(frame.second.saturating_add(1)..=frame.second.saturating_add(8))
                    .find(|(_, t)| {
                        let other = read_lines(t);
                        other.explicit
                            && other.goal().as_ref() == Some(&goal)
                            && !crate::quest::is_offer_screen(t)
                    })
            {
                seen.entry((frame.second, goal))
                    .or_insert((frame.second, second));
            }
            let periodic = frame.second >= self.next_sweep;
            if !periodic && (!clue.useful() || last_clue.is_some_and(|t| frame.second - t <= 2)) {
                continue;
            }
            last_clue = Some(frame.second);
            if periodic {
                self.next_sweep = frame.second.saturating_add(20);
            }
            let preferred = self.preferred;
            let panels = preferred
                .into_iter()
                .chain(PANELS.into_iter().filter(|p| Some(*p) != preferred));
            for panel in panels {
                let Some(seed) = inspect(index, panel) else {
                    break;
                };
                if !seed.useful() {
                    continue;
                }
                let mut matched: BTreeMap<Goal, Vec<usize>> = BTreeMap::new();
                for (near, f) in frames
                    .iter()
                    .enumerate()
                    .take(index + 4)
                    .skip(index.saturating_sub(3))
                {
                    let second = f.second;
                    if second.abs_diff(frame.second) > 8
                        || words
                            .get(&second)
                            .is_some_and(|s| interface(s) || crate::quest::is_offer_screen(s))
                    {
                        continue;
                    }
                    let Some(read) = inspect(near, panel) else {
                        break;
                    };
                    if let Some(goal) = read.goal() {
                        matched.entry(goal).or_default().push(near);
                    }
                }
                let mut confirmed = false;
                for (goal, indices) in matched {
                    let [first, proof, ..] = indices.as_slice() else {
                        continue;
                    };
                    // Work back through this display when small type was first
                    // noticed at a periodic probe. Do not bridge a changed or hidden HUD.
                    let mut start = frames[*first].second;
                    for (i, old) in frames.iter().enumerate().take(*first).rev() {
                        if frames[*first].second - old.second > 20
                            || words
                                .get(&old.second)
                                .is_some_and(|s| interface(s) || crate::quest::is_offer_screen(s))
                        {
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
                        .or_insert((frames[*first].second, frames[*proof].second));
                }
                if confirmed {
                    self.preferred = Some(panel);
                    if !periodic {
                        break;
                    }
                }
            }
            if failed.get() {
                break;
            }
        }
        if failed.get() {
            *complete = false;
            self.unavailable = true;
        }
        for ((start, goal), (first_seen, proof)) in seen {
            let key = if goal.title.is_empty() {
                &goal.action
            } else {
                &goal.title
            };
            if self
                .current
                .get(key)
                .is_some_and(|(previous, action)| start <= *previous || action == &goal.action)
            {
                continue;
            }
            self.current
                .insert(key.clone(), (start, goal.action.clone()));
            let first = frames.iter().find(|f| f.second == start).unwrap();
            let last = frames.iter().find(|f| f.second == proof).unwrap();
            let label = goal.heading();
            self.events.push(Event {
                start,
                seen: proof,
                heading: label.clone(),
                basis: format!("{first_seen} 秒と {proof} 秒の目標欄に「{label}」を確認。開始は画面で追える最初の {start} 秒"),
                start_image: Some(crate::analyze::save(dir, first)?),
                evidence_image: crate::analyze::save(dir, last)?,
                ocr: format!(
                    "{first_seen} {} {}\n{proof} {} {}",
                    goal.title, goal.action, goal.title, goal.action
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
            &words,
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
        tracker.observe_with(300, &frames, &words, dir.path(), &mut complete, |_, _| {
            Ok(Reading::default())
        })?;
        assert!(complete);
        assert_eq!(tracker.events.len(), 1);
        assert_eq!(
            (tracker.events[0].start, tracker.events[0].seen),
            (299, 300)
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
            &BTreeMap::new(),
            dir.path(),
            &mut complete,
            |_, _| anyhow::bail!("OCR unavailable"),
        )?;
        assert!(!complete);
        tracker.observe_with(
            0,
            &frames,
            &BTreeMap::new(),
            dir.path(),
            &mut complete,
            |_, _| panic!("failed adapter used again"),
        )?;
        assert!(tracker.events.is_empty());
        Ok(())
    }
}
