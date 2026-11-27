//! Relate a named opponent's persistent HUD to the beginning of its encounter.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::sync::LazyLock;

use anyhow::Result;
use image::ImageFormat;
use regex::Regex;
use serde::Deserialize;

use crate::{
    analyze::{Analyzer, Event, save},
    media::Frame,
    scene::Signal,
    sheet,
};

#[derive(Default)]
pub struct Tracker {
    intervals: Vec<Encounter>,
    checked: BTreeSet<(String, u64)>,
    pending: Vec<Event>,
}

struct Encounter {
    name: String,
    first: u64,
    last: u64,
    start: Option<u64>,
}

fn compact(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn valid_name(s: &str) -> bool {
    (2..=36).contains(&s.chars().count())
        && s.chars().any(char::is_alphabetic)
        && s.chars()
            .all(|c| c.is_alphanumeric() || "・ー々ヶ-".contains(c))
        && ![
            "敵",
            "敵側",
            "ボス",
            "主人公",
            "プレイヤー",
            "装備",
            "ガード",
            "マップ",
        ]
        .contains(&s)
}

fn battle_control(text: &str) -> bool {
    [
        "敵HP",
        "ボス:",
        "ボス：",
        "BOSS",
        "ロックオン",
        "たたかう",
        "ジャストガード",
        "攻撃を回避",
        "ブレイク",
        "戦闘開始",
    ]
    .iter()
    .any(|s| text.contains(s))
}

// Only discard a repeated attack whose *entire* visual evidence describes
// the ordinary attack, receiving it, or its damage. An extra clause, named
// technique, direction or different action stays visible in the timeline.
static ORDINARY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)^
    (?:
       (?:画面に)?(?:敵|相手|ボス|魔物)?(?:が|の|による)?(?:通常)?攻撃
         (?:を(?:開始|仕掛け|行|繰り出)|が(?:画面に)?(?:表示|開始|命中|見え)|する|し)
      |(?:[\p{Han}\p{Katakana}ー・]{1,24}(?:が|は))?(?:その|敵の|ボスの)?
        (?:攻撃|ダメージ)を(?:受け|受けた|くらう)
      |(?:[\p{Han}\p{Katakana}ー・]{1,24}に)?ダメージを与え
    )
    (?:され|られ|える|る|られる|す|い|う|し|て|た|いる|様子|のが|が|を|確認|できる|見える)*$
    ",
    )
    .unwrap()
});

fn ordinary_evidence(e: &Event, nameplate: &str, name: &str) -> bool {
    let text = compact(&e.basis)
        .replace(nameplate, "敵")
        .replace(name, "敵")
        .replace("敵「敵」", "敵")
        .replace("ボス「敵」", "ボス");
    let mut clauses = text
        .split(['、', '。', ',', '.', '\n'])
        .filter(|l| !l.is_empty())
        .peekable();
    clauses.peek().is_some() && clauses.all(|s| ORDINARY.is_match(s))
}

fn generic_attack(heading: &str) -> Option<&str> {
    ["の通常攻撃", "の攻撃", "が攻撃"]
        .iter()
        .find_map(|suffix| {
            let name = heading
                .strip_suffix(suffix)?
                .trim_matches(['「', '」', '『', '』']);
            valid_name(name).then_some(name)
        })
}

fn name_matches(plate: &str, named: &str) -> bool {
    plate == named || (named.chars().count() >= 3 && plate.ends_with(named))
}

fn battle_start(heading: &str) -> Option<&str> {
    [
        "との戦闘開始",
        "との戦いが始まる",
        "との戦いの開始",
        "との戦い",
        "との決戦",
        "戦の開始",
        "戦開始",
    ]
    .iter()
    .find_map(|suffix| {
        let name = heading
            .strip_suffix(suffix)?
            .trim_matches(['「', '」', '『', '』']);
        valid_name(name).then_some(name)
    })
}

fn reset(text: &str) -> bool {
    [
        "ゲームオーバー",
        "全滅",
        "再挑戦",
        "再戦",
        "戦闘終了",
        "戦闘結果",
        "戦闘勝利",
        "討伐成功",
        "リザルト",
    ]
    .iter()
    .any(|s| text.contains(s))
}

