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

fn cursor_control(text: &str) -> bool {
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
    if event.screen_label.is_some()
        || ["目標", "目的地を設定"]
            .iter()
            .any(|w| event.heading.contains(w))
    {
        return None;
    }
    [
        "に到着",
        "へ到着",
        "に到達",
        "へ到達",
        "に移動",
        "へ移動",
        "への移動",
        "に移動する",
        "へ移動する",
        "に入る",
        "へ入る",
        "へ戻る",
        "に戻る",
    ]
    .iter()
    .find_map(|suffix| event.heading.strip_suffix(suffix))
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

#[derive(Default)]
pub struct Tracker {
    decisions: BTreeMap<Key, Option<Event>>,
    modes: BTreeMap<u64, Screen>,
}

impl Tracker {
    pub fn observe(
        &mut self,
        from: u64,
        frames: &VecDeque<Frame>,
        words: &BTreeMap<u64, String>,
        events: &[Event],
        analyzer: &Analyzer<'_>,
    ) -> Result<()> {
        self.observe_with(from, frames, words, events, analyzer.dir, |frame| {
            let prompt = "このゲーム画面でプレイヤーが操作している対象を観察してください。\n\
                cursor = 画面全体が地点を閲覧・選択する地図メニュー。地域の地形図、位置マーカー、境界、経路、地点アイコン、説明カードなどを広域に配置し、実際に歩かせている人物や駒は見えない。カーソルを動かすと地図自体がスクロールします。\n\
                play = 風景またはプレイ用のワールドマップに動かせる人物・駒が実際に見え、その位置を操作するゲーム画面。片隅にミニマップがあってもplay。\n\
                other = ロード、暗転、判断がつかない画面。JSONでkindをひとつ返してください。";
            let view: View = analyzer.model.infer(prompt, &sheet::render(&[frame], 1, 960)?)?;
            Ok(view.kind)
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
            if anchors
                .range(event.seen.saturating_sub(12)..=event.seen.saturating_add(12))
                .next()
                .is_none()
            {
                continue;
            }
            let Some(frame) = frames.iter().find(|f| f.second == event.seen) else {
                continue;
            };
            required.insert(frame.second);
            // Only a standalone field label can support moving the head to an actual return.
            let later: Vec<_> = words
                .range(event.seen.saturating_add(1)..=event.seen.saturating_add(12))
                .filter(|(_, s)| place_label(s, name))
                .map(|(&sec, _)| sec)
                .collect();
            if let [_, .., end] = later.as_slice() {
                required.extend(
                    frames
                        .iter()
                        .map(|f| f.second)
                        .filter(|sec| *sec > event.seen && sec <= end),
                );
            }
            candidates.push((event, name, later));
        }
        for frame in frames.iter().filter(|f| required.contains(&f.second)) {
            if let std::collections::btree_map::Entry::Vacant(mode) = self.modes.entry(frame.second)
            {
                // Each candidate or corroborating picture has its own verdict.
                // A verdict about another tile does not establish a transition.
                mode.insert(classify(frame)?);
            }
        }
        for (event, name, later) in candidates {
            if self.modes.get(&event.seen) != Some(&Screen::Cursor) {
                continue;
            }
            let key = (event.start, event.seen, event.heading.clone());
            let mut field = None;
            if let Some((onset, first, proof)) = self.field_return(event.seen, &later)
                && let Some(first_image) = frames.iter().find(|f| f.second == onset)
                && let Some(proof_image) = frames.iter().find(|f| f.second == proof)
            {
                field = Some(Event {
                    start: onset,
                    seen: proof,
                    heading: format!("{name}でフィールドに復帰"),
                    basis: format!(
                        "{} 秒は地図の地点選択。{} 秒に操作画面へ切り替わり、{}・{} 秒に地名「{}」を確認",
                        event.seen, onset, first, proof, name
                    ),
                    ocr: format!("{first} {name}\n{proof} {name}"),
                    screen_label: Some(name.to_owned()),
                    start_image: Some(crate::analyze::save(dir, first_image)?),
                    evidence_image: crate::analyze::save(dir, proof_image)?,
                });
            }
            self.decisions.insert(key, field);
        }
        Ok(())
    }

    fn field_return(&self, selected: u64, later: &[u64]) -> Option<(u64, u64, u64)> {
        let [_, .., end] = later else { return None };
        let mut play = self
            .modes
            .range(selected + 1..=*end)
            .skip_while(|(_, mode)| **mode != Screen::Play)
            .take_while(|(_, mode)| **mode == Screen::Play)
            .map(|(&second, _)| second)
            .peekable();
        let onset = *play.peek()?;
        let mut labels = play.filter(|second| later.binary_search(second).is_ok());
        let (first, proof) = (labels.next()?, labels.next()?);
        (first - onset <= 2 && proof - first <= 3).then_some((onset, first, proof))
    }

    pub fn merge(mut self, events: &mut Vec<Event>) {
        let mut published = BTreeSet::new();
        events.retain_mut(|event| {
            let key = (event.start, event.seen, event.heading.clone());
            match self.decisions.remove(&key) {
                None => true,
                Some(None) => false,
                Some(Some(field)) => {
                    if !published.insert((field.start, field.heading.clone())) {
                        return false;
                    }
                    *event = field;
                    true
                }
            }
        });
    }
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
            (9, "現在地：風の塔".into()),
            (10, "広場へ向かう".into()),
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
