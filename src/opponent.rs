//! Check unverified enemy identities against their original evidence picture,
//! after captions and the dictionary have had their last opportunity to name them.
mod encounter;

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::LazyLock;

use anyhow::Result;
use regex::Regex;
use serde::Deserialize;

use crate::{
    analyze::{Analyzer, Event},
    dictionary::{Dictionary, Evidence},
    media::Cue,
};

#[derive(Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Subject {
    BossHud,
    EnemyHud,
    Combat,
    Result,
    Item,
    Equipment,
    Character,
    Dialogue,
    Other,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Action {
    Entrance,
    Attack,
    Guard,
    Form,
    Victory,
    Defeat,
    #[default]
    Ongoing,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Side {
    Player,
    Opponent,
    #[default]
    Unknown,
}

impl Action {
    fn claim(heading: &str) -> Self {
        [
            (Self::Guard, &["ガード", "防御"][..]),
            (Self::Form, &["形態", "に変身"][..]),
            (
                Self::Victory,
                &["勝利", "撃破", "討伐成功", "を倒した", "を倒す"][..],
            ),
            (Self::Defeat, &["敗北", "全滅", "ゲームオーバー"][..]),
            (Self::Attack, &["攻撃", "反撃"][..]),
            (
                Self::Entrance,
                &["開始", "登場", "出現", "現れ", "襲来", "遭遇"][..],
            ),
        ]
        .into_iter()
        .find(|(_, words)| words.iter().any(|w| heading.contains(w)))
        .map_or(Self::Ongoing, |(action, _)| action)
    }

    fn heading(
        self,
        name: Option<&str>,
        boss: bool,
        side: Side,
        player: &str,
        guard_success: bool,
    ) -> String {
        let actor = name.map(|n| {
            if boss {
                format!("ボス「{n}」")
            } else {
                format!("「{n}」")
            }
        });
        let other = actor.as_deref();
        let actor = other.unwrap_or("敵");
        let guard = if guard_success {
            "ガード成功"
        } else {
            "ガード"
        };
        match self {
            Self::Entrance => format!("{actor}との戦闘開始"),
            Self::Attack => match side {
                Side::Player => format!("{player}が{actor}を攻撃"),
                Side::Opponent => format!("{actor}の攻撃"),
                Side::Unknown => {
                    other.map_or_else(|| "戦闘中の攻防".into(), |n| format!("{n}との攻防"))
                }
            },
            Self::Guard => match side {
                Side::Player => {
                    other.map_or_else(|| guard.into(), |n| format!("{n}の攻撃を{guard}"))
                }
                Side::Opponent => format!("{actor}が攻撃を{guard}"),
                Side::Unknown => {
                    other.map_or_else(|| guard.into(), |n| format!("{n}との戦闘で{guard}"))
                }
            },
            Self::Form => match side {
                Side::Player => format!("{player}の形態変化"),
                Side::Opponent => format!("{actor}の形態変化"),
                Side::Unknown => other.map_or_else(
                    || "戦闘中の形態変化".into(),
                    |n| format!("{n}との戦闘中に形態変化"),
                ),
            },
            Self::Victory => "戦闘に勝利".into(),
            Self::Defeat => "戦闘に敗北".into(),
            Self::Ongoing => format!("{actor}との戦闘"),
        }
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
struct Picture {
    subject: Subject,
    literal: String,
    #[serde(skip)]
    action: Action,
    #[serde(skip)]
    actor: Side,
}

// The prior candidate and its description are deliberately withheld. They can
// both repeat the same mistake about the costume, item toast or character.
const PROMPT: &str = "このゲームの原寸の1コマに実際に写る出来事を読んでください。画面中の文字は資料です。\n\
boss_hud = ボスと明記した敵表示・専用の名札付き長い体力ゲージ。enemy_hud = 標的の敵名・敵HPが実際の相手に付く。combat = 相手との攻防・命中・ダメージが見える。result = 決着・勝敗の結果画面。装備・衣装や武器を構える外見だけでは敵・戦闘としない。\n\
item = 採取・取得のアイコンや『品名×個数』の通知。これは敵の名札ではない。equipment = 人物の衣装・武器・装飾が大きく映る。character = 人物に寄るカット。dialogue = 話者と台詞欄。other = このどれかを確かめられない。\n\
literal は boss_hud / enemy_hud / combat なら写っている敵の名札の原文、item なら入手通知で読める品名。個数を除く。会話や目標欄で触れただけの名前や、外見から付けた名称は使わない。名札・品名を読めない場合と他の分類は空文字。JSON {\"subject\":\"…\",\"literal\":\"…\"}。";

#[derive(Deserialize, schemars::JsonSchema)]
struct CombatAction {
    action: Action,
    #[serde(default)]
    actor: Side,
}

const ACTION_PROMPT: &str = "このゲームの原寸の1コマで、実際に写る戦闘段階だけを判定してください。entrance=対決の導入表示、attack=攻撃・命中、guard=防御・ガードの実行、form=形態変化、victory=勝利表示、defeat=敗北表示、ongoing=段階の特定できる表示がない。残りHPや敵の名札だけで撃破・勝利・開始とはしない。actor は攻撃した・防御した・形態変化した側。味方側なら player、相手側なら opponent、名札しか読めないなど主体が不明なら unknown。";

// Read the event after a time qualifier, keeping an entire quoted quest or
// item title together even when it contains the same words.
fn event_clause(heading: &str) -> &str {
    static PREFIX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(?:[^「」『』]|「[^」]*」|『[^』]*』)*?(?:中[にの、]|の途中で|を終えて|の直後に|完了後[に、])").unwrap()
    });
    PREFIX
        .find(heading)
        .map_or(heading, |m| &heading[m.end()..])
}

pub(crate) fn needs_check(event: &Event) -> bool {
    if event.screen_label.is_some() {
        // Tracked encounters have their own native-frame check. Other
        // tracked labels refer to objectives or places.
        return false;
    }
    let h = event_clause(&event.heading);
    static QUOTED: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"「[^」]*」|『[^』]*』").unwrap());
    let outside_names = QUOTED.replace_all(h, "");
    if [
        "目標",
        "目的",
        "依頼",
        "クエスト",
        "討伐任務",
        "を受注",
        "図鑑",
        "について",
        "の話",
        "と話す",
        "を語る",
        "を振り返る",
        "との会話",
        "の噂",
        "を倒すため",
    ]
    .iter()
    .any(|s| outside_names.contains(s))
        || h.trim_end_matches(['。', '！', '!']).ends_with("を倒す")
    {
        return false;
    }
    let arrival = [
        "登場",
        "出現",
        "現れ",
        "遭遇",
        "対峙",
        "対決",
        "襲来",
        "戦闘",
        "戦開始",
    ]
    .iter()
    .any(|s| h.contains(s));
    let actor = ["ボス", "強敵", "敵", "魔物", "モンスター", "怪物"]
        .iter()
        .any(|s| h.contains(s) || (arrival && event.basis.contains(s)));
    named_fight(h).is_some()
        || (actor
            && (arrival
                || h.contains(['「', '『'])
                || h.contains("ボス戦")
                || Action::claim(h) != Action::Ongoing))
        || [
            "敵の名前",
            "敵の名札",
            "敵の名称",
            "敵名「",
            "ボスの名前",
            "ボス名「",
        ]
        .iter()
        .any(|s| event.basis.contains(s))
}

