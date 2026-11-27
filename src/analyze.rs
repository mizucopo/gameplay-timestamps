use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::media::Frame;
use crate::{model::Model, scene, sheet};

const WINDOW_SECONDS: u32 = 300;
const LOOKBACK: u64 = 180;

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Candidates {
    #[schemars(length(max = 8))]
    events: Vec<Candidate>,
    #[serde(default)]
    more: bool,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Candidate {
    start: u64,
    seen: u64,
    #[schemars(length(max = 24))]
    heading: String,
    #[serde(default)]
    kind: Option<Kind>,
    #[serde(default)]
    lead_in: bool,
    #[serde(default)]
    update: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Story,
    Place,
    Quest,
    Boss,
    Reward,
    Progress,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Confirmations {
    #[schemars(length(max = 4))]
    events: Vec<Confirmation>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Confirmation {
    id: usize,
    start: u64,
    #[schemars(length(max = 36))]
    heading: String,
    #[schemars(length(max = 64))]
    basis: String,
}

#[derive(Deserialize, Serialize)]
pub struct Event {
    pub start: u64,
    pub seen: u64,
    pub heading: String,
    pub basis: String,
    pub start_image: Option<String>,
    pub evidence_image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_label: Option<String>,
    pub ocr: String,
}

pub struct Analyzer<'a> {
    pub game: &'a str,
    pub edition: &'a str,
    pub model: &'a Model,
    pub dir: &'a Path,
    pub use_ocr: bool,
    pub ocr_command: Option<&'a Path>,
}

pub struct Scan {
    pub events: Vec<Event>,
    /// All requested observations finished, including optional lead-in checks.
    pub complete: bool,
}

struct Run<'a> {
    analyzer: &'a Analyzer<'a>,
    frames: VecDeque<Frame>,
    events: Vec<Event>,
    superseded: BTreeSet<usize>,
    verified: BTreeSet<usize>,
    pending: usize,
    signals: BTreeMap<u64, scene::Signal>,
    words: BTreeMap<u64, String>,
    // A cutscene's arrival card can appear after a conversation or in the next window.
    cutscenes: BTreeMap<u64, usize>,
    ocr_available: bool,
    complete: bool,
}

impl Analyzer<'_> {
    pub fn run(&self, video: &Path, step: u32) -> Result<Scan> {
        fs::create_dir(self.dir.join("frames"))?;
        let mut run = Run {
            analyzer: self,
            frames: VecDeque::new(),
            events: Vec::new(),
            superseded: BTreeSet::new(),
            verified: BTreeSet::new(),
            pending: 0,
            signals: BTreeMap::new(),
            words: BTreeMap::new(),
            cutscenes: BTreeMap::new(),
            ocr_available: self.use_ocr,
            complete: true,
        };
        crate::media::scan(video, step, |frame| {
            if run.frames.is_empty() {
                let path = save(self.dir, &frame)?;
                run.events.push(Event {
                    start: 0,
                    seen: frame.second,
                    heading: "動画開始".to_owned(),
                    basis: "録画の先頭".to_owned(),
                    start_image: (frame.second == 0).then(|| path.clone()),
                    evidence_image: path,
                    screen_label: None,
                    ocr: String::new(),
                });
            }
            let signature =
                scene::Signal::measure(&frame, run.signals.last_key_value().map(|(_, v)| v))?;
            run.signals.insert(frame.second, signature);
            run.frames.push_back(frame);
            run.pending += 1;
            if run.pending == WINDOW_SECONDS.div_ceil(step) as usize {
                run.process()?;
            }
            Ok(())
        })?;
        run.process()?;
        let mut events: Vec<_> = run
            .events
            .into_iter()
            .enumerate()
            .filter_map(|(id, event)| (!run.superseded.contains(&id)).then_some(event))
            .collect();
        events.sort_by_key(|event| event.start);
        Ok(Scan {
            events,
            complete: run.complete && (!self.use_ocr || run.ocr_available),
        })
    }
}

