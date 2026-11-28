//! A cursor can pass over named destinations without the player travelling.
//! Verify the screen type only for movement candidates near map controls.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

use anyhow::Result;
use serde::Deserialize;

use crate::{
    analyze::{Analyzer, Event},
    media::Frame,
    sheet,
};

#[derive(Clone, Copy, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum Screen {
    Cursor,
    Play,
    Other,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct View {
    kind: Screen,
}

pub(crate) fn cursor_control(text: &str) -> bool {
    let words: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    [
        "カーソルリセット",
        "マップカーソル",
        "カーソルを現在地",
        "地図を閉じる",
        "マップを閉じる",
        "目的地を選択",
        "マーカーを設置",
        "ピンを設置",
        "ワープ先を選択",
        "ワールドマップを閉じ",
    ]
    .iter()
    .any(|t| words.contains(t))
}

fn destination(event: &Event) -> Option<&str> {
    if event.screen_label.is_some() {
        return None;
    }
    movement_destination(&event.heading)
}

pub(crate) fn movement_destination(heading: &str) -> Option<&str> {
    if ["目標", "目的地を設定"].iter().any(|w| heading.contains(w)) {
        return None;
    }
    [
        "に到着",
        "へ到着",
        "への到着",
        "への到達",
        "に到着した",
        "へ到着した",
        "に着く",
        "に着いた",
        "にたどり着く",
        "へたどり着く",
        "に到達",
        "へ到達",
        "に移動",
        "へ移動",
        "への移動",
        "に移動する",
        "へ移動する",
        "に入る",
        "へ入る",
        "に入った",
        "へ戻る",
        "に戻る",
        "へ戻った",
        "に戻った",
    ]
    .iter()
    .find_map(|suffix| heading.strip_suffix(suffix))
    .map(|name| {
        ["ワールドマップ上で", "マップ上で", "地図上で"]
            .into_iter()
            .find_map(|prefix| name.strip_prefix(prefix))
            .unwrap_or(name)
    })
    .map(|s| s.trim_matches(['「', '」', '『', '』', ' ']))
    .filter(|s| (2..=32).contains(&s.chars().count()))
    // Opening the destination picker is an event in its own right.
    .filter(|s| {
        !["地図", "マップ", "地図画面", "マップ画面"]
            .iter()
            .any(|ending| s.ends_with(ending))
    })
}

fn place_label(text: &str, name: &str) -> bool {
    let name: String = name.chars().filter(|c| !c.is_whitespace()).collect();
    text.lines().any(|line| {
        let line: String = line.chars().filter(|c| !c.is_whitespace()).collect();
        crate::location::explicit_place(&line).unwrap_or(&line) == name
    })
}

type Key = (u64, u64, String);

struct Return {
    event: Event,
    through: u64,
}

#[derive(Default)]
pub struct Tracker {
    decisions: BTreeMap<Key, Option<Return>>,
    modes: BTreeMap<u64, Screen>,
}

impl Tracker {
    pub(crate) fn is_cursor(&self, second: u64) -> bool {
        self.modes.get(&second) == Some(&Screen::Cursor)
    }

    pub(crate) fn cursor_seconds(&self) -> BTreeSet<u64> {
        self.modes
            .keys()
            .copied()
            .filter(|&sec| self.is_cursor(sec))
            .collect()
    }

    pub fn observe(
        &mut self,
        from: u64,
        frames: &VecDeque<Frame>,
        words: &BTreeMap<u64, String>,
        events: &[Event],
        analyzer: &Analyzer<'_>,
    ) -> Result<()> {
        self.observe_with(from, frames, words, events, analyzer.dir, |frame| {
            screen(analyzer, frame)
        })
    }

