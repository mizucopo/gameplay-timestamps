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
struct Candidates {
    events: Vec<Candidate>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct Candidate {
    start: u64,
    seen: u64,
    heading: String,
    basis: String,
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
struct Confirmations {
    events: Vec<Confirmation>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct Confirmation {
    seen: u64,
    start: u64,
    heading: String,
    basis: String,
}

#[derive(Deserialize, Serialize)]
pub struct Event {
    pub start: u64,
    pub seen: u64,
    pub heading: String,
    pub basis: String,
    pub start_image: String,
    pub evidence_image: String,
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

struct Run<'a> {
    analyzer: &'a Analyzer<'a>,
    frames: VecDeque<Frame>,
    events: Vec<Event>,
    pending: usize,
    signals: BTreeMap<u64, scene::Signal>,
    words: BTreeMap<u64, String>,
    ocr_available: bool,
}

impl Analyzer<'_> {
    pub fn run(&self, video: &Path, step: u32) -> Result<Vec<Event>> {
        fs::create_dir(self.dir.join("frames"))?;
        let mut run = Run {
            analyzer: self,
            frames: VecDeque::new(),
            events: Vec::new(),
            pending: 0,
            signals: BTreeMap::new(),
            words: BTreeMap::new(),
            ocr_available: self.use_ocr,
        };
        crate::media::scan(video, step, |frame| {
            if run.frames.is_empty() {
                let path = save(self.dir, &frame)?;
                run.events.push(Event {
                    start: 0,
                    seen: frame.second,
                    heading: "動画開始".to_owned(),
                    basis: "録画の先頭".to_owned(),
                    start_image: path.clone(),
                    evidence_image: path,
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
        run.events.sort_by_key(|event| event.start);
        Ok(run.events)
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
        let all: Vec<_> = self.frames.iter().skip(first.saturating_sub(2)).collect();
        let panels = scene::panels(&all, &self.signals);
        let a = self.analyzer;
        let mut screen = String::new();
        let text_seconds: BTreeSet<_> = all
            .iter()
            .step_by(5)
            .chain(panels.iter())
            .map(|frame| frame.second)
            .collect();
        for frame in all.iter().filter(|f| text_seconds.contains(&f.second)) {
            read_screen(
                &mut self.words,
                &mut self.ocr_available,
                frame,
                a.ocr_command,
            );
            if let Some(text) = self.words.get(&frame.second) {
                writeln!(
                    screen,
                    "{} {}",
                    frame.second,
                    clean(text).chars().take(130).collect::<String>()
                )?;
            }
        }
        let recent: Vec<_> = self
            .events
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, e)| e.seen >= from.saturating_sub(LOOKBACK))
            .take(20)
            .map(|(id, e)| serde_json::json!({"id":id,"start":e.start,"heading":e.heading}))
            .collect();
        let labels: Vec<_> = panels.iter().map(|f| f.second).collect();
        let prompt = format!(
            r#"作品『{}』版『{}』の動画を編集します。順に並ぶコマの再生秒は {labels:?}。各コマの黒い帯の整数も同じ秒です。新規対象 {from}〜{until} 秒。
画面から確かめられる、後からここへ戻る意味のある出来事の導入を拾ってください。タイトル/ロード後のプレイ再開、場所の紹介、章、名前を伴う目標や更新、依頼、報酬、ボス登場・決着、物語上の会話や演出を区別します。短い間隔でも別の出来事は残します。通常の移動、UIの点滅、会話の各台詞、単に画面が明るくなる様子から出来事を作らないでください。ゲーム内時計を開始秒に使わないこと。
台詞を1文ずつ列挙せず、物語の会話なら何を知ったかを1つにまとめてください。ロード一覧・装備選択を除き、目的地・新しい目標・物語の節目などの変化を優先します。継続中なら空配列です。
同じ場所でも依頼の再開や違う場面なら別件。すでに掲載した場面 {}。後から同じ依頼の名前等が分かった場合だけ update にその行の id を指定。それ以外は null。
画面のOCR（時刻付き、画像の間のサンプルも含む。誤読あり、メニューの一覧を達成と混同しないこと）:
{screen}
JSON {{"events":[{{"start":整数,"seen":整数,"heading":"短い日本語見出し","basis":"根拠を短く","kind":"story/place/quest/boss/reward/progress のいずれか","lead_in":false,"update":null}}]}}。時刻は画像またはOCRの再生秒から選ぶ。seenは新規対象内の根拠コマ。依頼・ボスなど後から表示された名前の導入を探す場合は lead_in を true とする。該当なしなら空配列。"#,
            a.game,
            a.edition,
            serde_json::to_string(&recent)?
        );
        let overview = sheet::render(&panels, 4, 384)?;
        let result: Candidates = a
            .model
            .infer(&prompt, &overview)
            .with_context(|| format!("{from}〜{until} 秒の候補抽出"))?;
        self.resume_after_menu(from, until)?;
        let mut candidates: Vec<_> = result
            .events
            .into_iter()
            .filter(|c| (from..=until).contains(&c.seen))
            .collect();
        candidates.retain(|c| !self.words.get(&c.seen).is_some_and(|t| is_load_menu(t)));
        self.cutscene_entries(from, until, &mut candidates);
        candidates.sort_by_key(|c| c.seen);
        for group in candidates.chunks(4) {
            self.confirm_group(group)?;
        }
        if from / 300 != (until + 1) / 300 || from == 0 {
            eprintln!("走査済み {until} 秒 / 見出し {} 件", self.events.len());
        }
        self.pending = 0;
        while self
            .frames
            .front()
            .is_some_and(|f| f.second < until.saturating_sub(LOOKBACK))
        {
            let second = self.frames.pop_front().unwrap().second;
            self.signals.remove(&second);
            self.words.remove(&second);
        }
        Ok(())
    }

    fn resume_after_menu(&mut self, from: u64, until: u64) -> Result<()> {
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
            let path = save(self.analyzer.dir, frame)?;
            self.events.push(Event {
                start: frame.second,
                seen: frame.second,
                heading: "プレイ再開".into(),
                basis: format!("{menu_second} 秒のロード画面から暗転を挟んで操作画面に復帰"),
                start_image: path.clone(),
                evidence_image: path,
                ocr: clean(screen[0].1),
            });
        }
        Ok(())
    }

    // A field HUD disappearing into a skippable story sequence gives a
    // visual onset even when a location card's stylized lettering defeats OCR.
    fn cutscene_entries(&self, from: u64, until: u64, candidates: &mut Vec<Candidate>) {
        let mut last_field = None;
        for (&second, text) in &self.words {
            let t = text.to_lowercase().replace(' ', "");
            if ["ミニマップ", "マップ", "経路", "minimap"]
                .iter()
                .any(|w| t.contains(w))
            {
                last_field = Some(second);
            }
            if !t.contains("スキップ") && !t.contains("skip") {
                continue;
            }
            let Some(field) = last_field.take() else {
                continue;
            };
            if !(from..=until).contains(&second) || second.saturating_sub(field) > 15 {
                continue;
            }
            let samples: Vec<_> = self
                .signals
                .range(field.saturating_sub(1)..=second)
                .collect();
            // The first darkening in a fade is the transition's start.
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
            if candidates.iter().any(|c| c.seen == second) {
                continue;
            }
            candidates.push(Candidate {
                start,
                seen: second,
                heading: "イベントシーンの導入".into(),
                basis: "探索画面から演出に移行し、スキップ操作が表示された".into(),
                kind: Some(Kind::Story),
                lead_in: false,
                update: None,
            });
        }
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
            "作品『{}』。最後の{}秒の画像の出来事「{}」を確かめます。{}。同じ人物・依頼・行動の導入が過去のコマに写っていれば、その再生秒。別の出来事や同じ場所での別件、単に似た画面を選ばない。特定できなければ {}。JSON {{\"start\":整数}}。",
            self.analyzer.game, proof.second, candidate.heading, candidate.basis, initial
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
        for c in group {
            let Some(index) = self.frames.iter().position(|f| f.second == c.seen) else {
                continue;
            };
            let initial = self
                .frames
                .iter()
                .find(|f| f.second == c.start && f.second <= c.seen)
                .map_or(c.seen, |f| f.second);
            let lead = self.find_lead(c, index, initial)?;
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
            inputs.push(serde_json::json!({"seen":c.seen,"times":times,"ocr":ocr}));
            leads.insert(c.seen, (c, lead, times, ocr));
        }
        if inputs.is_empty() {
            return Ok(());
        }
        let prompt = format!(
            r#"作品『{}』版『{}』の元の画像。グループ内の seen は根拠を確認する秒、times はその周囲と導入候補を示す画像の整数ラベル。グループごとに、画像とOCRから独立に確認できた出来事だけを記載。候補が同じ出来事の続きなら1件にする。新しい場所・章・依頼・目標・ボス戦・物語上の発見が何かを短く日本語で表す。個々の台詞、普通の移動、装備やロードの一覧、天候やエフェクトを節目として列挙しないこと。戦闘は敵や攻撃・決着が画面で確認できる場合だけ。文字が読めない地名や音は推測しないこと。
結果は {{"events":[{{"seen":根拠秒,"start":同じグループから選んだ導入の秒,"heading":"具体的な日本語見出し","basis":"実際に写った文字か動き"}}]}}。該当しないグループは出力から除く。
{}"#,
            self.analyzer.game,
            self.analyzer.edition,
            serde_json::to_string(&inputs)?
        );
        let frames = panels.values().copied().collect::<Vec<_>>();
        let result: Confirmations = self.analyzer.model.infer(
            &prompt,
            &sheet::render(&frames, if group.len() == 1 { 2 } else { 4 }, 512)?,
        )?;
        for confirmed in result.events {
            let Some((c, lead, times, ocr)) = leads.remove(&confirmed.seen) else {
                continue;
            };
            let heading = clean(&confirmed.heading);
            let basis = clean(&confirmed.basis);
            if !japanese(&heading) || basis.is_empty() {
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
                (start, save(self.analyzer.dir, panels[&start])?)
            };
            let event = Event {
                start,
                seen: c.seen,
                heading,
                basis,
                start_image,
                evidence_image: save(self.analyzer.dir, panels[&c.seen])?,
                ocr,
            };
            if let Some(id) = earlier {
                self.events[id] = event;
            } else if event.start == 0 && self.events[0].heading == "動画開始" {
                self.events[0] = event;
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