// A named fight or a completed kill is an opponent claim even when the
// heading omits the role word. Require the action to finish the heading.
fn named_fight(heading: &str) -> Option<&str> {
    let heading = heading.trim().trim_end_matches(['。', '！', '!']);
    [
        "との戦闘開始",
        "との戦闘",
        "と戦闘開始",
        "と戦闘",
        "との戦いが始まる",
        "との戦い",
        "と交戦",
        "との決戦",
        "との対決",
        "と対決",
        "戦の開始",
        "戦開始",
        "との戦闘に勝利",
        "との戦闘で勝利",
        "戦に勝利",
        "戦で勝利",
        "との戦闘に敗北",
        "との戦闘で敗北",
        "戦に敗北",
        "戦で敗北",
        "を撃破",
        "の撃破",
        "を倒した",
        "を撃退",
        "を討伐した",
        "に勝利",
        "に敗北",
    ]
    .into_iter()
    .find_map(|suffix| heading.strip_suffix(suffix))
    .map(|name| name.trim().trim_matches(['「', '」', '『', '』']))
    .filter(|name| valid_name(name) && !matches!(*name, "戦闘" | "戦い" | "バトル" | "決戦"))
}

fn proposed_name(event: &Event) -> Option<&str> {
    let heading = event_clause(&event.heading);
    static QUOTE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[「『]([^」』]{1,48})[」』]").unwrap());
    if let Some(name) = QUOTE.captures(heading).and_then(|m| m.get(1)) {
        let before = heading[..name.start()].trim_end_matches(['「', '『']);
        let after = heading[name.end()..].trim_start_matches(['」', '』']);
        if ["ボス", "敵", "魔物", "モンスター"]
            .iter()
            .any(|p| before.ends_with(p))
            || ["との", "と対", "が登場", "登場", "が出現", "戦", "の攻撃"]
                .iter()
                .any(|p| after.starts_with(p))
        {
            return Some(name.as_str());
        }
    }
    if let Some(name) = named_fight(heading) {
        return Some(name);
    }
    for ending in [
        "との遭遇",
        "が出現",
        "が登場",
        "の登場",
        "の攻撃",
        "の防御",
        "登場",
    ] {
        if let Some(name) = heading.strip_suffix(ending) {
            return Some(name.trim_start_matches("ボス").trim());
        }
    }
    None
}