    fn observe_with(
        &mut self,
        from: u64,
        frames: &VecDeque<Frame>,
        words: &BTreeMap<u64, String>,
        events: &[Event],
        dir: &Path,
        mut classify: impl FnMut(&Frame) -> Result<Screen>,
    ) -> Result<()> {
        let anchors: BTreeSet<_> = words
            .iter()
            .filter(|(_, t)| cursor_control(t))
            .map(|(&sec, _)| sec)
            .collect();
        if anchors.is_empty() {
            return Ok(());
        }
        let mut candidates = Vec::new();
        let mut required = BTreeSet::new();
        for event in events.iter().filter(|e| e.seen >= from.saturating_sub(14)) {
            let Some(name) = destination(event) else {
                continue;
            };
            let pivots = BTreeSet::from([event.start, event.seen]);
            if !pivots.iter().any(|s| {
                anchors
                    .range(s.saturating_sub(12)..=s.saturating_add(12))
                    .next()
                    .is_some()
            }) {
                continue;
            }
            let mut labels = BTreeMap::new();
            for pivot in pivots {
                required.insert(pivot);
                // Either end of a proposal may still show the picker.
                let later: Vec<_> = words
                    .range(pivot.saturating_add(1)..=pivot.saturating_add(12))
                    .filter(|(_, text)| place_label(text, name))
                    .map(|(&s, _)| s)
                    .collect();
                if let [_, .., end] = later.as_slice() {
                    required.extend(
                        frames
                            .iter()
                            .map(|f| f.second)
                            .filter(|sec| pivot < *sec && sec <= end),
                    );
                }
                labels.insert(pivot, later);
            }
            candidates.push((event, name, labels));
        }
        for frame in frames.iter().filter(|f| required.contains(&f.second)) {
            if let std::collections::btree_map::Entry::Vacant(mode) = self.modes.entry(frame.second)
            {
                // Each candidate or corroborating picture has its own verdict.
                // A verdict about another tile does not establish a transition.
                mode.insert(classify(frame)?);
            }
        }
        for (event, name, labels) in candidates {
            let Some(selected) = [event.seen, event.start]
                .into_iter()
                .find(|&s| self.is_cursor(s))
            else {
                continue;
            };
            let key = (event.start, event.seen, event.heading.clone());
            let mut field = None;
            if let Some((onset, first, proof, through)) =
                self.field_return(selected, &labels[&selected], words, name)
                && let Some(first_image) = frames.iter().find(|f| f.second == onset)
                && let Some(proof_image) = frames.iter().find(|f| f.second == proof)
            {
                field = Some(Return {
                    through,
                    event: Event {
                        start: onset,
                        seen: proof,
                        heading: format!("{name}でフィールドに復帰"),
                        basis: format!(
                            "{} 秒は地図の地点選択。{} 秒に操作画面へ切り替わり、{}・{} 秒に地名「{}」を確認",
                            selected, onset, first, proof, name
                        ),
                        ocr: crate::analyze::event_ocr(words, onset, first, proof, name),
                        screen_label: Some(name.to_owned()),
                        start_image: Some(crate::analyze::save(dir, first_image)?),
                        evidence_image: crate::analyze::save(dir, proof_image)?,
                    },
                });
            } else if self.modes.get(&event.seen) == Some(&Screen::Play)
                && words
                    .get(&event.seen)
                    .is_some_and(|text| place_label(text, name))
            {
                // The model witnessed an actual location later, even when
                // there is too little evidence to move back from that frame.
                field = Some(Return {
                    through: event.seen,
                    event: Event {
                        start: event.seen,
                        seen: event.seen,
                        heading: event.heading.clone(),
                        basis: event.basis.clone(),
                        start_image: Some(event.evidence_image.clone()),
                        evidence_image: event.evidence_image.clone(),
                        screen_label: Some(name.to_owned()),
                        ocr: event.ocr.clone(),
                    },
                });
            }
            self.decisions.insert(key, field);
        }
        Ok(())
    }

