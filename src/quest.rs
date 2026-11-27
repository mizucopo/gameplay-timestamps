//! Link a named Japanese quest offer to its on-screen introduction. A
//! request in dialogue alone cannot establish a quest or supply its title.
use std::collections::{BTreeMap, BTreeSet};

use crate::{analyze::Event, scene::Signal};

const LOOKBACK: u64 = 180;
const TITLES: &[&str] = &[
    "依頼名",
    "クエスト名",
    "任務名",
    "ミッション名",
    "依頼タイトル",
];
const CLIENTS: &[&str] = &["依頼人", "依頼主", "依頼者", "発注者"];
const GOALS: &[&str] = &[
    "達成条件",
    "クリア条件",
    "成功条件",
    "依頼内容",
    "任務内容",
    "目的",
    "目標",
];

fn contains_any(text: &str, labels: &[&str]) -> bool {
    labels.iter().any(|label| text.contains(label))
}

// OCR may separate every kanji with spaces, and either width of colon can
// follow a field. Leave spaces between Latin words in displayed names.
fn line(text: &str) -> String {
    let mut result = String::new();
    let mut space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            space = true;
            continue;
        }
        if space
            && ch.is_ascii_alphanumeric()
            && result.ends_with(|c: char| c.is_ascii_alphanumeric())
        {
            result.push(' ');
        }
        space = false;
        result.push(if ch == '：' { ':' } else { ch });
    }
    result
}

fn value(text: &str) -> &str {
    text.trim_matches(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                ':' | '・' | '「' | '」' | '『' | '』' | '【' | '】' | '◆' | '◇'
            )
    })
}

fn field<'a>(lines: &'a [String], labels: &[&str]) -> Option<(usize, &'a str)> {
    let (i, suffix) = lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            let suffix = labels.iter().find_map(|key| line.strip_prefix(key))?;
            (!suffix.starts_with(['の', 'を', 'が', 'は', 'に'])).then_some((i, suffix))
        })
        .min_by_key(|(_, s)| !(s.is_empty() || s.starts_with(':')))?;
    let tail = value(suffix);
    let text = if tail.is_empty() {
        value(lines.get(i + 1)?)
    } else {
        tail
    };
    Some((i, text))
}

fn boundary(text: &str) -> bool {
    text.lines().any(|raw| {
        let t = line(raw);
        !contains_any(
            &t,
            &[
                "達成条件",
                "クリア条件",
                "完了条件",
                "達成報酬",
                "クリア報酬",
            ],
        ) && contains_any(
            &t,
            &[
                "クエストクリア",
                "クエスト完了",
                "依頼完了",
                "任務完了",
                "クエスト達成",
                "依頼達成",
                "達成しました",
                "報酬を受け取",
                "受け取り済",
                "クエスト再開",
                "依頼再開",
                "依頼を再開",
                "再受注",
                "プレイ再開",
                "ロード中",
                "セーブデータをロード",
            ],
        )
    })
}

fn menu(text: &str) -> bool {
    contains_any(
        text,
        &[
            "クエスト一覧",
            "依頼一覧",
            "クエストログ",
            "依頼履歴",
            "受注済",
            "受注中",
            "完了済",
            "達成済",
        ],
    )
}

fn possible_name(text: &str) -> bool {
    let len = text.chars().count();
    (2..=42).contains(&len)
        && text.chars().any(char::is_alphabetic)
        && !text.contains('。')
        && !text.contains(':')
        && !menu(text)
        && !boundary(text)
        && !matches!(
            text,
            "クエスト"
                | "新規クエスト"
                | "サブクエスト"
                | "メインクエスト"
                | "依頼"
                | "依頼書"
                | "依頼票"
                | "受注"
                | "クエスト受注"
                | "受注する"
                | "引き受ける"
                | "報酬"
                | "はい"
                | "いいえ"
                | "戻る"
                | "決定"
        )
        && !TITLES
            .iter()
            .chain(CLIENTS)
            .chain(GOALS)
            .any(|&key| text == key)
}