fn result_event(e: &Event) -> bool {
    reset(&e.heading)
        || [
            "形態変化",
            "形態移行",
            "に変身",
            "を撃破",
            "に勝利",
            "戦に敗北",
            "との決着",
            "プレイ再開",
        ]
        .iter()
        .any(|s| e.heading.contains(s))
}

fn selection_or_dialogue(text: &str) -> bool {
    [
        "図鑑",
        "スロット",
        "装備一覧",
        "装備選択",
        "上書き",
        "ステータス画面",
    ]
    .iter()
    .any(|s| text.contains(s))
        || (text.contains("閉じる") && (text.contains("武器") || text.contains("装備")))
        || ((text.contains("スキップ") || text.contains("オート")) && !text.contains("を倒"))
}

fn title_lines(text: &str) -> impl Iterator<Item = String> + '_ {
    text.lines().filter_map(|line| {
        let line = compact(line);
        let line = line
            .strip_prefix("ボス：")
            .or_else(|| line.strip_prefix("BOSS:"))
            .unwrap_or(&line);
        valid_name(line).then(|| line.to_owned())
    })
}

// Only bridge a minor OCR fluctuation after an exact reading has established
// the name. Numbers, directions and stage words are part of the identity.
fn alternate(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    if a.chars().count() < 6 || a.chars().count() != b.chars().count() {
        return false;
    }
    let mut diff = a.chars().zip(b.chars()).filter(|(x, y)| x != y);
    let Some((x, y)) = diff.next() else {
        return true;
    };
    let distinct = |c: char| {
        c.is_numeric()
            || "東西南北上下左右前後真新改赤青白黒金銀炎氷雷闇光一二三四五六七八九十".contains(c)
    };
    !distinct(x) && !distinct(y) && diff.next().is_none()
}