fn save(dir: &Path, frame: &Frame) -> Result<String> {
    let relative = format!("frames/{:08}.jpg", frame.second);
    let path = dir.join(&relative);
    if !path.exists() {
        fs::write(path, &frame.jpeg)?;
    }
    Ok(relative)
}

fn clean(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn japanese(value: &str) -> bool {
    value
        .chars()
        .any(|c| matches!(c, 'ぁ'..='ヿ' | '㐀'..='鿿'))
}

fn read_screen(
    words: &mut BTreeMap<u64, String>,
    available: &mut bool,
    frame: &Frame,
    command: Option<&Path>,
) {
    if !*available || words.contains_key(&frame.second) {
        return;
    }
    match crate::ocr::read(&frame.jpeg, command) {
        Ok(text) => {
            words.insert(frame.second, text);
        }
        Err(error) => {
            eprintln!("日本語 OCR を利用できません。映像推論を続けます ({error})");
            *available = false;
        }
    }
}

impl Run<'_> {
    fn process(&mut self) -> Result<()> {
        if self.pending == 0 {
            return Ok(());
        }
        let first = self.frames.len() - self.pending;
        let from = self.frames[first].second;
        let until = self.frames.back().unwrap().second;
        self.inspect_range(first, self.frames.len())?;
        self.confirm_offers(from, until)?;
        self.identify_locations(until)?;
        if from / 300 != (until + 1) / 300 || from == 0 {
            eprintln!("走査済み {until} 秒 / 見出し {} 件", self.events.len());
        }
        self.pending = 0;
        let retain_from = self
            .cutscenes
            .first_key_value()
            .map_or(until.saturating_sub(LOOKBACK), |(&start, _)| {
                until.saturating_sub(LOOKBACK).min(start.saturating_sub(25))
            });
        while self.frames.front().is_some_and(|f| f.second < retain_from) {
            let second = self.frames.pop_front().unwrap().second;
            self.signals.remove(&second);
            self.words.remove(&second);
        }
        Ok(())
    }

    fn identify_locations(&mut self, until: u64) -> Result<()> {
        let cutscenes: Vec<_> = self
            .cutscenes
            .iter()
            .map(|(&start, &id)| (start, id))
            .collect();
        for (i, &(start, id)) in cutscenes.iter().enumerate() {
            let end = cutscenes
                .get(i + 1)
                .map_or(until, |(next, _)| next.saturating_sub(1))
                .min(start.saturating_add(LOOKBACK));
            if self.ocr_available
                && let Some(place) = crate::location::identify(
                    start,
                    end,
                    &self.frames,
                    &self.words,
                    self.analyzer.ocr_command,
                    &mut self.complete,
                )
            {
                let proof = self
                    .frames
                    .iter()
                    .find(|f| f.second == place.seen)
                    .context("場所名の根拠画面がありません")?;
                let heading = format!("{}の紹介", place.name);
                let matches = self
                    .events
                    .iter()
                    .enumerate()
                    .filter(|(other, e)| {
                        *other != id
                            && (place.start..=place.confirmed).contains(&e.start)
                            && e.heading.contains(&place.name)
                            && ["紹介", "到着", "到達", "に入る", "へ入る"]
                                .iter()
                                .any(|label| e.heading.contains(label))
                    })
                    .map(|(id, _)| id);
                self.superseded.extend(matches);
                self.events[id].seen = place.seen;
                self.events[id].heading = heading;
                self.events[id].basis = format!(
                    "{} 秒の移動目標と、{}・{} 秒の現在地表示「{}」が一致",
                    place.objective_second, place.seen, place.confirmed, place.name
                );
                self.events[id].ocr = format!("{} 秒の現在地表示：{}", place.seen, place.name);
                self.events[id].evidence_image = save(self.analyzer.dir, proof)?;
                self.events[id].screen_label = Some(place.name);
                self.verified.insert(id);
                self.cutscenes.remove(&start);
            } else if end < until || end - start >= LOOKBACK || !self.ocr_available {
                self.cutscenes.remove(&start);
            }
        }
        Ok(())
    }

    fn confirm_offers(&mut self, from: u64, until: u64) -> Result<()> {
        let mut extra = BTreeSet::new();
        for (i, frame) in self.frames.iter().enumerate() {
            if frame.second >= from.saturating_sub(2)
                && self
                    .words
                    .get(&frame.second)
                    .is_some_and(|t| crate::quest::is_offer_screen(t))
            {
                extra.extend(i.saturating_sub(2)..(i + 3).min(self.frames.len()));
            }
        }
        for i in extra {
            read_screen(
                &mut self.words,
                &mut self.ocr_available,
                &self.frames[i],
                self.analyzer.ocr_command,
            );
        }
        for offer in crate::quest::offers(&self.words, &self.signals, from, until) {
            let Some(first) = self.frames.iter().find(|f| f.second == offer.start) else {
                continue;
            };
            let Some(proof) = self.frames.iter().find(|f| f.second == offer.seen) else {
                continue;
            };
            let mut matching = self
                .events
                .iter()
                .enumerate()
                .filter(|(id, e)| *id != 0 && !self.superseded.contains(id) && offer.matches(e))
                .map(|(id, _)| id);
            let previous = matching.next();
            self.superseded.extend(matching.collect::<Vec<_>>());
            let event = Event {
                start: offer.start,
                seen: offer.seen,
                heading: format!("依頼「{}」", offer.title),
                basis: offer.basis,
                ocr: offer.ocr,
                start_image: Some(save(self.analyzer.dir, first)?),
                evidence_image: save(self.analyzer.dir, proof)?,
                screen_label: Some(offer.title),
            };
            if let Some(i) = previous {
                self.events[i] = event;
                self.verified.insert(i);
            } else {
                self.verified.insert(self.events.len());
                self.events.push(event);
            }
        }
        Ok(())
    }

    fn inspect_range(&mut self, first: usize, end: usize) -> Result<()> {
        let from = self.frames[first].second;
        let until = self.frames[end - 1].second;
        let begin = first.saturating_sub(2);
        let all: Vec<_> = self.frames.range(begin..end).collect();
        let panels = scene::panels(&all, &self.signals);
        if panels.len() > 36 && end - first > 24 {
            return self.split_range(first, end);
        }
        let a = self.analyzer;
        let mut screen = String::new();
        let text_seconds: BTreeSet<_> = all
            .iter()
            .step_by(5)
            .chain(panels.iter())
            .chain(all.iter().filter(|f| self.signals[&f.second].local_change))
            .map(|frame| frame.second)
            .collect();
        let mut previous = BTreeSet::new();
        for frame in all.iter().filter(|f| text_seconds.contains(&f.second)) {
            read_screen(
                &mut self.words,
                &mut self.ocr_available,
                frame,
                a.ocr_command,
            );
            if let Some(text) = self.words.get(&frame.second) {
                let lines: Vec<_> = text
                    .lines()
                    .map(clean)
                    .filter(|line| !line.is_empty())
                    .collect();
                let new: Vec<_> = lines
                    .iter()
                    .filter(|line| !previous.contains(line.as_str()))
                    .map(String::as_str)
                    .collect();
                if !new.is_empty() {
                    writeln!(
                        screen,
                        "{} {}",
                        frame.second,
                        new.join(" ").chars().take(160).collect::<String>()
                    )?;
                }
                previous = lines.into_iter().collect();
            }
        }
        if screen.chars().count() > 10_000 && end - first > 24 {
            return self.split_range(first, end);
        }
        let recent: Vec<_> = self
            .events
            .iter()
            .enumerate()
            .rev()
            .filter(|(id, _)| !self.superseded.contains(id))
            .filter(|(_, e)| e.seen >= from.saturating_sub(LOOKBACK))
            .take(20)
            .map(|(id, e)| serde_json::json!({"id":(!self.verified.contains(&id)).then_some(id),"start":e.start,"heading":e.heading}))
            .collect();
        let eligible_updates: BTreeSet<usize> = recent
            .iter()
            .filter_map(|entry| entry["id"].as_u64().map(|v| v as usize))
            .collect();
        let labels: Vec<_> = panels.iter().map(|f| f.second).collect();
        let prompt = format!(
            r#"作品『{}』版『{}』の動画を編集します。順に並ぶコマの再生秒は {labels:?}。各コマの黒い帯の整数も同じ秒です。新規対象 {from}〜{until} 秒。
この区間に新しく始まる章・場所紹介・依頼受注や更新・報酬・ボス登場や決着・物語上の発見を選ぶ。通常の移動、ロード一覧、天候、演出の途中の暗転、台詞が変わるだけなら空配列。会話は話題ごとでなく、一連の出来事を一件にする。敵の話をすることは敵の登場ではありません。画面に人物や戦いが現れず台詞で触れただけなら登場・勝利にしない。近接していても新しい別の出来事なら残す。ゲーム内時計を使わない。
同じ場所でも依頼の再開や違う場面なら別件。すでに掲載した場面 {}。後から同じ依頼の名前等が分かった場合だけ update にその行の id を指定。それ以外は null。
画面のOCR（時刻付き、画像の間のサンプルも含む。誤読あり、メニューの一覧を達成と混同しないこと）:
{screen}
JSON {{"events":[{{"start":整数,"seen":整数,"heading":"画面内の名称や変化を一言","kind":"story/place/quest/boss/reward/progress のいずれか","lead_in":false,"update":null}}],"more":false}}。時刻は画像かOCRの再生秒。seenは新規対象内の根拠コマ。後から示された依頼やボス名の導入を探す場合は lead_in を true とする。9件以上の独立した出来事が実際に見える場合だけ more を true にする。"#,
            a.game,
            a.edition,
            serde_json::to_string(&recent)?
        );
        let overview = sheet::render(&panels, 4, 384)?;
        let result: Candidates = match a.model.infer::<Candidates>(&prompt, &overview) {
            Ok(result) if result.more && end - first > 24 => {
                return self.split_range(first, end);
            }
            Err(error) if end - first > 24 && crate::model::needs_smaller_input(&error) => {
                return self.split_range(first, end);
            }
            result => result.with_context(|| format!("{from}〜{until} 秒の候補抽出"))?,
        };
        let resumptions = self.resume_after_menu(from, until)?;
        let mut candidates: Vec<_> = result
            .events
            .into_iter()
            .filter(|c| (from..=until).contains(&c.seen))
            .collect();
        for c in &mut candidates {
            c.update = c.update.filter(|id| eligible_updates.contains(id));
        }
        candidates.retain(|c| {
            !self
                .words
                .get(&c.seen)
                .is_some_and(|t| is_load_menu(t) || is_save_menu(t))
                && !resumptions.iter().any(|sec| {
                    sec.abs_diff(c.start) <= 3
                        && c.can_be_resumption()
                        && is_resume_heading(&c.heading)
                })
        });
        self.cutscene_entries(from, until)?;
        candidates.sort_by_key(|c| c.seen);
        for group in candidates.chunks(4) {
            self.confirm_group(group)?;
        }
        Ok(())
    }

    fn split_range(&mut self, first: usize, end: usize) -> Result<()> {
        let mid = first + (end - first) / 2;
        self.inspect_range(first, mid)?;
        self.inspect_range(mid, end)
    }

    fn resume_after_menu(&mut self, from: u64, until: u64) -> Result<Vec<u64>> {
        let mut resumptions = Vec::new();
        for i in 1..self.frames.len().saturating_sub(1) {
            let frame = &self.frames[i];
            if !(from..=until).contains(&frame.second)
                || self.signals[&frame.second].dark
                || !self.signals[&self.frames[i - 1].second].dark
            {
                continue;
            }
            let menu = self
                .words
                .range(frame.second.saturating_sub(25)..frame.second)
                .rev()
                .find(|(_, t)| is_load_menu(t));
            let Some((&menu_second, _)) = menu else {
                continue;
            };
            // The loaded screen must replace this menu, not be another copy of it.
            let screen = self
                .words
                .range(frame.second..=frame.second + 12)
                .filter(|(_, t)| !t.is_empty())
                .take(3)
                .collect::<Vec<_>>();
            if screen.is_empty()
                || screen.iter().any(|(_, t)| is_load_menu(t))
                || self
                    .events
                    .iter()
                    .any(|e| e.heading == "プレイ再開" && e.start > menu_second)
            {
                continue;
            }
            resumptions.push(frame.second);
            let path = save(self.analyzer.dir, frame)?;
            self.verified.insert(self.events.len());
            self.events.push(Event {
                start: frame.second,
                seen: frame.second,
                heading: "プレイ再開".into(),
                basis: format!("{menu_second} 秒のロード画面から暗転を挟んで操作画面に復帰"),
                start_image: Some(path.clone()),
                evidence_image: path,
                screen_label: None,
                ocr: clean(screen[0].1),
            });
        }
        Ok(resumptions)
    }

    // A field HUD disappearing into a skippable story sequence gives a
    // visual onset even when a location card's stylized lettering defeats OCR.
    fn cutscene_entries(&mut self, from: u64, until: u64) -> Result<()> {
        let mut last_field = None;
        let mut last_entry = None;
        for (&second, text) in &self.words {
            let t = text.to_lowercase().replace(' ', "");
            if [
                "ミニマップ",
                "マップ",
                "経路",
                "minimap",
                "へ向か",
                "に向か",
                "目的地",
            ]
            .iter()
            .any(|w| t.contains(w))
            {
                last_field = Some(second);
            }
            if !t.contains("スキップ") && !t.contains("skip") {
                continue;
            }
            let resumed = self
                .events
                .iter()
                .rev()
                .find(|e| e.heading == "プレイ再開" && e.start <= second && second - e.start <= 15)
                .map(|e| e.start);
            let Some(field) = last_field.take().or(resumed) else {
                continue;
            };
            if !(from..=until).contains(&second)
                || second.saturating_sub(field) > 15
                || last_entry.is_some_and(|seen| field <= seen)
            {
                continue;
            }
            let samples: Vec<_> = self
                .signals
                .range(field.saturating_sub(1)..=second)
                .collect();
            let onset = samples
                .windows(3)
                .find_map(|triple| {
                    let ((_, a), (&b_time, b), (_, c)) = (triple[0], triple[1], triple[2]);
                    (a.mean > b.mean + 15 && b.mean > c.mean + 10).then_some(b_time)
                })
                .or_else(|| {
                    samples
                        .windows(2)
                        .find(|p| p[1].1.change >= 32)
                        .map(|p| *p[1].0)
                });
            let Some(start) = onset else { continue };
            last_entry = Some(second);
            if self.cutscenes.contains_key(&start) {
                continue;
            }
            let images: Vec<_> = self
                .frames
                .iter()
                .filter(|f| f.second == start || f.second == second || f.second == second + 1)
                .collect();
            let proof = self
                .frames
                .iter()
                .find(|f| f.second == second)
                .expect("OCR sample in retained range");
            let first = images[0];
            let prompt = format!(
                "『{}』のフィールドの表示が消え、スキップ可能な演出に切り替わりました。再生秒 {} と {} 前後の画像を見比べてください。最初のカメラが場所・建物・地形の全景を紹介したら location、人物の対話から始まれば dialogue、判断がつかない場合は other。picture は画像に写る根拠を短く日本語で記入。JSON {{\"kind\":\"location\",\"picture\":\"根拠\"}}。",
                self.analyzer.game, start, second
            );
            #[derive(Deserialize, schemars::JsonSchema)]
            #[serde(rename_all = "lowercase")]
            enum ShotKind {
                Location,
                Dialogue,
                Other,
            }
            #[derive(Deserialize, schemars::JsonSchema)]
            #[serde(deny_unknown_fields)]
            struct Shot {
                kind: ShotKind,
                #[schemars(length(max = 48))]
                picture: String,
            }
            let shot: Shot = self
                .analyzer
                .model
                .infer(&prompt, &sheet::render(&images, 2, 720)?)?;
            let kind = match shot.kind {
                ShotKind::Location => "場所紹介",
                ShotKind::Dialogue => "会話シーン開始",
                ShotKind::Other => "イベントシーン開始",
            };
            self.cutscenes.insert(start, self.events.len());
            self.verified.insert(self.events.len());
            self.events.push(Event {
                start,
                seen: second,
                heading: kind.into(),
                basis: format!("探索画面からスキップ可能な演出へ。{}", clean(&shot.picture)),
                start_image: Some(save(self.analyzer.dir, first)?),
                evidence_image: save(self.analyzer.dir, proof)?,
                screen_label: None,
                ocr: text.clone(),
            });
        }
        Ok(())
    }

    fn find_lead(&self, candidate: &Candidate, index: usize, initial: u64) -> Result<u64> {
        if !candidate.lead_in
            || !matches!(candidate.kind, Some(Kind::Quest | Kind::Boss))
            || candidate.update.is_some()
        {
            return Ok(initial);
        }
        let proof = &self.frames[index];
        let timeline: Vec<_> = self
            .frames
            .iter()
            .take(index + 1)
            .filter(|f| f.second >= proof.second.saturating_sub(LOOKBACK))
            .collect();
        let mut panels: Vec<_> = timeline
            .iter()
            .step_by(timeline.len().div_ceil(16))
            .copied()
            .collect();
        if panels.last().is_none_or(|f| f.second != proof.second) {
            panels.push(proof);
        }
        if panels.len() < 3 {
            return Ok(initial);
        }
        #[derive(Deserialize, schemars::JsonSchema)]
        struct Lead {
            start: u64,
        }
        let prompt = format!(
            "作品『{}』。最後の{}秒の画像の出来事「{}」を確かめます。同じ人物・依頼・行動の導入が過去のコマに写っていれば、その再生秒。別の出来事や同じ場所での別件、単に似た画面を選ばない。特定できなければ {}。JSON {{\"start\":整数}}。",
            self.analyzer.game, proof.second, candidate.heading, initial
        );
        let answer: Lead = self
            .analyzer
            .model
            .infer(&prompt, &sheet::render(&panels, 4, 400)?)?;
        Ok(panels
            .iter()
            .find(|f| f.second == answer.start)
            .map_or(initial, |f| f.second))
    }

    fn confirm_group(&mut self, group: &[Candidate]) -> Result<()> {
        let mut panels = BTreeMap::new();
        let mut leads = BTreeMap::new();
        let mut inputs = Vec::new();
        for (id, c) in group.iter().enumerate() {
            let Some(index) = self.frames.iter().position(|f| f.second == c.seen) else {
                continue;
            };
            let initial = self
                .frames
                .iter()
                .find(|f| f.second == c.start && f.second <= c.seen)
                .map_or(c.seen, |f| f.second);
            let lead = match self.find_lead(c, index, initial) {
                Err(error) if crate::model::needs_smaller_input(&error) => {
                    self.complete = false;
                    initial
                }
                result => result?,
            };
            let first = self.frames.iter().position(|f| f.second == lead).unwrap();
            let mut times = Vec::new();
            // Show samples bracketing the proposed onset, plus both sides of the evidence.
            let coarse_radius = if lead < initial && c.lead_in { 12 } else { 2 };
            let onset_indices = first.saturating_sub(coarse_radius)..=first;
            for i in onset_indices.chain([first + 1, index.saturating_sub(1), index, index + 1]) {
                if let Some(frame) = self.frames.get(i) {
                    panels.insert(frame.second, frame);
                    times.push(frame.second);
                }
            }
            times.sort_unstable();
            times.dedup();
            let mut screen = Vec::new();
            for &time in &times {
                if time == c.seen
                    || self
                        .frames
                        .get(index + 1)
                        .is_some_and(|next| next.second == time)
                {
                    read_screen(
                        &mut self.words,
                        &mut self.ocr_available,
                        panels[&time],
                        self.analyzer.ocr_command,
                    );
                }
                if let Some(t) = self.words.get(&time) {
                    screen.push(format!(
                        "{time} {}",
                        clean(t).chars().take(190).collect::<String>()
                    ));
                }
            }
            let ocr = screen.join("\n");
            inputs.push(serde_json::json!({"id":id,"proposal":c.heading,"seen":c.seen,"times":times,"ocr":ocr}));
            leads.insert(id, (c, lead, times, ocr));
        }
        if inputs.is_empty() {
            return Ok(());
        }
        let prompt = format!(
            r#"作品『{}』版『{}』の元の画像。グループ内の seen は根拠を確認する秒、times はその周囲と導入候補を示す画像の整数ラベル。グループごとに、画像とOCRから独立に確認できた出来事だけを記載。候補が同じ出来事の続きなら1件にする。同じ秒に裏付けられる別の出来事は id ごとに区別する。新しい場所・章・依頼・目標・ボス戦・物語上の発見が何かを短く日本語で表す。個々の台詞、普通の移動、装備やロードの一覧、天候やエフェクトを節目として列挙しないこと。戦闘は敵や攻撃・決着が画面で確認できる場合だけ。文字が読めない地名や音は推測しないこと。
proposal は仮説。根拠秒にそれが本当に起きたかを画像で検証し、台詞で名前が言及されただけの登場や、敵も勝利表示もない戦闘、気持ち・台詞の要約だけなら除く。別の出来事に読み替えない。結果は {{"events":[{{"id":候補のid,"start":同じグループから選んだ導入の秒,"heading":"具体的な日本語見出し","basis":"写っている文字か動き"}}]}}。該当しなければ空配列。
{}"#,
            self.analyzer.game,
            self.analyzer.edition,
            serde_json::to_string(&inputs)?
        );
        let frames = panels.values().copied().collect::<Vec<_>>();
        let response = self.analyzer.model.infer(
            &prompt,
            &sheet::render(&frames, if group.len() == 1 { 2 } else { 4 }, 512)?,
        );
        let result: Confirmations = match response {
            Err(e) if group.len() > 1 && crate::model::needs_smaller_input(&e) => {
                let mid = group.len() / 2;
                self.confirm_group(&group[..mid])?;
                return self.confirm_group(&group[mid..]);
            }
            response => response?,
        };
        for confirmed in result.events {
            let Some((c, lead, times, ocr)) = leads.remove(&confirmed.id) else {
                continue;
            };
            let heading = clean(&confirmed.heading);
            let basis = clean(&confirmed.basis);
            let after_fade = self
                .signals
                .range(c.seen.saturating_sub(24)..c.seen)
                .rev()
                .find(|(_, signal)| signal.dark)
                .map_or(c.seen.saturating_sub(24), |(&sec, _)| sec);
            if !japanese(&heading)
                || basis.is_empty()
                || ongoing_speaker(&heading, c.seen, after_fade, &self.words)
            {
                continue;
            }
            let start = if times.contains(&confirmed.start) && confirmed.start <= c.seen {
                confirmed.start
            } else {
                lead
            };
            let earlier = c.update.filter(|&id| {
                self.events.get(id).is_some_and(|e| {
                    e.heading != "動画開始" && e.seen <= c.seen && c.seen - e.seen <= LOOKBACK
                })
            });
            let (start, start_image) = if let Some(id) = earlier {
                (self.events[id].start, self.events[id].start_image.clone())
            } else {
                (start, Some(save(self.analyzer.dir, panels[&start])?))
            };
            let event = Event {
                start,
                seen: c.seen,
                heading,
                basis,
                start_image,
                evidence_image: save(self.analyzer.dir, panels[&c.seen])?,
                screen_label: None,
                ocr,
            };
            if let Some(id) = earlier {
                self.events[id] = event;
            } else if let Some((_, anchor)) =
                self.events.iter_mut().enumerate().rev().find(|(id, e)| {
                    // A detail can refine the transition we have already seen.
                    // A label from the preceding shot is a separate event.
                    self.verified.contains(id)
                        && (e.start..=e.start.saturating_add(2)).contains(&event.start)
                        && (e.seen..=e.seen.saturating_add(3)).contains(&event.seen)
                        && ((e.heading == "プレイ再開"
                            && c.can_be_resumption()
                            && is_resume_heading(&event.heading))
                            || (matches!(c.kind, Some(Kind::Place)) && e.heading == "場所紹介")
                            || (matches!(c.kind, Some(Kind::Story))
                                && e.heading == "会話シーン開始"))
                })
            {
                if anchor.heading != "プレイ再開" {
                    *anchor = Event {
                        start: anchor.start,
                        start_image: anchor.start_image.take(),
                        ..event
                    };
                }
            } else if !self
                .events
                .iter()
                .any(|e| e.start == event.start && e.heading == event.heading)
            {
                self.events.push(event);
            }
        }
        Ok(())
    }
}