struct Panel {
    title: String,
    client: Option<String>,
    goal: String,
}

fn panel(text: &str) -> Option<Panel> {
    let lines: Vec<_> = text.lines().map(line).filter(|t| !t.is_empty()).collect();
    let full = lines.join("\n");
    if boundary(&full) || menu(&full) {
        return None;
    }
    let client = field(&lines, CLIENTS);
    let named = field(&lines, TITLES);
    let goal = field(&lines, GOALS);
    let reward = lines
        .iter()
        .any(|l| l.starts_with("報酬") || l.starts_with("報奨"));
    let accepts = lines.iter().any(|l| {
        l == "受注"
            || l == "受ける"
            || l.ends_with("クエスト受注")
            || contains_any(
                l,
                &[
                    "受注する",
                    "受注します",
                    "受注しまし",
                    "受けますか",
                    "引き受ける",
                    "引き受けますか",
                ],
            )
    });
    let formal = named.is_some() || contains_any(&full, &["依頼書", "依頼票"]);
    if !((accepts || formal) && (client.is_some() || named.is_some()) && (reward || goal.is_some()))
    {
        return None;
    }
    // A form title is commonly an unlabeled heading immediately above the
    // requester. The body below that field contains prose, rewards and goals.
    let title = named
        .map(|(_, name)| name)
        .filter(|t| possible_name(t))
        .or_else(|| {
            let (i, _) = client?;
            lines[..i]
                .iter()
                .rev()
                .map(|l| value(l))
                .find(|t| possible_name(t))
        })?;
    Some(Panel {
        title: title.into(),
        client: client
            .map(|(_, t)| t)
            .filter(|t| possible_name(t) && t.chars().count() <= 24 && *t != "不明")
            .map(str::to_owned),
        goal: goal.map_or_else(String::new, |(_, t)| t.to_owned()),
    })
}

pub fn is_offer_screen(text: &str) -> bool {
    panel(text).is_some()
}

fn asks(text: &str) -> bool {
    contains_any(
        text,
        &[
            "依頼が",
            "依頼を",
            "この依頼",
            "お願いが",
            "頼みが",
            "頼みたい",
            "頼みごと",
            "頼み事",
            "頼める",
            "力を貸し",
            "手を貸し",
            "手伝って",
            "てきてほし",
            "てほしい",
            "てほしく",
            "てくれない",
            "届けてくれ",
            "探してくれ",
        ],
    )
}

fn topic(panel: &Panel, text: &str) -> bool {
    if text.contains(&panel.title) {
        return true;
    }
    // Shared content words can link an unnamed speaker's request to the
    // form; common field names and Japanese inflection cannot establish it.
    [&panel.title, &panel.goal]
        .into_iter()
        .flat_map(|s| nouns(s))
        .any(|term| text.contains(term))
}

fn nouns(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| {
        !matches!(c, '㐀'..='鿿' | 'ァ'..='ヺ' | 'ー') && !c.is_ascii_alphanumeric()
    })
    .filter(|&term| {
        term.chars().count() >= 2
            && !matches!(
                term,
                "依頼"
                    | "報酬"
                    | "目的"
                    | "達成"
                    | "目標"
                    | "対象"
                    | "条件"
                    | "入手"
                    | "入手後"
                    | "報告"
                    | "必要"
                    | "収集"
                    | "討伐"
                    | "納品"
            )
    })
}

fn speaker(panel: &Panel, text: &str) -> bool {
    let Some(client) = &panel.client else {
        return false;
    };
    text.lines().any(|line| {
        value(line) == client
            || value(line).strip_prefix(client).is_some_and(|rest| {
                rest.starts_with([':', '「', '『', '】']) || rest.starts_with("さん:")
            })
    })
}