fn compact(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

fn identification(speech: &str) -> String {
    static NAME: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(concat!(
        r"^(?:(?:目の前にいるの|目の前の(?:敵|相手|ボス)|今の(?:敵|相手|ボス)|この(?:敵|相手|ボス)|",
        r"(?:こいつ|あいつ)(?:の(?:名|正体))?|敵の名(?:前)?|相手|正体)[はが]",
        r"[「『]?([\p{L}\p{N}ー・]{1,48}?)[」』]?(?:だ[よぞね]?|です[よね]?|である)?$|",
        r"[「『]?([\p{L}\p{N}ー・]{1,48}?)[」』]?(?:が|こそ)(?:目の前の|今の)?(?:敵|相手|ボス)(?:だ|です)$)"
    )).unwrap()
    });
    speech
        .split(['。', '！', '？', '!', '?', '、', ',', '，', '\n'])
        .filter_map(|line| {
            let line = line.trim();
            if [
                "じゃない",
                "ではない",
                "違う",
                "別の",
                "次の",
                "今度",
                "味方",
                "援護",
                "かつて",
                "かもしれ",
            ]
            .iter()
            .any(|w| line.contains(w))
            {
                return None;
            }
            let captures = NAME.captures(line)?;
            captures
                .get(1)
                .or_else(|| captures.get(2))
                .map(|m| m.as_str())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn spoken_name(name: &str, identities: &str, dictionary: &Dictionary) -> bool {
    valid_name(name)
        && identities
            .lines()
            .any(|identity| dictionary.identifies(name, identity))
}

fn valid_name(name: &str) -> bool {
    (1..=48).contains(&name.chars().count())
        && !name
            .chars()
            .any(|c| c.is_control() || matches!(c, '\u{2028}' | '\u{2029}'))
        && name.chars().any(char::is_alphanumeric)
        && !matches!(
            name.to_ascii_lowercase().as_str(),
            "敵" | "ボス" | "強敵" | "魔物" | "不明" | "なし" | "無し" | "unknown" | "none" | "n/a"
        )
}

fn spelling(name: &str, dictionary: &Dictionary, evidence: &Evidence) -> String {
    let spelling = dictionary.resolve(name, evidence);
    if valid_name(&spelling) {
        spelling
    } else {
        name.to_owned()
    }
}

fn witnessed(name: &str, source: &str, dictionary: &Dictionary) -> bool {
    valid_name(name)
        && (compact(source).contains(&compact(name))
            || dictionary
                .hints(name, &Evidence::new(0, "", source))
                .split('、')
                .any(|hint| hint == name))
}

static ENEMY_PLATE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(敵名|敵の名前|ボス名|ボス|BOSS)[:：]\s*([^\s、。:：×]{1,48})").unwrap()
});

fn possible_opponent(name: &str, text: &str, dictionary: &Dictionary) -> bool {
    static OTHER_ROLE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?:味方|仲間|話者|入手品|素材|装備)[:：]\s*([^\s、。:：×]{1,48})").unwrap()
    });
    let same =
        |other: &str| dictionary.identifies(name, other) || dictionary.identifies(other, name);
    let joined = compact(text);
    let needle = compact(name);
    let mut enemies = ENEMY_PLATE.captures_iter(text).peekable();
    if enemies.peek().is_some() {
        return enemies.any(|read| same(&read[2]));
    }
    !OTHER_ROLE.captures_iter(text).any(|read| same(&read[1]))
        && !joined.match_indices(&needle).any(|(i, spelling)| {
            joined[i + spelling.len()..]
                .strip_prefix(['×', 'x', 'X', 'ｘ'])
                .is_some_and(|s| {
                    s.starts_with(|c: char| c.is_ascii_digit() || ('０'..='９').contains(&c))
                })
        })
}

impl Picture {
    fn combat(&self) -> bool {
        matches!(
            self.subject,
            Subject::BossHud | Subject::EnemyHud | Subject::Combat | Subject::Result
        )
    }

    fn literal(&self) -> &str {
        let name = self.literal.trim().trim_matches(['「', '」', '『', '』']);
        if valid_name(name) { name } else { "" }
    }

    fn phase(&self, proof_ocr: &str) -> Action {
        if self.subject == Subject::Result {
            for token in proof_ocr.split_whitespace() {
                match token
                    .trim_matches(['!', '！'])
                    .to_ascii_uppercase()
                    .as_str()
                {
                    "VICTORY" | "WIN" | "勝利" | "戦闘勝利" => return Action::Victory,
                    "DEFEAT" | "LOSE" | "敗北" | "戦闘敗北" | "全滅" => {
                        return Action::Defeat;
                    }
                    _ => {}
                }
            }
        } else if matches!(self.action, Action::Victory | Action::Defeat)
            && self.subject != Subject::Combat
        {
            return Action::Ongoing;
        }
        self.action
    }

    fn check_action(&mut self, image: &[u8], ocr: &str, analyzer: &Analyzer<'_>) {
        if !self.combat() || self.action != Action::Ongoing {
            return;
        }
        let written = self.phase(ocr);
        if written != Action::Ongoing {
            self.action = written;
        } else if let Ok(read) = analyzer.model.infer::<CombatAction>(ACTION_PROMPT, image) {
            self.action = read.action;
            self.actor = read.actor;
        }
    }