    fn field_return(
        &self,
        selected: u64,
        later: &[u64],
        words: &BTreeMap<u64, String>,
        name: &str,
    ) -> Option<(u64, u64, u64, u64)> {
        let [_, .., end] = later else { return None };
        let mut play = self
            .modes
            .range(selected + 1..=*end)
            .skip_while(|(_, mode)| **mode != Screen::Play)
            .take_while(|(_, mode)| **mode == Screen::Play)
            .map(|(&second, _)| second)
            .peekable();
        let onset = *play.peek()?;
        let mut labels = play
            .take_while(|second| {
                !words.get(second).is_some_and(|text| {
                    text.lines()
                        .filter_map(|line| crate::location::explicit_place(line.trim()))
                        .any(|place| place != name)
                })
            })
            .filter(|second| later.binary_search(second).is_ok());
        let (first, proof) = (labels.next()?, labels.next()?);
        if first - onset > 2 || proof - first > 3 {
            return None;
        }
        let mut through = proof;
        for next in labels {
            if next - through > 3 {
                break;
            }
            through = next;
        }
        Some((onset, first, proof, through))
    }

    pub fn merge(mut self, events: &mut Vec<Event>) {
        // Identify each uninterrupted place display by the return to play,
        // not by the wording or the proof second of a particular proposal.
        let mut exits = BTreeMap::new();
        for field in self.decisions.values().flatten() {
            let event = &field.event;
            let key = (
                event.start,
                event.screen_label.as_deref().unwrap().to_owned(),
            );
            let range = exits
                .entry(key)
                .or_insert_with(|| (field.through, event.start_image.clone(), event.ocr.clone()));
            range.0 = range.0.max(field.through);
        }
        let mut belongs = BTreeMap::new();
        let mut owners = BTreeMap::new();
        for (id, event) in events.iter().enumerate() {
            let Some(place) = destination(event) else {
                continue;
            };
            if self.modes.get(&event.seen) != Some(&Screen::Play) {
                continue;
            }
            let Some((group, _)) = exits.iter().rev().find(|((start, name), (end, _, _))| {
                name == place
                    && (*start..=*end).contains(&event.seen)
                    && (event.start >= *start || self.is_cursor(event.start))
            }) else {
                continue;
            };
            let rank = (
                self.is_cursor(event.start),
                event.start.abs_diff(group.0),
                event.seen,
                id,
            );
            let owner = owners.entry(group.clone()).or_insert(rank);
            *owner = (*owner).min(rank);
            belongs.insert(id, group.clone());
        }
        let mut published = BTreeSet::new();
        let mut index = 0;
        events.retain_mut(|event| {
            let id = index;
            index += 1;
            if let Some(group) = belongs.get(&id) {
                if owners[group].3 != id {
                    return false;
                }
                if event.start != group.0 {
                    event.start = group.0;
                    event.start_image = exits[group].1.clone();
                    event.basis.push_str(&format!(
                        "。{}秒に操作画面へ切り替わり、同じ地名の表示を確認",
                        group.0
                    ));
                    let extra: Vec<_> = exits[group]
                        .2
                        .lines()
                        .filter(|text| !event.ocr.lines().any(|old| old == *text))
                        .collect();
                    if !extra.is_empty() {
                        if !event.ocr.is_empty() {
                            event.ocr.push('\n');
                        }
                        event.ocr.push_str(&extra.join("\n"));
                    }
                }
                return true;
            }
            let key = (event.start, event.seen, event.heading.clone());
            match self.decisions.remove(&key) {
                None => true,
                Some(None) => false,
                Some(Some(field)) => {
                    let field = field.event;
                    let group = (field.start, field.screen_label.as_ref().unwrap().clone());
                    if owners.contains_key(&group) || !published.insert(group) {
                        return false;
                    }
                    *event = field;
                    true
                }
            }
        });
    }
}

fn screen(analyzer: &Analyzer<'_>, frame: &Frame) -> Result<Screen> {
    let prompt = "このゲーム画面でプレイヤーが操作している対象を観察してください。\n\
        cursor = 画面全体が地点を閲覧・選択する地図メニュー。地域の地形図、位置マーカー、境界、経路、地点アイコン、説明カードなどを広域に配置し、実際に歩かせている人物や駒は見えない。カーソルを動かすと地図自体がスクロールします。\n\
        play = 風景またはプレイ用のワールドマップに動かせる人物・駒が実際に見え、その位置を操作するゲーム画面。片隅にミニマップがあってもplay。\n\
        other = ロード、暗転、判断がつかない画面。JSONでkindをひとつ返してください。";
    let view: View = analyzer
        .model
        .infer(prompt, &sheet::render(&[frame], 1, 960)?)?;
    Ok(view.kind)
}