fn is_load_menu(text: &str) -> bool {
    let t = text.to_lowercase().replace(' ', "");
    [
        "loadgame",
        "つづきから",
        "続きから",
        "ゲームを再開",
        "セーブデータをロード",
    ]
    .iter()
    .any(|label| t.contains(label))
        || (t.contains("ロード") && t.contains("スロット"))
}

fn is_save_menu(text: &str) -> bool {
    let t = text.to_lowercase().replace(' ', "");
    (t.contains("セーブ") || t.contains("savegame"))
        && (t.contains("スロット") || t.contains("上書き") || t.matches("難易度").count() > 1)
}

// A speaker whose name plate was already visible in the same exchange
// has not just been introduced by the next line of dialogue.
fn ongoing_speaker(heading: &str, seen: u64, since: u64, words: &BTreeMap<u64, String>) -> bool {
    let Some((name, _)) = heading
        .split_once("の登場")
        .or_else(|| heading.split_once("が登場"))
    else {
        return false;
    };
    let name_on_screen = |text: &String| text.lines().any(|line| line.trim() == name);
    let until = seen.saturating_sub(3);
    since < until
        && words.get(&seen).is_some_and(name_on_screen)
        && words
            .range(since..=until)
            .any(|(_, text)| name_on_screen(text))
}