fn fade(signals: &BTreeMap<u64, Signal>, lower: u64, anchor: u64) -> Option<u64> {
    let samples: Vec<_> = signals
        .range(lower.max(anchor.saturating_sub(14))..=anchor)
        .collect();
    let light = (1..samples.len())
        .rev()
        .find(|&i| samples[i - 1].1.dark && !samples[i].1.dark && anchor - samples[i].0 <= 6)?;
    let mut first = light - 1;
    while first > 0 && samples[first - 1].1.dark {
        first -= 1;
    }
    if first == 0 {
        return None;
    }
    // The visible fade may start a frame or two before full black.
    while first > 1
        && samples[first - 2].1.mean > samples[first - 1].1.mean + 12
        && samples[first - 1].1.mean > samples[first].1.mean + 8
    {
        first -= 1;
    }
    Some(*samples[first].0)
}

fn other_speaker(text: &str, client: &str) -> bool {
    let lines: Vec<_> = text.lines().map(line).collect();
    lines.windows(2).any(|pair| {
        let label = value(&pair[0]);
        label != client
            && (2..=16).contains(&label.chars().count())
            && possible_name(label)
            && !label.contains(['、', '。', '？', '！'])
            && !contains_any(label, &["オート", "スキップ", "マップ", "レベル", "目標"])
            && pair[1].chars().filter(|c| matches!(c, 'ぁ'..='ゖ')).count() >= 2
    })
}

// At one frame per second the bottom of a quick fade is often still lit.
// Require a local trough with recovery on both sides and a different
// on-screen speaker giving way to the requester. A dark room or a flash
// during a continuing speech cannot move the start.
fn speaker_fade(
    words: &BTreeMap<u64, String>,
    signals: &BTreeMap<u64, Signal>,
    lower: u64,
    anchor: u64,
    panel: &Panel,
) -> Option<u64> {
    let client = panel.client.as_deref()?;
    let text = words
        .get(&anchor)?
        .lines()
        .map(line)
        .collect::<Vec<_>>()
        .join("\n");
    if !speaker(panel, &text) {
        return None;
    }
    let samples: Vec<_> = signals
        .range(lower.max(anchor.saturating_sub(10))..=anchor)
        .collect();
    for i in (1..samples.len().saturating_sub(1)).rev() {
        let ((before, a), (low, b), (after, c)) = (samples[i - 1], samples[i], samples[i + 1]);
        let side = a.mean.min(c.mean);
        if after - low > 2
            || low - before > 2
            || anchor - after > 5
            || side < b.mean + 15
            || b.mean * 2 > side
            || a.mean.max(c.mean) > side * 2
        {
            continue;
        }
        let mut first = i;
        while first > 1
            && samples[first - 1].0 - samples[first - 2].0 <= 2
            && samples[first - 2]
                .1
                .mean
                .saturating_sub(samples[first - 1].1.mean)
                >= (samples[first - 2].1.mean / 8).max(7)
            && samples[first - 1].1.mean > samples[first].1.mean + 7
        {
            first -= 1;
        }
        let onset = *samples[first].0;
        // Use the last legible conversation before the dip. A prior stranger
        // earlier in the scene does not count if this requester was already speaking.
        for (_, raw) in words.range(lower.max(onset.saturating_sub(8))..onset).rev() {
            let prior = raw.lines().map(line).collect::<Vec<_>>().join("\n");
            if speaker(panel, &prior) || boundary(&prior) || menu(&prior) {
                break;
            }
            if other_speaker(&prior, client) {
                return Some(onset);
            }
        }
    }
    None
}