pub(crate) fn playable_picture(analyzer: &Analyzer<'_>, frame: &Frame) -> Result<bool> {
    Ok(screen(analyzer, frame)? == Screen::Play)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(second: u64, heading: &str) -> Event {
        Event {
            start: second,
            seen: second,
            heading: heading.into(),
            basis: "場面の根拠".into(),
            start_image: None,
            evidence_image: String::new(),
            screen_label: None,
            ocr: String::new(),
        }
    }

    #[test]
    fn cursor_highlights_are_removed_but_travel_requires_character_control_and_repeated_field_text()
    -> Result<()> {
        let frames: VecDeque<_> = (0..13)
            .map(|second| Frame {
                second,
                jpeg: vec![second as u8],
            })
            .collect();
        let words = BTreeMap::from([
            (0, "地図を閉じる".into()),
            (3, "雪の港".into()),
            (4, "雪の港".into()),
            (6, "風の塔".into()),
            (7, "風の塔".into()),
            (8, "Area\n風の塔".into()),
            (9, "現在地：風の塔\n地区：白波".into()),
            (10, "広場へ向かう\n隣の画面の地区".into()),
            (12, "南の村".into()),
        ]);
        let mut events = vec![
            event(0, "地図を開く"),
            event(1, "ワールドマップへ移動"),
            event(2, "地図上で雪の港への移動"),
            event(6, "風の塔に到着"),
            event(10, "目標：広場へ向かう"),
            event(12, "南の村に到着"),
        ];
        let dir = tempfile::tempdir()?;
        std::fs::create_dir(dir.path().join("frames"))?;
        let mut tracker = Tracker::default();
        tracker.observe_with(0, &frames, &words, &events, dir.path(), |frame| {
            Ok(if frame.second < 8 {
                Screen::Cursor
            } else {
                Screen::Play
            })
        })?;
        tracker.merge(&mut events);
        let rows: Vec<_> = events
            .iter()
            .map(|e| (e.start, e.seen, e.heading.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                (0, 0, "地図を開く"),
                (1, 1, "ワールドマップへ移動"),
                (8, 9, "風の塔でフィールドに復帰"),
                (10, 10, "目標：広場へ向かう"),
                (12, 12, "南の村に到着")
            ]
        );
        assert_eq!(
            std::fs::read(dir.path().join(&events[2].evidence_image))?,
            [9]
        );
        let returned = &events[2];
        let proof = crate::dictionary::Evidence::new(returned.seen, &returned.ocr, &returned.basis);
        assert!(proof.ocr.contains("地区：白波"));
        assert!(!proof.ocr.contains("隣の画面の地区"));
        assert!(returned.ocr.contains("10 隣の画面の地区"));
        Ok(())
    }

    #[test]
    fn a_grounded_arrival_supersedes_only_its_own_reconstructed_map_exit() -> Result<()> {
        let frames: VecDeque<_> = (0..11)
            .map(|second| Frame {
                second,
                jpeg: vec![1],
            })
            .collect();
        let words = BTreeMap::from([
            (2, "地図を閉じる\n岬の港".into()),
            (5, "現在地：岬の港".into()),
            (6, "現在地：岬の港\n地区：白波".into()),
        ]);
        let dir = tempfile::tempdir()?;
        std::fs::create_dir(dir.path().join("frames"))?;
        let mut arrived = event(5, "岬の港に到着");
        arrived.seen = 6;
        arrived.basis = "操作画面で確認した入港".into();
        let mut events = vec![
            arrived,
            event(2, "岬の港に移動"),
            event(5, "目標：岬の港を調査する"),
            event(6, "岬の港で買い物"),
            event(7, "南の集落に到着"),
            event(9, "岬の港へ戻る"),
        ];
        let mut tracker = Tracker::default();
        tracker.observe_with(0, &frames, &words, &events, dir.path(), |f| {
            Ok(if f.second < 5 {
                Screen::Cursor
            } else {
                Screen::Play
            })
        })?;
        tracker.merge(&mut events);
        assert_eq!(
            events
                .iter()
                .map(|e| (e.start, e.heading.as_str()))
                .collect::<Vec<_>>(),
            [
                (5, "岬の港に到着"),
                (5, "目標：岬の港を調査する"),
                (6, "岬の港で買い物"),
                (7, "南の集落に到着"),
                (9, "岬の港へ戻る")
            ]
        );
        assert_eq!(events[0].basis, "操作画面で確認した入港");
        Ok(())
    }

    #[test]
    fn a_playable_proof_moves_the_cursor_start_and_revisiting_the_same_place_stays_separate()
    -> Result<()> {
        let frames: VecDeque<_> = (0..24)
            .map(|second| Frame {
                second,
                jpeg: vec![second as u8],
            })
            .collect();
        let dir = tempfile::tempdir()?;
        std::fs::create_dir(dir.path().join("frames"))?;
        for (candidate, another_menu) in [(2, false), (6, false), (2, true)] {
            let cursor = |sec| sec < 5 || (another_menu && (10..15).contains(&sec));
            let words = frames
                .iter()
                .map(|f| {
                    (
                        f.second,
                        if cursor(f.second) {
                            "地図を閉じる\n風の港"
                        } else {
                            "現在地：風の港"
                        }
                        .into(),
                    )
                })
                .collect();
            let mut arrived = event(candidate, "風の港に到着");
            arrived.seen = if candidate == 2 { 6 } else { 7 };
            let mut events = vec![
                arrived,
                event(2, "風の港へ移動"),
                event(5, "風の港の調査を開始"),
            ];
            if another_menu {
                events.push(event(11, "風の港へ移動"));
                let mut revisited = event(15, "風の港へ戻る");
                revisited.seen = 16;
                events.push(revisited);
            }
            let mut tracker = Tracker::default();
            tracker.observe_with(0, &frames, &words, &events, dir.path(), |f| {
                Ok(if cursor(f.second) {
                    Screen::Cursor
                } else {
                    Screen::Play
                })
            })?;
            tracker.merge(&mut events);
            let rows: Vec<_> = events
                .iter()
                .map(|e| (e.start, e.heading.as_str()))
                .collect();
            let mut expected = vec![(5, "風の港に到着"), (5, "風の港の調査を開始")];
            if another_menu {
                expected.push((15, "風の港へ戻る"));
            }
            assert_eq!(rows, expected);
            assert_eq!(
                std::fs::read(dir.path().join(events[0].start_image.as_ref().unwrap()))?,
                [5]
            );
        }
        // Even without a separate menu candidate, the later proof cannot
        // place a field arrival back on the earlier cursor frame.
        let words = frames
            .iter()
            .map(|f| {
                (
                    f.second,
                    if f.second < 5 {
                        "地図を閉じる\n風の港"
                    } else {
                        "現在地：風の港"
                    }
                    .into(),
                )
            })
            .collect();
        let mut only = event(2, "風の港に到着");
        only.seen = 6;
        let mut rows = vec![only];
        let mut tracker = Tracker::default();
        tracker.observe_with(0, &frames, &words, &rows, dir.path(), |f| {
            Ok(if f.second < 5 {
                Screen::Cursor
            } else {
                Screen::Play
            })
        })?;
        tracker.merge(&mut rows);
        assert_eq!((rows[0].start, rows[0].seen), (5, 6));
        Ok(())
    }

    #[test]
    fn an_unmarked_playable_overworld_never_triggers_cursor_filtering() -> Result<()> {
        let frames = (0..3)
            .map(|second| Frame {
                second,
                jpeg: vec![],
            })
            .collect();
        let words = BTreeMap::from([(1, "ワールドマップ\n青の村".into())]);
        let mut events = vec![event(1, "青の村に到着")];
        let mut tracker = Tracker::default();
        tracker.observe_with(
            0,
            &frames,
            &words,
            &events,
            std::path::Path::new("."),
            |_| panic!("not a map selection"),
        )?;
        tracker.merge(&mut events);
        assert_eq!(events.len(), 1);
        Ok(())
    }
}