fn is_resume_heading(heading: &str) -> bool {
    matches!(
        heading.trim(),
        "再開"
            | "プレイ再開"
            | "プレイの再開"
            | "プレイを再開"
            | "ゲーム再開"
            | "ゲームの再開"
            | "ゲームを再開"
            | "セーブデータから再開"
            | "続きから再開"
            | "プレイ開始"
            | "ゲーム開始"
            | "ロード完了"
            | "ロードして開始"
    )
}

impl Candidate {
    fn can_be_resumption(&self) -> bool {
        matches!(self.kind, None | Some(Kind::Story | Kind::Progress))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinguishes_a_save_list_and_an_ongoing_speaker_from_story_events() {
        assert!(is_save_menu(
            "セーブ\nスロット2\nこのデータを上書きしますか"
        ));
        assert!(!is_save_menu("セーブポイントを発見した"));
        let captions = BTreeMap::from([
            (4, "旅商人\nおはよう".to_owned()),
            (12, "旅商人\n南の国へ行こう".to_owned()),
        ]);
        assert!(ongoing_speaker("旅商人の登場", 12, 0, &captions));
        assert!(!ongoing_speaker("旅商人の登場", 12, 8, &captions));
        assert!(!ongoing_speaker("国王の登場", 12, 0, &captions));
    }
}