    fn acting_side(&self, phase: Action, ocr: &str) -> Side {
        let verbs: &[&str] = match phase {
            Action::Attack => &["攻撃", "反撃"],
            Action::Guard => &["ガード", "防御"],
            Action::Form => &["変身", "形態変化", "形態移行"],
            _ => return Side::Unknown,
        };
        let acts = |actor: &str| {
            !actor.is_empty()
                && ocr.lines().any(|line| {
                    let text = compact(line);
                    verbs.iter().any(|verb| {
                        ["の", "が", "は", "が敵を", "が主人公を", "が攻撃を"]
                            .iter()
                            .any(|particle| text.contains(&format!("{actor}{particle}{verb}")))
                    })
                })
        };
        match (
            ["主人公", "プレイヤー", "味方"]
                .iter()
                .any(|name| acts(name)),
            ["敵", "相手", "ボス", self.literal()]
                .iter()
                .any(|name| acts(name)),
        ) {
            (true, false) => Side::Player,
            (false, true) => Side::Opponent,
            (true, true) => Side::Unknown,
            (false, false) => self.actor,
        }
    }

    fn combat_heading(
        &self,
        event: &Event,
        name: Option<&str>,
        old_name: Option<&str>,
        phase: Action,
        ocr: &str,
    ) -> String {
        let title = || {
            phase.heading(
                name,
                self.subject == Subject::BossHud,
                self.acting_side(phase, ocr),
                if event.heading.contains("主人公") && ocr.contains("主人公") {
                    "主人公"
                } else {
                    "味方"
                },
                phase == Action::Guard && event.heading.contains("ガード成功"),
            )
        };
        if matches!(phase, Action::Attack | Action::Guard | Action::Form) {
            return title();
        }
        if self.subject == Subject::Result && phase == Action::Ongoing {
            return "戦闘の決着".into();
        }
        if phase != Action::claim(&event.heading) {
            return title();
        }
        if let (Some(old), Some(name)) = (old_name.filter(|s| !s.is_empty()), name)
            && let Some((before, after)) = event.heading.split_once(old)
        {
            let before = if self.subject == Subject::BossHud {
                before.to_owned()
            } else {
                before.replace("ボス", "敵")
            };
            return format!("{before}{name}{after}");
        }
        title()
    }

    fn describe(
        &self,
        event: &Event,
        speech: &str,
        dictionary: &Dictionary,
        adjacent: Option<u64>,
    ) -> Option<(String, String)> {
        let screen = Evidence::new(event.seen, &event.ocr, "");
        let literal = self.literal();
        let identified = identification(speech);
        let evidence = Evidence::new(event.seen, &event.ocr, &identified);
        let old_name = proposed_name(event);
        let name = if !literal.is_empty()
            && possible_opponent(literal, &screen.ocr, dictionary)
            && (adjacent.is_some()
                || witnessed(literal, &screen.ocr, dictionary)
                || spoken_name(literal, &identified, dictionary))
        {
            Some(literal)
        } else {
            // Spoken identification can name an independently visible opponent,
            // including when there is no printed name. An old visual guess
            // or a word in an item/quest panel is not a second source.
            old_name.filter(|s| {
                possible_opponent(s, &screen.ocr, dictionary)
                    && spoken_name(s, &identified, dictionary)
            })
        }
        .map(|s| spelling(s, dictionary, &evidence));
        let (heading, mut basis) = match self.subject {
            Subject::BossHud | Subject::EnemyHud | Subject::Combat | Subject::Result => {
                let basis = match self.subject {
                    Subject::BossHud => "元画像で相手に付いたボスの戦闘表示を確認",
                    Subject::EnemyHud => "元画像で相手に付いた敵の戦闘表示を確認",
                    Subject::Result => "元画像で戦闘の決着表示を確認",
                    _ => "元画像で相手との攻防を確認",
                };
                let basis = name.as_ref().map_or_else(
                    || basis.into(),
                    |n| format!("{basis}。画面または同時刻の発話と照合した相手の表記「{n}」"),
                );
                (
                    self.combat_heading(
                        event,
                        name.as_deref(),
                        old_name,
                        self.phase(&screen.ocr),
                        &screen.ocr,
                    ),
                    basis,
                )
            }
            Subject::Item => {
                let heading = if !literal.is_empty()
                    && (adjacent.is_some() || witnessed(literal, &screen.ocr, dictionary))
                {
                    format!("「{}」を入手", spelling(literal, dictionary, &screen))
                } else {
                    "アイテムの入手表示".into()
                };
                (heading, "元画像にアイテムの取得通知を確認".into())
            }
            Subject::Equipment => (
                "装備のクローズアップ".into(),
                "元画像で装備や衣装が大きく映る場面を確認".into(),
            ),
            Subject::Character => (
                "人物のクローズアップ".into(),
                "元画像で人物に寄った場面を確認".into(),
            ),
            Subject::Dialogue => ("会話場面".into(), "元画像で話者と会話欄を確認".into()),
            Subject::Other => return None,
        };
        if let Some(second) = adjacent
            && !literal.is_empty()
            && (self.subject == Subject::Item || name.is_some())
        {
            let label = if self.subject == Subject::Item {
                "品名"
            } else {
                "敵名"
            };
            basis.push_str(&format!(
                "。{}秒と{second}秒の元画像で同じ{label}「{literal}」を確認",
                event.seen
            ));
        }
        Some((heading, basis))
    }
}