// A requester can be introduced well before the form, and later than the
// opening shot. Follow consecutive dialogue-control screens back to that
// shot only while the conversation stays on screen and concerns a request.
fn conversation_fade(
    words: &BTreeMap<u64, String>,
    signals: &BTreeMap<u64, Signal>,
    lower: u64,
    anchor: u64,
    mut related: bool,
) -> Option<u64> {
    let mut earliest = anchor;
    let mut screens = 0;
    for (&time, raw) in words.range(lower..=anchor).rev() {
        if time == anchor && !related {
            continue; // The form itself could still have a cinematic control.
        }
        let text = line(raw);
        if earliest - time > 15
            || menu(&text)
            || boundary(raw)
            || contains_any(&text, &["ミニマップ", "マップを開く", "装備変更"])
            || signals.range(time..earliest).any(|(_, s)| s.dark)
        {
            break;
        }
        if !contains_any(&text, &["スキップ", "自動送り", "会話ログ", "オート再生"])
        {
            continue;
        }
        related |= contains_any(&text, &["依頼", "クエスト", "任務"]);
        earliest = time;
        screens += 1;
    }
    if related && screens >= 2 {
        fade(signals, lower, earliest)
    } else {
        None
    }
}

pub struct Offer {
    pub start: u64,
    pub seen: u64,
    pub title: String,
    pub basis: String,
    pub ocr: String,
    lower: u64,
    first: u64,
    client: Option<String>,
    goal_terms: BTreeSet<String>,
}

impl Offer {
    pub fn matches(&self, event: &Event) -> bool {
        if event.seen < self.lower || event.seen > self.seen.saturating_add(12) {
            return false;
        }
        let heading = line(&event.heading);
        let action = heading.replace(&self.title, "");
        if contains_any(
            &action,
            &[
                "完了",
                "達成",
                "再開",
                "再受注",
                "報告",
                "決着",
                "報酬",
                "クリア",
                "進捗",
                "更新",
                "進展",
                "進行度",
                "目標追加",
                "目標変更",
            ],
        ) {
            return false;
        }
        // Once the offer is established, a later action can advance this very
        // quest. Only an explicit repeat of its acceptance can still be a duplicate.
        if event.start > self.seen && !(action.contains("受注") && event.ocr.contains("受注")) {
            return false;
        }
        let is_offer = contains_any(&action, &["依頼", "受注", "クエスト", "任務", "引き受け"]);
        if heading.contains(&self.title) && (value(&action).is_empty() || is_offer) {
            return true;
        }
        if ['「', '『']
            .into_iter()
            .any(|quote| heading.contains(quote))
        {
            return false;
        }
        let evidence = line(&event.ocr);
        if contains_any(&action, &["別件", "別の依頼", "別のクエスト"])
            || contains_any(&evidence, &["別の依頼を", "別件だが", "もう一件の依頼"])
        {
            return false;
        }
        if is_offer
            && (evidence.contains(&self.title)
                || ((event.start.abs_diff(self.start) <= 2
                    || event.seen.abs_diff(self.first) <= 2)
                    && self
                        .client
                        .as_ref()
                        .is_some_and(|name| evidence.contains(name))
                    && asks(&evidence)))
        {
            return true;
        }
        if event.start < self.start
            || event.seen > self.first
            || event.screen_label.is_some()
            || !contains_any(
                &event.basis,
                &[
                    "台詞",
                    "セリフ",
                    "発言",
                    "会話",
                    "問いかけ",
                    "と言",
                    "と話",
                    "と答",
                    "と伝",
                    "と頼",
                    "と依頼",
                    "語る",
                    "告げる",
                ],
            )
        {
            return false;
        }
        // An item obtained, enemy defeated or objective discovered is a
        // separate milestone even if a speaker comments on it using the same words.
        if contains_any(
            &action,
            &[
                "を入手",
                "を獲得",
                "を発見",
                "を撃破",
                "を倒す",
                "を救出",
                "に到着",
                "に到達",
                "戦闘開始",
                "との戦闘",
                "と戦闘",
                "と対決",
            ],
        ) {
            return false;
        }
        let heading_terms: Vec<_> = self
            .goal_terms
            .iter()
            .map(String::as_str)
            .filter(|term| heading.contains(term))
            .collect();
        let targeted = self.goal_terms.iter().any(|term| evidence.contains(term));
        let request_or_reply = asks(&evidence)
            || contains_any(
                &evidence,
                &[
                    "受けてくれ",
                    "引き受け",
                    "わかりました",
                    "分かりました",
                    "了解",
                    "任せて",
                    "それなら",
                ],
            );
        let issuer = self
            .client
            .as_ref()
            .is_some_and(|name| evidence.contains(name));
        let exchange = contains_any(
            &action,
            &[
                "依頼を",
                "依頼に応",
                "依頼の提示",
                "依頼の受注",
                "クエストを",
                "任務を",
                "引き受け",
                "受諾",
            ],
        );
        if is_offer
            && ((issuer && request_or_reply && exchange)
                || (targeted && (exchange || !heading_terms.is_empty())))
        {
            return true;
        }
        let discussing_goal = contains_any(
            &heading,
            &[
                "調べ",
                "調査",
                "探す",
                "探し",
                "確かめ",
                "確認",
                "向かう",
                "向かい",
                "届ける",
                "捜索",
                "決意",
            ],
        );
        let visual_text = line(&event.basis);
        (heading_terms.len() >= 2
            && heading_terms
                .iter()
                .any(|term| evidence.contains(term) || visual_text.contains(term)))
            || (discussing_goal && heading_terms.iter().any(|term| evidence.contains(term)))
    }
}