// A horizontal meter has parallel, separated edges with contrast across a
// long span of the HUD. Match its position in two frames, independent of
// fill color, opponent, UI font, resolution or game.
fn bars(frame: &Frame) -> Result<BTreeSet<u32>> {
    let im = image::load_from_memory_with_format(&frame.jpeg, ImageFormat::Jpeg)?
        .thumbnail_exact(384, 216)
        .into_rgb8();
    let mut edges = BTreeSet::new();
    for y in (3..68).chain(145..212) {
        let changes = (96..288)
            .filter(|&x| {
                let a = im.get_pixel(x, y - 1);
                let b = im.get_pixel(x, y + 1);
                (0..3).any(|i| a[i].abs_diff(b[i]) > 30)
            })
            .count();
        if changes >= 88 {
            edges.insert(y);
        }
    }
    Ok(edges
        .iter()
        .copied()
        .filter(|&y| (3..10).any(|d| edges.contains(&(y + d))))
        .collect())
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum Phase {
    Intro,
    Hud,
    Ongoing,
    Other,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Check {
    phase: Phase,
}

fn observations(
    name: &str,
    words: &BTreeMap<u64, String>,
    boundaries: &BTreeSet<u64>,
) -> Vec<Vec<(u64, bool)>> {
    let mut groups = Vec::new();
    let mut times = Vec::new();
    let finish = |times: &mut Vec<(u64, bool)>, out: &mut Vec<Vec<(u64, bool)>>| {
        if times.len() >= 2 {
            out.push(std::mem::take(times));
        } else {
            times.clear();
        }
    };
    for (&time, text) in words {
        if reset(text) {
            finish(&mut times, &mut groups);
            continue;
        }
        if selection_or_dialogue(text) {
            continue;
        }
        let labels = title_lines(text).collect::<Vec<_>>();
        let exact = labels.iter().any(|l| l == name);
        if !exact && !labels.iter().any(|l| alternate(l, name)) {
            continue;
        }
        if let Some(&(prev, _)) = times.last()
            && (time - prev > 12 || boundaries.range(prev + 1..=time).next().is_some())
        {
            finish(&mut times, &mut groups);
        }
        times.push((time, exact));
    }
    finish(&mut times, &mut groups);
    groups
}

struct Window<'a> {
    frames: &'a VecDeque<Frame>,
    words: &'a BTreeMap<u64, String>,
    signals: &'a BTreeMap<u64, Signal>,
    events: &'a [Event],
    from: u64,
}

impl Tracker {
    pub fn observe(
        &mut self,
        frames: &VecDeque<Frame>,
        words: &BTreeMap<u64, String>,
        signals: &BTreeMap<u64, Signal>,
        events: &[Event],
        from: u64,
        analyzer: &Analyzer<'_>,
    ) -> Result<()> {
        self.inspect(
            Window {
                frames,
                words,
                signals,
                events,
                from,
            },
            analyzer.dir,
            |prompt, image| Ok(analyzer.model.infer::<Check>(prompt, image)?.phase),
        )
    }

    fn inspect(
        &mut self,
        window: Window<'_>,
        dir: &Path,
        mut verify: impl FnMut(&str, &[u8]) -> Result<Phase>,
    ) -> Result<()> {
        let Window {
            frames,
            words,
            signals,
            events,
            from,
        } = window;
        let mut names = events
            .iter()
            .filter(|e| frames.front().is_some_and(|f| e.seen >= f.second))
            .filter_map(|e| generic_attack(&e.heading).or_else(|| battle_start(&e.heading)))
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        // Enemy objectives can nominate an opponent even if the overview
        // produced no attack heading. Only a separate on-screen name can match.
        let targets = words
            .values()
            .flat_map(|text| text.lines())
            .filter_map(|line| {
                let line = compact(line);
                let (target, _) = line.split_once("を倒")?;
                let target = target.rsplit([':', '：', '▶']).next()?;
                valid_name(target).then(|| target.to_owned())
            })
            .chain(names.iter().cloned())
            .collect::<BTreeSet<_>>();
        if !targets.is_empty() {
            for label in words.values().flat_map(|s| title_lines(s)) {
                if targets.iter().any(|target| name_matches(&label, target))
                    && !names.iter().any(|n| alternate(n, &label))
                {
                    names.insert(label);
                }
            }
        }
        for text in words
            .values()
            .filter(|s| battle_control(s) && !selection_or_dialogue(s))
        {
            for label in title_lines(text).filter(|s| {
                s.chars().count() >= 3 && !battle_control(s) && !s.contains(['を', '！', '？'])
            }) {
                if !names.iter().any(|n| alternate(n, &label)) {
                    names.insert(label);
                }
            }
        }
        let boundaries = events
            .iter()
            .filter(|e| result_event(e))
            .map(|e| e.start)
            .collect();
        let mut meters = BTreeMap::new();
        for name in names {
            for samples in observations(&name, words, &boundaries) {
                let last = samples.last().unwrap().0;
                let earliest = samples[0].0;
                if last < from {
                    continue;
                }
                if let Some(prior) = self
                    .intervals
                    .iter_mut()
                    .find(|b| b.name == name && b.first <= last && earliest <= b.last)
                {
                    prior.last = prior.last.max(last);
                    continue;
                }
                let get = |sec| frames.iter().find(|f| f.second == sec);
                let mut confirmed = None;
                let exact = samples
                    .iter()
                    .filter_map(|&(s, exact)| exact.then_some(s))
                    .collect::<Vec<_>>();
                for pair in exact.windows(2).filter(|pair| pair[1] - pair[0] <= 8) {
                    let (Some(a), Some(b)) = (get(pair[0]), get(pair[1])) else {
                        continue;
                    };
                    for frame in [a, b] {
                        if let std::collections::btree_map::Entry::Vacant(entry) =
                            meters.entry(frame.second)
                        {
                            entry.insert(bars(frame)?);
                        }
                    }
                    if !meters[&a.second].is_disjoint(&meters[&b.second]) {
                        confirmed = Some((a, b));
                        break;
                    }
                }
                let Some((a, b)) = confirmed else { continue };
                if !self.checked.insert((name.clone(), a.second)) {
                    continue;
                }
                // The last dark-to-field transition before the nameplate can
                // already introduce the encounter. Do not backtrack through
                // an intervening dialogue or menu into its earlier scene.
                let history = frames
                    .iter()
                    .filter(|f| f.second >= a.second.saturating_sub(10) && f.second <= a.second)
                    .collect::<Vec<_>>();
                let mut at = a;
                for pair in history.windows(2).rev() {
                    let (prev, next) = (&signals[&pair[0].second], &signals[&pair[1].second]);
                    if (prev.dark || prev.mean < 20) && next.mean > prev.mean + 24 {
                        at = pair[1];
                        break;
                    }
                }
                let before = history
                    .iter()
                    .copied()
                    .filter(|f| f.second < at.second)
                    .rev()
                    .take(3);
                let mut panels: BTreeMap<u64, &Frame> =
                    before.chain([at, a, b]).map(|f| (f.second, f)).collect();
                if let Some(next) = frames
                    .iter()
                    .find(|f| at.second < f.second && f.second < a.second)
                {
                    panels.insert(next.second, next);
                }
                let prompt = format!(
                    "再生秒付きのゲーム画像。{}秒と{}秒で『{}』が敵の名称として同じ長い体力ゲージに付き、実際の対決が画面で確認できるかを点検する。名前が会話・移動目標・装備/敵図鑑に出ただけならother。{}秒が暗転後に戦場・敵を見せ始め、そのまま同じ対決につながるならintro（操作やゲージが数秒後でもよい）。その秒は無関係だが後のゲージ出現で新戦闘の開始が確認できるならhud。既に同じ戦闘中でメニューから戻っただけ・開始の前が見えないならongoing。判断不能ならother。JSON {{\"phase\":\"intro|hud|ongoing|other\"}} だけ。",
                    a.second, b.second, name, at.second
                );
                let phase = verify(
                    &prompt,
                    &sheet::render(&panels.into_values().collect::<Vec<_>>(), 2, 560)?,
                )?;
                if matches!(phase, Phase::Other) {
                    continue;
                }
                let has_before = history.iter().any(|f| f.second < at.second);
                let start = match phase {
                    Phase::Intro if has_before => Some(at.second),
                    Phase::Hud if history.len() > 1 => Some(a.second),
                    _ => None,
                };
                if let Some(second) = start {
                    self.pending.push(Event{
                        start:second,seen:a.second,heading:format!("{name}戦開始"),
                        basis:format!("{second}秒から戦闘場面に切り替わり、{}秒と{}秒の敵名・体力ゲージを照合",a.second,b.second),
                        start_image:Some(save(dir,get(second).expect("selected battle frame"))?),
                        evidence_image:save(dir,a)?,screen_label:Some(name.clone()),
                        ocr:format!("{} {name}\n{} {name}",a.second,b.second),
                    });
                }
                self.intervals.push(Encounter {
                    name: name.clone(),
                    first: a.second,
                    last,
                    start,
                });
            }
        }
        Ok(())
    }

    pub fn consolidate(self, events: &mut Vec<Event>) {
        events.extend(self.pending);
        events.sort_by_key(|e| e.start);
        let mut represented = BTreeSet::new();
        events.retain(|e| {
            if result_event(e) {
                represented.clear();
            }
            if e.start == 0 || e.screen_label.is_some() {
                return true;
            }
            if let Some(name) = battle_start(&e.heading)
                && self
                    .intervals
                    .iter()
                    .filter(|b| {
                        name_matches(&b.name, name)
                            && b.start
                                .is_some_and(|s| s <= e.start && e.start <= b.first + 3)
                    })
                    .count()
                    == 1
            {
                return false;
            }
            let Some(name) = generic_attack(&e.heading) else {
                return true;
            };
            // An overview or dictionary may omit the epithet printed on the
            // HUD. Link that short name only to a unique simultaneous opponent.
            let mut matching = self.intervals.iter().enumerate().filter(|(_, b)| {
                name_matches(&b.name, name) && b.first <= e.seen && e.seen <= b.last
            });
            let Some((id, encounter)) = matching.next() else {
                return true;
            };
            if matching.next().is_some() || !ordinary_evidence(e, &encounter.name, name) {
                return true;
            }
            represented.insert(id)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage, codecs::jpeg::JpegEncoder};

    const OPPONENT: &str = "霧纏う琥珀竜";

    fn event(time: u64, heading: &str, basis: &str) -> Event {
        Event {
            start: time,
            seen: time,
            heading: heading.into(),
            basis: basis.into(),
            start_image: None,
            evidence_image: String::new(),
            screen_label: None,
            ocr: String::new(),
        }
    }

    fn attack(time: u64, name: &str, basis: &str) -> Event {
        event(time, &format!("{name}の攻撃"), basis)
    }

    fn footage(meter: bool) -> (VecDeque<Frame>, BTreeMap<u64, Signal>) {
        let mut frames = VecDeque::new();
        let mut signals = BTreeMap::new();
        for second in 0..=24 {
            let mut img = RgbImage::from_pixel(
                384,
                216,
                Rgb(if second == 6 {
                    [0, 0, 0]
                } else {
                    [70, 82, 100]
                }),
            );
            if meter && second >= 9 {
                for y in 24..31 {
                    for x in 64..320 {
                        img.put_pixel(x, y, Rgb([210, 160, 40]));
                    }
                }
            }
            let mut jpeg = Vec::new();
            JpegEncoder::new_with_quality(&mut jpeg, 92)
                .encode_image(&img)
                .unwrap();
            let frame = Frame { second, jpeg };
            signals.insert(
                second,
                Signal::measure(&frame, signals.last_key_value().map(|(_, v)| v)).unwrap(),
            );
            frames.push_back(frame);
        }
        (frames, signals)
    }

    #[test]
    fn encounter_intro_advances_to_the_field_and_folds_only_ordinary_attacks() {
        let (frames, signals) = footage(true);
        let goal = format!("{OPPONENT}を倒す");
        let mut words = BTreeMap::from([(3, format!("スキップ\n{OPPONENT}")), (5, goal.clone())]);
        for t in 9..=24 {
            words.insert(t, format!("{OPPONENT}\n{goal}"));
        }
        let mut events = vec![
            event(0, "動画開始", ""),
            attack(
                13,
                "琥珀竜",
                "敵の攻撃が画面に表示され、ルナがその攻撃を受ける様子が確認できる。",
            ),
            attack(
                14,
                "琥珀竜",
                &format!("敵「{OPPONENT}」が攻撃を開始し、キャラクターにダメージを与える。"),
            ),
            event(15, "足場が凍結", "床に新しく氷が広がる"),
            attack(18, OPPONENT, "翼を畳んで尾で床の左端を払う"),
            event(20, "氷の光線", "氷の光線が画面を横切る"),
        ];
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("frames")).unwrap();
        let mut tracker = Tracker::default();
        let mut calls = 0;
        for from in [0, 15] {
            tracker
                .inspect(
                    Window {
                        frames: &frames,
                        words: &words,
                        signals: &signals,
                        events: &events,
                        from,
                    },
                    temp.path(),
                    |prompt, image| {
                        assert!(prompt.contains(OPPONENT));
                        assert!(image.starts_with(&[0xff, 0xd8]));
                        calls += 1;
                        Ok(Phase::Intro)
                    },
                )
                .unwrap();
        }
        assert_eq!(calls, 1);
        tracker.consolidate(&mut events);
        assert_eq!(
            events.iter().map(|e| e.start).collect::<Vec<_>>(),
            [0, 7, 13, 15, 18, 20]
        );
        let start = &events[1];
        assert_eq!(start.seen, 9);
        assert_eq!(start.screen_label.as_deref(), Some(OPPONENT));
        assert!(start.ocr.contains(&format!("10 {OPPONENT}")));
        assert_eq!(
            std::fs::read(temp.path().join(start.start_image.as_ref().unwrap())).unwrap(),
            frames[7].jpeg
        );
        assert_eq!(
            std::fs::read(temp.path().join(&start.evidence_image)).unwrap(),
            frames[9].jpeg
        );
    }

    #[test]
    fn a_mentioned_name_menu_or_single_meter_is_insufficient() {
        let (mut frames, signals) = footage(false);
        let mut words = BTreeMap::from([
            (2, format!("スキップ\n{OPPONENT}")),
            (3, format!("図鑑\n{OPPONENT}")),
        ]);
        let events = vec![attack(16, OPPONENT, "敵が攻撃を開始した。")];
        let with_meter = footage(true).0;
        frames[16].jpeg.clone_from(&with_meter[16].jpeg);
        for t in 14..=17 {
            words.insert(t, OPPONENT.into());
        }
        let mut tracker = Tracker::default();
        tracker
            .inspect(
                Window {
                    frames: &frames,
                    words: &words,
                    signals: &signals,
                    events: &events,
                    from: 0,
                },
                Path::new("."),
                |_, _| panic!("no shared meter"),
            )
            .unwrap();
        assert!(tracker.intervals.is_empty());
        let menus = BTreeMap::from([
            (14, format!("図鑑\n{OPPONENT}")),
            (15, format!("{OPPONENT}\nスキップ")),
            (16, format!("装備一覧\n{OPPONENT}")),
        ]);
        tracker
            .inspect(
                Window {
                    frames: &with_meter,
                    words: &menus,
                    signals: &signals,
                    events: &events,
                    from: 0,
                },
                Path::new("."),
                |_, _| panic!("menu and dialogue name"),
            )
            .unwrap();
        // A matching graphic such as a pictured health meter in a cutscene
        // still needs direct visual confirmation of a real fight.
        let mut checks = 0;
        tracker
            .inspect(
                Window {
                    frames: &with_meter,
                    words: &words,
                    signals: &signals,
                    events: &events,
                    from: 0,
                },
                Path::new("."),
                |_, _| {
                    checks += 1;
                    Ok(Phase::Other)
                },
            )
            .unwrap();
        assert_eq!(checks, 1);
        assert!(tracker.intervals.is_empty() && tracker.pending.is_empty());
    }

    #[test]
    fn simultaneous_opponents_sharing_a_name_do_not_absorb_each_others_attacks() {
        let tracker = Tracker {
            intervals: [OPPONENT, "赤い琥珀竜"]
                .into_iter()
                .map(|n| Encounter {
                    name: n.into(),
                    first: 10,
                    last: 25,
                    start: Some(9),
                })
                .collect(),
            ..Tracker::default()
        };
        let mut events = vec![
            attack(12, "琥珀竜", "敵が攻撃を開始した。"),
            attack(13, "琥珀竜", "敵が攻撃を開始した。"),
            attack(15, OPPONENT, "敵が攻撃を開始した。"),
            attack(16, OPPONENT, "敵が攻撃を開始した。"),
            attack(17, "赤い琥珀竜", "敵が攻撃を開始した。"),
        ];
        tracker.consolidate(&mut events);
        assert_eq!(
            events.iter().map(|e| e.start).collect::<Vec<_>>(),
            [12, 13, 15, 17]
        );
    }

    #[test]
    fn ongoing_fight_and_retry_preserve_separate_attacks_forms_and_outcomes() {
        let (frames, signals) = footage(true);
        let words = (9..=24)
            .map(|t| (t, if t == 15 { "再挑戦" } else { OPPONENT }.to_owned()))
            .collect();
        let mut events = vec![
            attack(10, OPPONENT, "敵が攻撃を開始した。"),
            attack(11, OPPONENT, "敵が攻撃を開始した。"),
            event(15, "再挑戦", "敵に敗れて再挑戦を選ぶ"),
            attack(17, OPPONENT, "敵が攻撃を開始した。"),
            attack(18, OPPONENT, "敵が攻撃を開始した。"),
            event(19, "第二形態に変身", "外見が変化した"),
            attack(21, "真・霧纏う琥珀竜", "敵が攻撃を開始した。"),
            event(24, "琥珀竜を撃破", "結果が表示された"),
        ];
        let mut tracker = Tracker::default();
        let mut calls = 0;
        tracker
            .inspect(
                Window {
                    frames: &frames,
                    words: &words,
                    signals: &signals,
                    events: &events,
                    from: 0,
                },
                Path::new("."),
                |_, _| {
                    calls += 1;
                    Ok(Phase::Ongoing)
                },
            )
            .unwrap();
        assert!(calls >= 2);
        tracker.consolidate(&mut events);
        assert_eq!(
            events.iter().map(|e| e.start).collect::<Vec<_>>(),
            [10, 15, 17, 19, 21, 24]
        );
        assert!(events.iter().all(|e| battle_start(&e.heading).is_none()));
    }
}