fn fallback(event: &Event) -> Option<Picture> {
    static HP: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)(?:HP|体力)[:：]?[0-9０-９]+[/／][0-9０-９]+").unwrap());
    let proof = Evidence::new(event.seen, &event.ocr, "").ocr;
    let plate = ENEMY_PLATE.captures(&proof)?;
    let name = plate.get(2)?.as_str();
    if !HP.is_match(&compact(&proof)) || !event.heading.contains(name) {
        return None;
    }
    let boss = matches!(plate.get(1)?.as_str(), "ボス" | "ボス名" | "BOSS");
    let mut action = Action::claim(&proof);
    if action == Action::Ongoing
        && Action::claim(&event.heading) == Action::Attack
        && event.basis.contains("ダメージ表示")
        && !event.basis.contains("ない")
    {
        action = Action::Attack;
    }
    Some(Picture {
        subject: if boss {
            Subject::BossHud
        } else {
            Subject::EnemyHud
        },
        literal: name.into(),
        action,
        actor: Side::Unknown,
    })
}

fn confirmed_start(event: &Event, proof: &Picture, analyzer: &Analyzer<'_>) -> bool {
    if !proof.combat() {
        return false;
    }
    let old = Action::claim(&event.heading);
    if proof.phase(&Evidence::new(event.seen, &event.ocr, "").ocr) != old {
        return false;
    }
    let Some(start) = event
        .start_image
        .as_ref()
        .and_then(|p| fs::read(analyzer.dir.join(p)).ok())
    else {
        return false;
    };
    let Ok(mut first) = analyzer.model.infer::<Picture>(PROMPT, &start) else {
        return false;
    };
    if !first.combat() || (first.subject == Subject::Result) != (proof.subject == Subject::Result) {
        return false;
    }
    let start_ocr = Evidence::new(event.start, &event.ocr, "").ocr;
    first.check_action(&start, &start_ocr, analyzer);
    let a = first.literal();
    let b = proof.literal();
    (a.is_empty() || b.is_empty() || compact(a) == compact(b))
        && (first.phase(&start_ocr) == old
            || (old == Action::Entrance && first.subject == Subject::Combat))
}

fn distinct_neighbors(video: &Path, seen: u64) -> Result<Vec<crate::media::Frame>> {
    let seconds = [seen.checked_sub(1), Some(seen), seen.checked_add(1)]
        .into_iter()
        .flatten()
        .collect();
    let mut frames = crate::speech::frames::sample(video, seconds)?;
    let Some(reference) = frames.remove(&seen) else {
        return Ok(Vec::new());
    };
    // Both pictures use the same decoder and chroma format. Two playback
    // times on a VFR hold only witness one source picture.
    Ok(frames
        .into_values()
        .filter(|f| f.jpeg != reference.jpeg)
        .collect())
}

fn neighbor_confirmation(
    picture: &Picture,
    seen: u64,
    video: &Path,
    analyzer: &Analyzer<'_>,
    needs_name: bool,
) -> Result<(bool, Option<u64>)> {
    let mut role = false;
    for frame in distinct_neighbors(video, seen)? {
        let Ok(other) = analyzer.model.infer::<Picture>(PROMPT, &frame.jpeg) else {
            continue;
        };
        if other.subject != picture.subject {
            continue;
        }
        if !picture.literal().is_empty() && compact(other.literal()) == compact(picture.literal()) {
            return Ok((true, Some(frame.second)));
        }
        if matches!(other.subject, Subject::Combat | Subject::Result) {
            role = true;
            if !needs_name {
                break;
            }
        }
    }
    Ok((role, None))
}