struct Seen {
    panel: Panel,
    first: u64,
    last: u64,
    confirmed: bool,
}

pub fn offers(
    words: &BTreeMap<u64, String>,
    signals: &BTreeMap<u64, Signal>,
    from: u64,
    until: u64,
) -> Vec<Offer> {
    let mut offers = Vec::new();
    let mut previous: Option<Seen> = None;
    let mut lower = from.saturating_sub(LOOKBACK + 12);
    for (&second, text) in words.range(lower..=until) {
        if boundary(text) {
            previous = None;
            lower = second.saturating_add(1);
            continue;
        }
        if menu(&line(text)) {
            if previous.as_ref().is_some_and(|p| !p.confirmed) {
                previous = None;
            }
            lower = second.saturating_add(1);
            continue;
        }
        let Some(current) = panel(text) else { continue };
        if let Some(last) = &mut previous {
            let same = current.title == last.panel.title
                && !matches!((&current.client,&last.panel.client), (Some(a),Some(b)) if a != b);
            if same && (last.confirmed || second - last.last <= 12) {
                last.last = second;
                if !last.confirmed && second >= from {
                    offers.push(introduction(
                        words, signals, lower, last.first, second, &current,
                    ));
                }
                last.confirmed = true;
                continue;
            }
            if !same {
                lower = lower.max(last.last.saturating_add(1));
            }
        }
        previous = Some(Seen {
            panel: current,
            first: second,
            last: second,
            confirmed: false,
        });
    }
    offers
}