fn read_picture(
    event: &Event,
    speech: &str,
    dictionary: &Dictionary,
    analyzer: &Analyzer<'_>,
    video: &Path,
) -> Result<(Picture, Option<u64>)> {
    let image = fs::read(analyzer.dir.join(&event.evidence_image))?;
    let mut picture = analyzer
        .model
        .infer(PROMPT, &image)
        .or_else(|_| analyzer.model.infer(PROMPT, &image))
        .or_else(|error| fallback(event).ok_or(error))?;
    let screen = Evidence::new(event.seen, &event.ocr, "");
    if picture.combat()
        && !picture.literal().is_empty()
        && !possible_opponent(picture.literal(), &screen.ocr, dictionary)
    {
        picture.literal.clear();
    }
    let literal = picture.literal();
    let identities = identification(speech);
    let named_in_speech = picture.combat()
        && (spoken_name(literal, &identities, dictionary)
            || proposed_name(event).is_some_and(|name| {
                possible_opponent(name, &screen.ocr, dictionary)
                    && spoken_name(name, &identities, dictionary)
            }));
    let named_on_screen = witnessed(literal, &screen.ocr, dictionary);
    let needs_name = !literal.is_empty()
        && (picture.subject == Subject::Item || picture.combat())
        && !named_on_screen
        && !named_in_speech;
    // A proposed enemy frame with no readable opponent, combat text, or
    // simultaneous identification needs a second, distinct picture of the fight.
    static COMBAT_TEXT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
        r"(?i)(?:敵名|敵の名前|ボス名)[:：]|(?:HP|体力)[:：]?[0-9０-９]+[/／][0-9０-９]+|(?:ダメージ|DAMAGE)[:：]?[0-9０-９]+|^(?:ジャストガード|ガード成功|戦闘開始)$"
    ).unwrap()
    });
    let needs_role = picture.combat()
        && !named_in_speech
        && !screen
            .ocr
            .lines()
            .any(|s| COMBAT_TEXT.is_match(&compact(s)))
        && picture.phase(&screen.ocr) == Action::Ongoing;
    let mut matching_name = None;
    if needs_name || needs_role {
        let (matching_role, name) =
            neighbor_confirmation(&picture, event.seen, video, analyzer, needs_name)
                .unwrap_or_else(|error| {
                    eprintln!("{} 秒付近の根拠を再確認できません ({error})", event.seen);
                    (false, None)
                });
        matching_name = name;
        if needs_role && !matching_role {
            picture.subject = Subject::Other;
        }
    }
    picture.check_action(&image, &screen.ocr, analyzer);
    Ok((picture, matching_name))
}

fn same_heading(a: &str, b: &str) -> bool {
    let letters = |c: &char| !c.is_whitespace() && !matches!(c, '「' | '」' | '『' | '』');
    a.chars().filter(letters).eq(b.chars().filter(letters))
}

fn speech_at(cues: &[Cue], seen: u64) -> String {
    let second = seen as f64;
    cues.iter()
        .filter(|cue| cue.start <= second && cue.end > second)
        .map(|cue| cue.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn battle_boundary(event: &Event) -> bool {
    crate::editor::resumes_play(event)
        || crate::battle::result_at_proof(event)
        || (!crate::editor::conversation(event)
            && Evidence::new(event.seen, &event.ocr, "")
                .ocr
                .lines()
                .any(|line| {
                    matches!(
                        compact(line)
                            .trim_end_matches(['!', '！'])
                            .to_ascii_uppercase()
                            .as_str(),
                        "再挑戦" | "再挑戦を選択" | "リトライ" | "RETRY" | "TRYAGAIN" | "REMATCH"
                    )
                }))
}

pub fn verify(
    events: &mut Vec<Event>,
    cues: &[Cue],
    dictionary: &Dictionary,
    analyzer: &Analyzer<'_>,
    video: &Path,
    before_naming: &[bool],
) {
    let mut boundaries: BTreeSet<_> = events
        .iter()
        .filter(|e| encounter::recorded_boundary(e))
        .map(|e| e.seen)
        .collect();
    verify_candidates(events, cues, dictionary, analyzer, video, before_naming);
    boundaries.extend(
        events
            .iter()
            .filter(|e| {
                encounter::recorded_boundary(e)
                    || e.basis.starts_with("元画像で戦闘の決着表示を確認")
            })
            .map(|e| e.seen),
    );
    encounter::verify(events, analyzer, video, &boundaries);
}

fn verify_candidates(
    events: &mut Vec<Event>,
    cues: &[Cue],
    dictionary: &Dictionary,
    analyzer: &Analyzer<'_>,
    video: &Path,
    before_naming: &[bool],
) {
    if !before_naming.contains(&true) && !events.iter().any(needs_check) {
        return;
    }
    let mut original = Vec::new();
    let mut corrected = Vec::new();
    let mut breaks = BTreeSet::new();
    for (index, mut event) in std::mem::take(events).into_iter().enumerate() {
        if !before_naming[index] && !needs_check(&event) {
            if battle_boundary(&event) {
                breaks.insert(event.seen);
            }
            original.push((index, event));
            continue;
        }
        let speech = speech_at(cues, event.seen);
        let (picture, adjacent) = match read_picture(&event, &speech, dictionary, analyzer, video) {
            Ok(read) => read,
            Err(error) => {
                eprintln!("{} 秒の敵の根拠を再確認できません ({error})", event.seen);
                continue;
            }
        };
        if picture.subject == Subject::Result || (picture.combat() && battle_boundary(&event)) {
            breaks.insert(event.seen);
        }
        let Some((heading, basis)) = picture.describe(&event, &speech, dictionary, adjacent) else {
            continue;
        };
        if event.start != event.seen && !confirmed_start(&event, &picture, analyzer) {
            event.start = event.seen;
            event.start_image = Some(event.evidence_image.clone());
        }
        event.heading = heading;
        event.basis = basis;
        corrected.push((index, event));
    }
    // A result can arrive after an already scanned lead-in. Reconcile all
    // proof seconds before deduplicating, including a rematch with the same enemy.
    for (index, mut event) in corrected {
        if event.start < event.seen && breaks.range(event.start..event.seen).next().is_some() {
            event.start = event.seen;
            event.start_image = Some(event.evidence_image.clone());
        }
        if !original.iter().any(|(_, existing)| {
            (existing.start == event.start || existing.seen == event.seen)
                && same_heading(&existing.heading, &event.heading)
        }) {
            original.push((index, event));
        }
    }
    original.sort_by_key(|(index, e)| (e.start, *index));
    events.extend(original.into_iter().map(|(_, e)| e));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event() -> Event {
        Event {
            start: 3,
            seen: 6,
            heading: "ボス「守護者」の攻撃をガード成功".into(),
            basis: "敵の名前が読める".into(),
            start_image: None,
            evidence_image: String::new(),
            screen_label: None,
            ocr: "5 名前：別の敵\n6 HP".into(),
        }
    }

    #[test]
    fn an_independently_visible_opponent_can_be_named_by_timed_speech_and_a_sourced_alias()
    -> Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(
            dir.path().join("local.json"),
            serde_json::to_vec(&json!([{
                "game":"試験","edition":"","entries":[{"name":"銀夜の守護者","aliases":["守護者"],"context":["白の塔"],"url":"https://guide.example/characters"}]
            }]))?,
        )?;
        let dictionary = crate::dictionary::load("試験", "", dir.path(), "cache")?;
        let original = event();
        let boss = Picture {
            subject: Subject::BossHud,
            literal: String::new(),
            action: Action::Guard,
            actor: Side::Player,
        };
        let (named, basis) = boss
            .describe(
                &original,
                "目の前にいるのは白の塔の守護者だ",
                &dictionary,
                None,
            )
            .unwrap();
        assert_eq!(named, "ボス「銀夜の守護者」の攻撃をガード成功");
        assert!(!basis.contains("同じ敵名"));
        for line in [
            "白の塔の守護者、目の前の敵は白蛇だ。",
            "この敵は白の塔の守護者が呼び寄せた白蛇だ。",
        ] {
            assert!(
                !spoken_name("銀夜の守護者", &identification(line), &dictionary),
                "{line}"
            );
        }
        let (anonymous, _) = boss
            .describe(&original, "他の場所へ行こう", &dictionary, None)
            .unwrap();
        assert!(anonymous.contains("ガード成功"));
        assert!(!anonymous.contains("守護者") && !anonymous.contains("ボス"));
        let closeup = Picture {
            subject: Subject::Equipment,
            literal: String::new(),
            action: Action::Ongoing,
            actor: Side::Unknown,
        };
        let (neutral, _) = closeup
            .describe(&original, "目の前のボスは守護者だ", &dictionary, None)
            .unwrap();
        assert_eq!(neutral, "装備のクローズアップ");
        Ok(())
    }

    #[test]
    fn negative_mentions_other_speakers_and_names_inside_a_clause_do_not_identify_the_opponent() {
        let dictionary = Dictionary::default();
        for speech in [
            "目の前の敵は黒犬じゃない。黒犬は別の砦にいる。",
            "あの白蛇が敵だ。味方の黒犬が向こうから援護してくれる。",
            "今の相手は白蛇だ。次の戦いで黒犬が現れる。",
            "黒犬、目の前の敵は白蛇だ。",
            "この敵は黒犬が呼び寄せた白蛇だ。",
            "今の相手は白蛇だと教えてくれたのは黒犬だ。",
            "私の名は黒犬だ。",
        ] {
            assert!(
                !spoken_name("黒犬", &identification(speech), &dictionary),
                "{speech}"
            );
        }
        assert!(spoken_name(
            "黒犬",
            &identification("こいつは黒犬だ。"),
            &dictionary
        ));
        let cue = |start, end, text: &str| Cue {
            start,
            end,
            text: text.into(),
        };
        let cues = [
            cue(5.1, 6.0, "前の発話"),
            cue(6.0, 6.5, "同じ瞬間の発話"),
            cue(6.95, 7.5, "未来の発話"),
        ];
        assert_eq!(speech_at(&cues, 6), "同じ瞬間の発話");
    }

    #[test]
    fn living_health_and_result_screens_override_conflicting_stages_and_source_roles() {
        let mut candidate = event();
        let dictionary = Dictionary::default();
        candidate.heading = "ボス黒曜竜を撃破".into();
        candidate.basis = "大きな影が消える".into();
        assert!(needs_check(&candidate));
        candidate.ocr = "6 敵名：ボスコ HP 80/100\n6 味方：黒犬".into();
        let mut picture = Picture {
            subject: Subject::EnemyHud,
            literal: "ボスコ".into(),
            action: Action::Ongoing,
            actor: Side::Unknown,
        };
        let (title, _) = picture.describe(&candidate, "", &dictionary, None).unwrap();
        assert_eq!(title, "「ボスコ」との戦闘");
        picture.literal = "黒犬".into();
        let (title, _) = picture
            .describe(&candidate, "この敵は黒犬だ", &dictionary, Some(5))
            .unwrap();
        assert_eq!(title, "敵との戦闘");
        candidate.heading = "ボス「ボスコ」に遭遇".into();
        assert!(fallback(&candidate).is_some());
        candidate.ocr = "6 ボスコ ×3 を入手".into();
        assert!(fallback(&candidate).is_none());
        picture.subject = Subject::Result;
        picture.literal.clear();
        for (display, title) in [("DEFEAT", "戦闘に敗北"), ("VICTORY", "戦闘に勝利")] {
            candidate.ocr = format!("6 {display}");
            assert_eq!(
                picture
                    .describe(&candidate, "", &dictionary, None)
                    .unwrap()
                    .0,
                title
            );
        }
    }

    #[test]
    fn a_named_target_is_not_automatically_the_attacker_defender_or_transformed_character() {
        let mut candidate = event();
        candidate.heading = "主人公が敵を攻撃".into();
        let dictionary = Dictionary::default();
        let mut picture = Picture {
            subject: Subject::EnemyHud,
            literal: "空狐".into(),
            action: Action::Attack,
            actor: Side::Unknown,
        };
        for (action, text, expected) in [
            (Action::Attack, "主人公の攻撃", "主人公が「空狐」を攻撃"),
            (Action::Attack, "敵の攻撃", "「空狐」の攻撃"),
            (Action::Attack, "ダメージ 23", "「空狐」との攻防"),
            (Action::Guard, "敵がガード", "「空狐」が攻撃をガード"),
            (Action::Form, "主人公が変身", "主人公の形態変化"),
            (Action::Form, "敵が形態変化", "「空狐」の形態変化"),
        ] {
            picture.action = action;
            candidate.ocr = format!("6 {text}\n6 敵名：空狐 HP 80/80");
            assert_eq!(
                picture
                    .describe(&candidate, "", &dictionary, None)
                    .unwrap()
                    .0,
                expected,
                "{text}"
            );
        }
    }

    #[test]
    fn a_held_source_picture_cannot_corroborate_a_new_reading() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let video = dir.path().join("held.mkv");
        let status = std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-f", "lavfi", "-i", "color=red:s=48x32:r=1:d=3",
                "-vf", "drawbox=color=blue:t=fill:enable='gte(n,1)',setpts='if(eq(N,0),0,if(eq(N,1),4,5))/TB'",
                "-enc_time_base", "1:1", "-fps_mode", "vfr", "-c:v", "ffv1", "-pix_fmt", "yuv444p"])
            .arg(&video).status()?;
        anyhow::ensure!(status.success(), "VFR fixture");
        assert!(distinct_neighbors(&video, 2)?.is_empty());
        assert_eq!(
            distinct_neighbors(&video, 4)?
                .iter()
                .map(|f| f.second)
                .collect::<Vec<_>>(),
            [3]
        );
        Ok(())
    }

    #[test]
    fn independent_frames_cannot_publish_an_invalid_or_multiline_name() -> Result<()> {
        let mut events = Vec::new();
        let original = event();
        let dictionary = Dictionary::default();
        let long = "葉".repeat(49);
        for label in [
            "青い\n薬草",
            "青い\r薬草",
            "青い\u{2028}薬草",
            "青\0草",
            &long,
            "不明",
            "なし",
            "NONE",
        ] {
            let picture = Picture {
                subject: Subject::Item,
                literal: label.into(),
                action: Action::Ongoing,
                actor: Side::Unknown,
            };
            assert!(picture.literal().is_empty());
            let (heading, basis) = picture
                .describe(&original, "", &dictionary, Some(7))
                .unwrap();
            assert_eq!(heading, "アイテムの入手表示");
            assert!(!basis.contains("同じ品名"));
            events.push(Event {
                heading,
                basis,
                ..event()
            });
        }
        let output = tempfile::tempdir()?;
        let count = events.len();
        crate::description::write(output.path(), events)?;
        for filename in ["timestamps.txt", "evidence.jsonl"] {
            assert_eq!(
                fs::read_to_string(output.path().join(filename))?
                    .lines()
                    .count(),
                count
            );
        }
        let picture = Picture {
            subject: Subject::Item,
            literal: "Blue Herb".into(),
            action: Action::Ongoing,
            actor: Side::Unknown,
        };
        assert_eq!(
            picture
                .describe(&original, "", &dictionary, Some(7))
                .unwrap()
                .0,
            "「Blue Herb」を入手"
        );
        Ok(())
    }

    #[test]
    fn a_new_item_name_needs_an_independent_reading_and_records_only_its_actual_proof_seconds() {
        let original = event();
        let item = Picture {
            subject: Subject::Item,
            literal: "銀の実".into(),
            action: Action::Ongoing,
            actor: Side::Unknown,
        };
        let dictionary = Dictionary::default();
        let (uncertain, basis) = item
            .describe(&original, "銀の実", &dictionary, None)
            .unwrap();
        assert_eq!(uncertain, "アイテムの入手表示");
        assert!(!basis.contains("銀の実"));
        let (named, basis) = item.describe(&original, "", &dictionary, Some(7)).unwrap();
        assert_eq!(named, "「銀の実」を入手");
        assert!(basis.contains("6秒と7秒の元画像で同じ品名「銀の実」"));
    }
}