fn introduction(
    words: &BTreeMap<u64, String>,
    signals: &BTreeMap<u64, Signal>,
    lower: u64,
    first: u64,
    seen: u64,
    panel: &Panel,
) -> Offer {
    let lower = lower.max(first.saturating_sub(LOOKBACK));
    let mut dialogue = None;
    let mut client_spoke = false;
    for (&second, raw) in words.range(lower..first).rev() {
        let text = raw.lines().map(line).collect::<Vec<_>>().join("\n");
        if menu(&text) || boundary(&text) {
            break;
        }
        let named = speaker(panel, &text);
        let request = asks(&text);
        let shared = topic(panel, &text);
        let continues = dialogue.is_some_and(|later| later - second <= 32);
        let has_line = text.contains(['「', '。', '？', '！'])
            || text
                .lines()
                .any(|l| l.chars().filter(|c| matches!(c, 'ぁ'..='ゖ')).count() >= 3);
        let explicit_offer = contains_any(
            &text,
            &[
                "依頼がある",
                "依頼したい",
                "依頼を受け",
                "依頼を引き受け",
                "この依頼を",
            ],
        );
        if !((named && has_line && (request || shared || continues || first - second <= 15))
            || (request && shared)
            || (explicit_offer && first - second <= 10))
        {
            continue;
        }
        if dialogue.is_some_and(|later| later - second > 32) {
            break;
        }
        dialogue = Some(second);
        client_spoke = named;
    }
    let anchor = dialogue.unwrap_or(first);
    // With no dialogue evidence, only a transition directly into this panel
    // (not a distant cinematic fade) can move its introduction.
    let fade_floor = if dialogue.is_some() {
        lower
    } else {
        lower.max(first.saturating_sub(3))
    };
    let dark = conversation_fade(words, signals, lower, anchor, dialogue.is_some())
        .or_else(|| fade(signals, fade_floor, anchor));
    let dip = dark
        .is_none()
        .then(|| speaker_fade(words, signals, lower, anchor, panel))
        .flatten();
    let start = dark.or(dip).unwrap_or(anchor);
    let mut basis = format!(
        "{first}秒と{seen}秒の依頼票に同じ依頼名「{}」を表示",
        panel.title
    );
    if let Some(s) = dialogue {
        if let Some(client) = panel.client.as_deref().filter(|_| client_spoke) {
            basis.push_str(&format!("。{s}秒に依頼人「{client}」の会話"));
        } else {
            basis.push_str(&format!("。{s}秒に依頼に対応する発話"));
        }
    }
    if dip.is_some() {
        basis.push_str(&format!(
            "。{start}秒に画面が減光し、明るさが戻って話者が交代"
        ));
    } else if start < anchor {
        basis.push_str(&format!("。その導入で{start}秒に暗転"));
    }
    let ocr = dialogue
        .into_iter()
        .chain([first, seen])
        .map(|time| {
            format!(
                "{time} {}",
                words[&time].chars().take(360).collect::<String>()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Offer {
        start,
        seen,
        title: panel.title.clone(),
        basis,
        ocr,
        lower,
        first,
        client: panel.client.clone(),
        goal_terms: nouns(&panel.goal).map(str::to_owned).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::Frame;
    use anyhow::Result;
    use image::{GrayImage, Luma, codecs::jpeg::JpegEncoder};

    fn form(title: &str, client: &str) -> String {
        format!(
            "{title}\n依 頼 人 ： {client}\n預かった荷物を渡すこと\n報酬\n金貨\n達成条件\n古地図を届ける\n受注"
        )
    }

    fn event(start: u64, seen: u64, heading: &str, ocr: &str) -> Event {
        Event {
            start,
            seen,
            heading: heading.into(),
            ocr: ocr.into(),
            basis: String::new(),
            start_image: None,
            evidence_image: String::new(),
            screen_label: None,
        }
    }

    fn fade_signals() -> Result<BTreeMap<u64, Signal>> {
        signals_for(&[
            (47, 180),
            (48, 180),
            (49, 130),
            (50, 80),
            (51, 2),
            (52, 2),
            (53, 160),
            (56, 160),
        ])
    }

    fn signals_for(levels: &[(u64, u8)]) -> Result<BTreeMap<u64, Signal>> {
        let mut signals = BTreeMap::new();
        for &(second, mean) in levels {
            let mut jpeg = Vec::new();
            JpegEncoder::new(&mut jpeg).encode_image(&GrayImage::from_pixel(
                96,
                54,
                Luma([mean]),
            ))?;
            let frame = Frame { second, jpeg };
            let signal = Signal::measure(&frame, signals.last_key_value().map(|(_, s)| s))?;
            signals.insert(second, signal);
        }
        Ok(signals)
    }

    #[test]
    fn a_still_lit_dip_can_introduce_a_different_speaker() -> Result<()> {
        let sheet = form("港の古地図", "運河の渡守");
        let mut words = BTreeMap::from([
            (12, "漁師の兄弟\n今朝は船が戻ってきた。".into()),
            (17, "運河の渡守\n頼みがある。古地図を届けてくれ。".into()),
            (54, sheet.clone()),
            (56, sheet),
        ]);
        let levels: Vec<_> = (10..=17)
            .zip([155, 155, 155, 91, 32, 101, 105, 105])
            .collect();
        let signals = signals_for(&levels)?;
        assert!(signals.values().all(|s| !s.dark));
        assert_eq!(offers(&words, &signals, 0, 56)[0].start, 13);
        let dim_room = signals_for(
            &(10..=17)
                .zip([155, 155, 155, 91, 32, 33, 32, 32])
                .collect::<Vec<_>>(),
        )?;
        assert_eq!(offers(&words, &dim_room, 0, 56)[0].start, 17);
        words.insert(12, "運河の渡守".into());
        assert_eq!(offers(&words, &signals, 0, 56)[0].start, 17);
        Ok(())
    }

    #[test]
    fn combines_the_same_spoken_offer_and_response_without_swallowing_other_actions() {
        let sheet = "依頼名：夜の届け物\n依頼人：文具屋\n報酬：金貨\n達成条件：灯台の古地図を探す";
        let words = BTreeMap::from([
            (6, "文具屋\n頼みがある。古地図のことだ。".into()),
            (69, sheet.into()),
            (71, sheet.into()),
        ]);
        let offers = offers(&words, &BTreeMap::new(), 0, 71);
        let q = &offers[0];
        let finding = |heading: &str, caption: &str, basis: &str| {
            let mut row = event(33, 33, heading, caption);
            row.basis = basis.into();
            q.matches(&row)
        };
        assert!(finding(
            "灯台で古地図がなくなる",
            "文具屋：灯台に古地図があった。",
            "文具屋の台詞に紛失の経緯を表示"
        ));
        assert!(finding(
            "文具屋が依頼を提示",
            "文具屋 受けてくれるか？",
            "文具屋が問いかける"
        ));
        assert!(finding(
            "見習いが依頼を受ける",
            "見習い それなら古地図を…",
            "見習いの発言に対する応答"
        ));
        assert!(finding(
            "灯台へ向かう決意",
            "見習い わかりました。灯台へ行きます",
            "台詞を表示"
        ));
        assert!(!finding(
            "古地図を入手",
            "文具屋 見つけたんだね",
            "台詞と入手画面に古地図を表示"
        ));
        assert!(!finding(
            "文具屋が屋号を語る",
            "文具屋 店の名は祖父が決めた",
            "文具屋が歴史を語る"
        ));
        assert!(!finding(
            "依頼「冬の届け物」",
            "文具屋 頼みがある。古地図も使う",
            "依頼人の会話を表示"
        ));
        assert!(!finding(
            "真鍮の飾りの依頼",
            "文具屋 受けてくれるか？",
            "文具屋が問いかける"
        ));
        assert!(!finding(
            "灯台の古地図を調査",
            "灯台 古地図",
            "主人公が棚の地図を手で広げた"
        ));
    }

    #[test]
    fn reads_a_repeated_form_and_links_its_requester_across_a_delayed_title() -> Result<()> {
        let title = "地図職人の忘れ物";
        let sheet = form(title, "旅の測量士");
        let words = BTreeMap::from([
            (18, "子供\nその本を取ってほしい。".into()),
            (56, "旅の測量士\n頼みがある。古地図を届けてほしい。".into()),
            (152, "別の人\n机を片付けてほしい。".into()),
            (160, sheet.clone()),
            (162, sheet.clone()),
            (167, sheet.clone()),
            (171, sheet.clone()),
        ]);
        let signals = fade_signals()?;
        // Both samples can straddle an analysis-window boundary.
        assert!(offers(&words, &signals, 0, 160).is_empty());
        let found = offers(&words, &signals, 161, 171);
        assert_eq!(found.len(), 1);
        let offer = &found[0];
        assert_eq!(
            (offer.start, offer.seen, offer.title.as_str()),
            (49, 162, title)
        );
        assert!(offer.ocr.contains("達成条件"));
        assert!(offer.matches(&event(56, 160, "地図職人の忘れ物の受注", &sheet)));
        assert!(!offer.matches(&event(57, 160, "地図職人の忘れ物を完了", &sheet)));
        assert!(!offer.matches(&event(56, 160, "地図職人の忘れ物を再開", &sheet)));
        assert!(!offer.matches(&event(
            165,
            165,
            "依頼「地図職人の忘れ物」進捗更新",
            "古地図 1/3"
        )));
        assert!(!offer.matches(&event(
            165,
            165,
            "依頼「地図職人の忘れ物」古地図を回収",
            "古地図 1/3"
        )));
        assert!(!offer.matches(&event(56, 160, "別の荷物の配達", "別の人から受け取る")));
        assert!(!offer.matches(&event(56, 160, "依頼「別の荷物」", &sheet)));
        assert!(offers(&words, &signals, 163, 171).is_empty());
        Ok(())
    }

    #[test]
    fn a_fade_into_continuous_request_dialogue_stops_at_a_return_to_gameplay() -> Result<()> {
        let signals = fade_signals()?;
        let sheet = form("落とし物の配達", "郵便屋");
        let mut words: BTreeMap<_, _> = (56..99)
            .step_by(7)
            .map(|sec| (sec, "スキップ\nもう少し聞いてください。".to_owned()))
            .collect();
        words.insert(56, "スキップ\n仕事の依頼だそうだ。".to_owned());
        words.insert(100, sheet.clone());
        words.insert(102, sheet);
        assert_eq!(offers(&words, &signals, 0, 102)[0].start, 49);
        words.insert(78, "フィールド\nマップを開く".to_owned());
        assert_eq!(offers(&words, &signals, 0, 102)[0].start, 100);
        Ok(())
    }

    #[test]
    fn distinguishes_other_offers_reacceptance_and_completed_or_browsed_quests() {
        let first = form("失われた織物", "染物師");
        let other = form("ガラスの風鈴", "細工師");
        let log = format!("クエスト一覧\n{first}");
        let completed = format!("クエスト完了\n{first}");
        let one_read = form("未確認の目標", "釣り人");
        let words = BTreeMap::from([
            (1, "通りすがり\n荷物を持ってほしい。".into()),
            (4, first.clone()),
            (6, first.clone()),
            (11, log.clone()),
            (14, first.clone()),
            (16, first.clone()),
            (22, other.clone()),
            (24, other.clone()),
            (30, completed.clone()),
            (32, completed.clone()),
            (39, "染物師\nまた頼みたい。".into()),
            (43, first.clone()),
            (44, first.clone()),
            (48, one_read.clone()),
            (50, log.clone()),
            (51, one_read.clone()),
        ]);
        assert!(!is_offer_screen(&log));
        assert!(!is_offer_screen(&completed));
        assert!(!is_offer_screen("染物師\nお願いがある。報酬は約束しよう。"));
        let found = offers(&words, &BTreeMap::new(), 0, 51);
        let times: Vec<_> = found
            .iter()
            .map(|o| (o.start, o.seen, o.title.as_str()))
            .collect();
        assert_eq!(
            times,
            [
                (4, 6, "失われた織物"),
                (22, 24, "ガラスの風鈴"),
                (39, 44, "失われた織物")
            ]
        );
        assert!(!found[2].matches(&event(4, 6, "失われた織物の依頼", &first)));
    }

    #[test]
    fn explicit_title_and_goal_can_link_a_request_even_without_a_speaker_caption() {
        let text = "依頼票\n依頼名：東の灯台\n依頼人：管理人\n報酬：金貨\n目標：灯台へ道具を届ける";
        let words = BTreeMap::from([
            (2, "話を聞いてほしい。".into()),
            (9, "灯台に道具を届けてほしい。".into()),
            (140, text.into()),
            (145, text.into()),
        ]);
        let found = offers(&words, &BTreeMap::new(), 0, 145);
        assert_eq!(found.len(), 1);
        assert_eq!(
            (found[0].start, found[0].seen, found[0].title.as_str()),
            (9, 145, "東の灯台")
        );
    }
}
