//! Review nearby observations together, using their existing evidence and IDs.
//! A retained row keeps its original timing, heading, images and evidence.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use serde::Deserialize;
use serde_json::json;

use crate::{analyze::Event, model::Model};

#[derive(Clone, Copy, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum Reason {
    Same,
    Ui,
    Mention,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Decision {
    id: usize,
    support: usize,
    why: Reason,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Edits {
    #[schemars(length(max = 6))]
    remove: Vec<Decision>,
}

fn has(text: &str, words: &[&str]) -> bool {
    words.iter().any(|word| text.contains(word))
}

pub(crate) fn resumes_play(e: &Event) -> bool {
    has(
        &e.heading,
        &[
            "プレイ再開",
            "プレイを再開",
            "ゲーム再開",
            "ゲームを再開",
            "ロードから再開",
        ],
    )
}

fn protected(e: &Event) -> bool {
    e.start == 0 || e.screen_label.is_some() || resumes_play(e)
}

pub(crate) fn conversation(e: &Event) -> bool {
    has(
        &e.basis,
        &[
            "会話",
            "台詞",
            "セリフ",
            "対話",
            "吹き出し",
            "発言",
            "と話",
            "と語",
            "と答",
            "と尋ね",
            "と告げ",
            "と説明",
            "と言",
            "語り",
            "話し",
            "話す",
            "問いかけ",
            "語る",
        ],
    ) || (e.ocr.contains('「') && e.ocr.contains('」') && !e.basis.trim().is_empty())
}

fn dialogue_evidence(e: &Event) -> bool {
    // A speaker or the word "conversation" alone does not identify its subject.
    conversation(e)
        && !has(
            &e.basis,
            &[
                "根拠不足",
                "同じ会話か不明",
                "話題は不明",
                "内容不明",
                "判別不能",
            ],
        )
        && (has(
            &e.basis,
            &[
                "「",
                "『",
                "について",
                "応じ",
                "相づち",
                "返答",
                "会話が続",
                "話を続け",
            ],
        ) || (e.basis.chars().count() >= 14
            && has(&e.basis, &["を", "という", "なぜ", "ため"]))
            || e.ocr
                .lines()
                .any(|line| line.chars().count() >= 6 && line.contains(['。', '？', '！', '「'])))
}

fn milestone(e: &Event) -> bool {
    has(
        &e.heading,
        &[
            "受注",
            "引き受け",
            "依頼開始",
            "依頼を開始",
            "依頼の開始",
            "依頼の導入",
            "クエスト開始",
            "完了",
            "達成",
            "再開",
            "更新",
            "進捗",
            "報酬",
            "獲得",
            "入手",
            "到着",
            "入場",
            "へ移動",
            "へ到達",
            "紹介",
            "発見",
            "判明",
            "解放",
            "解禁",
            "正体",
            "形態",
            "目的地を変更",
        ],
    ) || (has(&e.heading, &["依頼", "クエスト", "任務"])
        && has(
            &e.heading,
            &["開始", "新規", "新た", "受ける", "届ける", "納品"],
        ))
        || has(
            &e.basis,
            &[
                "受注画面",
                "受注の表示",
                "依頼書を表示",
                "完了表示",
                "達成表示",
                "結果画面",
                "地名表示",
                "地名が表示",
                "新しい場所",
                "別の場所",
                "別の場面",
                "場面が切り替わ",
                "報酬画面",
                "目標が更新",
                "依頼が更新",
                "再度依頼",
                "新たな依頼",
            ],
        )
        || crate::quest::is_offer_screen(&e.ocr)
}

fn battle_heading(heading: &str) -> bool {
    has(
        heading,
        &[
            "戦闘",
            "戦い",
            "ボス戦",
            "との戦",
            "決戦",
            "対決",
            "勝利",
            "敗北",
            "撃破",
            "討伐",
            "決着",
            "を倒",
            "戦開始",
        ],
    )
}

pub(crate) fn chapter_priority(event: &Event) -> u8 {
    if protected(event) {
        2
    } else if milestone(event) || battle_heading(&event.heading) {
        1
    } else {
        0
    }
}

fn action_on_screen(e: &Event) -> bool {
    // A spoken victory, or an explicit note that a health bar is absent, is
    // different from a visible result or attack. Check visual findings first.
    e.basis.split(['、', '。', '\n']).any(|clause| {
        !has(
            clause,
            &[
                "見えない",
                "未確認",
                "見当たら",
                "映らず",
                "確認できな",
                "表示はない",
                "映っていない",
                "表示されない",
                "出ていない",
                "言及のみ",
            ],
        ) && has(
            clause,
            &[
                "敵のHP",
                "敵の体力",
                "ボスのHP",
                "体力ゲージ",
                "HPゲージ",
                "戦闘画面",
                "戦闘UI",
                "攻撃モーション",
                "ダメージ数字",
                "ダメージ表示",
                "敵が倒れ",
                "敵が出現",
                "敵が現れ",
                "攻撃が命中",
                "武器を振り",
                "敵の姿が映",
                "戦闘が始ま",
                "攻撃エフェクト",
                "敵が消滅",
            ],
        )
    }) || crate::battle::result_at_proof(e)
}

fn routine_ui(e: &Event) -> bool {
    if !has(
        &e.heading,
        &[
            "セーブ",
            "保存",
            "スロット",
            "ロード",
            "装備選択",
            "装備の選択",
            "装備変更",
            "持ち物の確認",
        ],
    ) {
        return false;
    }
    [&e.basis, &e.ocr].into_iter().any(|text| {
        has(text, &["セーブ", "ロード", "保存", "装備", "持ち物"])
            && has(
                text,
                &[
                    "スロット",
                    "上書き",
                    "選択画面",
                    "装備一覧",
                    "データ一覧",
                    "保存先",
                    "確認ダイアログ",
                    "アイテム一覧",
                ],
            )
    })
}

fn eligible(e: &Event) -> bool {
    !protected(e) && !e.basis.trim().is_empty() && (conversation(e) || routine_ui(e))
}

fn same_setting(a: &Event, b: &Event) -> bool {
    // Explicitly different venues or named quests in the text are stronger
    // than an editor's recommendation based on the speakers or nearby seconds.
    if let (Some((left, _)), Some((right, _))) =
        (a.heading.split_once('で'), b.heading.split_once('で'))
        && left != right
    {
        return false;
    }
    let named = |e: &Event| {
        has(&e.heading, &["依頼", "クエスト", "任務"])
            .then(|| {
                e.heading
                    .split_once('「')?
                    .1
                    .split_once('」')
                    .map(|p| p.0.to_owned())
            })
            .flatten()
    };
    !matches!((named(a), named(b)), (Some(x), Some(y)) if x != y)
}

fn grounded(rows: &[Event], window: &[usize], d: Decision) -> bool {
    let Some(pos) = window.iter().position(|&i| i == d.id) else {
        return false;
    };
    let Some(other) = window.iter().position(|&i| i == d.support) else {
        return false;
    };
    let row = &rows[d.id];
    let support = &rows[d.support];
    if !eligible(row) || milestone(row) || action_on_screen(row) {
        return false;
    }
    if d.why == Reason::Ui {
        return d.support == d.id && routine_ui(row);
    }
    if pos == other
        || !dialogue_evidence(row)
        || support.basis.trim().is_empty()
        || row.start.abs_diff(support.start) > 150
        || window[pos.min(other) + 1..pos.max(other)]
            .iter()
            .any(|&i| protected(&rows[i]) || milestone(&rows[i]) || action_on_screen(&rows[i]))
    {
        return false;
    }
    match d.why {
        Reason::Same => {
            other < pos
                && !battle_heading(&row.heading)
                && same_setting(row, support)
                && (dialogue_evidence(support)
                    || support.screen_label.as_ref().is_some_and(|label| {
                        row.basis.contains(label)
                            && has(&row.basis, &["返答", "続き", "続け", "相づち", "応じ"])
                    }))
        }
        Reason::Mention => {
            battle_heading(&row.heading)
                && dialogue_evidence(support)
                && !battle_heading(&support.heading)
                && same_setting(row, support)
                && !action_on_screen(support)
        }
        Reason::Ui => false,
    }
}

pub fn refine(events: &mut Vec<Event>, model: &Model, game: &str) {
    review(events, game, |prompt| model.infer(prompt, &[]))
}

fn review(events: &mut Vec<Event>, game: &str, mut decide: impl FnMut(&str) -> Result<Edits>) {
    let mut ordered: Vec<_> = (0..events.len()).collect();
    ordered.sort_by_key(|&i| (events[i].start, i));
    let mut removed = BTreeSet::new();
    let mut supports = BTreeSet::new();
    let mut cursor = 0;
    while cursor < ordered.len() {
        let first = events[ordered[cursor]].start;
        let remaining = &ordered[cursor..(cursor + 6).min(ordered.len())];
        let end =
            cursor + remaining.partition_point(|&id| events[id].start.saturating_sub(first) <= 150);
        let targets = ordered[cursor..end]
            .iter()
            .copied()
            .filter(|&i| eligible(&events[i]) && !supports.contains(&i))
            .collect::<BTreeSet<_>>();
        if !targets.is_empty() {
            let last = events[ordered[end - 1]].start;
            let window: Vec<_> = ordered[cursor.saturating_sub(3)..(end + 2).min(ordered.len())]
                .iter()
                .copied()
                .filter(|&i| {
                    !removed.contains(&i)
                        && events[i].start >= first.saturating_sub(150)
                        && events[i].start <= last.saturating_add(150)
                })
                .collect();
            if window.len() == 1 && !routine_ui(&events[window[0]]) {
                cursor = end;
                continue;
            }
            let rows: Vec<_> = window.iter().map(|&i| {
                let e=&events[i];
                json!({"id":i,"start":e.start,"seen":e.seen,"heading":e.heading,"visual":e.basis.chars().take(160).collect::<String>(),
                    "screen":e.ocr.chars().take(220).collect::<String>(),"keep": protected(e)})
            }).collect();
            let prompt = format!(
                r#"作品『{game}』の近接した頭出し候補。start は導入、seen は根拠画像の再生秒。visual と screen はその画像から得た根拠。対象ID {targets:?} のうち、次の理由を画面根拠で確認できた行だけを整理する。根拠不足なら残す。
same: すでに始まった同じ会話で同じ依頼・話題に返答/相づち/言い換えをしただけ。同じ場面を代表する先の行を support にする。人物が同じ、時刻が近い、同じ名前というだけでは使わない。
ui: 保存先・ロードスロット・装備の選択のような通常操作だけで物語の進展がない。support は自分のID。
mention: 見出しは戦闘/勝利などを断言するが、本人と別の行の画面根拠を合わせると会話の中で触れただけ。support は同じ会話を裏づける別の行。実際の敵・攻撃・勝利画面が見えるときは保持。
新規依頼、依頼達成や再開、別の場所・発見、戦闘開始と決着など独立する変化は4秒差でも残す。keep が true の行は保持。同じ依頼の別段階も残す。見出し・時刻は書かない。根拠のある除外だけを JSON {{"remove":[{{"id":整数,"support":整数,"why":"same|ui|mention"}}]}} で返す。対象がなければ空配列。
{}"#,
                serde_json::Value::Array(rows)
            );
            let edits = match decide(&prompt) {
                Ok(edits) => edits,
                Err(error) => {
                    eprintln!("見出しの照合を続行できません。残りの観察を保持します ({error})");
                    break;
                }
            };
            // Conflicting recommendations for one row cannot establish removal.
            let mut by_id = BTreeMap::<usize, Option<Decision>>::new();
            for d in edits
                .remove
                .into_iter()
                .take(6)
                .filter(|d| targets.contains(&d.id))
            {
                by_id
                    .entry(d.id)
                    .and_modify(|entry| {
                        if *entry != Some(d) {
                            *entry = None
                        }
                    })
                    .or_insert(Some(d));
            }
            for d in by_id.into_values().flatten() {
                if !supports.contains(&d.id)
                    && !removed.contains(&d.support)
                    && grounded(events, &window, d)
                {
                    removed.insert(d.id);
                    if d.support != d.id {
                        supports.insert(d.support);
                    }
                    let reason = match d.why {
                        Reason::Same => "同じ会話の続き",
                        Reason::Ui => "通常の選択画面",
                        Reason::Mention => "会話内の言及",
                    };
                    eprintln!(
                        "見出し整理 {}秒の行 {}（{}、根拠行 {} の {}秒）",
                        events[d.id].start, d.id, reason, d.support, events[d.support].seen
                    );
                }
            }
        }
        cursor = end;
    }
    let mut id = 0;
    events.retain(|_| {
        let keep = !removed.contains(&id);
        id += 1;
        keep
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(start: u64, heading: &str, basis: &str) -> Event {
        Event {
            start,
            seen: start + 1,
            heading: heading.into(),
            basis: basis.into(),
            start_image: Some(format!("frames/{start}.jpg")),
            evidence_image: format!("frames/{}.jpg", start + 1),
            screen_label: None,
            ocr: String::new(),
        }
    }

    fn decision(id: usize, support: usize, why: Reason) -> Decision {
        Decision { id, support, why }
    }

    #[test]
    fn folds_a_reply_in_one_quest_but_preserves_independent_headings_four_seconds_apart() {
        let mut rows = vec![
            event(0, "旅の始まり", "旅人に話しかける"),
            event(
                18,
                "灯台守から船を探す頼み",
                "灯台守との会話。船の捜索を頼まれる",
            ),
            event(
                22,
                "船の捜索にうなずく",
                "同じ灯台守の会話に「探してみる」と応じる",
            ),
            event(
                26,
                "西門警備の依頼を受注",
                "別の依頼書を表示、警備隊長が港を守ってほしいと話す",
            ),
            event(30, "港の倉庫に到着", "地名表示が現れ、隊長との会話が始まる"),
            event(34, "灯台守へ報告し依頼完了", "灯台守の会話の後に完了表示"),
        ];
        let expected: Vec<_> = [0, 1, 3, 4, 5]
            .into_iter()
            .map(|i| serde_json::to_value(&rows[i]).unwrap())
            .collect();
        review(&mut rows, "合成例", |_| {
            Ok(Edits {
                remove: [0, 2, 3, 4, 5]
                    .into_iter()
                    .map(|id| decision(id, 1, Reason::Same))
                    .collect(),
            })
        });
        assert_eq!(
            rows.iter()
                .map(|e| serde_json::to_value(e).unwrap())
                .collect::<Vec<_>>(),
            expected
        );
    }

    #[test]
    fn drops_a_spoken_battle_claim_and_a_save_menu_but_keeps_an_actual_victory() {
        let mut rows = vec![
            event(
                10,
                "隊長が昔の戦を語る",
                "隊長との会話。昨日の襲撃について語る",
            ),
            event(
                13,
                "竜との戦闘に勝利",
                "会話の吹き出しに「昨日は竜に勝った」と表示",
            ),
            event(
                16,
                "保存先の選択",
                "セーブスロットの上書き確認ダイアログが出る",
            ),
            event(
                20,
                "竜との戦闘に勝利",
                "会話が出るが敵のHPゲージが空になり勝利画面が現れる",
            ),
            event(24, "プレイ再開", "セーブスロットの後、街の画面に戻る"),
            event(28, "銀の回廊", "会話中に今までの地名を再確認"),
        ];
        rows[5].screen_label = Some("銀の回廊".into());
        review(&mut rows, "合成例", |_| {
            Ok(Edits {
                remove: vec![
                    decision(1, 0, Reason::Mention),
                    decision(2, 2, Reason::Ui),
                    decision(3, 0, Reason::Mention),
                    decision(4, 4, Reason::Ui),
                    decision(5, 0, Reason::Same),
                ],
            })
        });
        assert_eq!(
            rows.iter().map(|e| e.start).collect::<Vec<_>>(),
            [10, 20, 24, 28]
        );
    }

    #[test]
    fn separate_venues_and_quests_uncertain_or_invalid_ids_leave_the_evidence_intact() {
        let mut rows = vec![
            event(30, "港で警備兵に相談", "警備兵との会話"),
            event(34, "街道で警備兵に相談", "警備兵との会話"),
            event(38, "依頼「青い灯」の説明", "旅商人との会話で道を示す"),
            event(42, "依頼「白い灯」の説明", "旅商人との会話で道を示す"),
            event(46, "扉が動く", "扉の動きが見える"),
            event(49, "警備兵からの返事", "警備兵との会話"),
        ];
        review(&mut rows, "合成例", |_| {
            Ok(Edits {
                remove: vec![
                    decision(1, 0, Reason::Same),
                    decision(3, 2, Reason::Same),
                    decision(4, 3, Reason::Same),
                    decision(5, 999, Reason::Same),
                    decision(5, 0, Reason::Same),
                    decision(999, 1, Reason::Ui),
                ],
            })
        });
        assert_eq!(rows.len(), 6);
        review(&mut rows, "合成例", |_| {
            anyhow::bail!("server unavailable")
        });
        assert_eq!(rows.len(), 6);
    }

    #[test]
    fn overview_at_a_batch_boundary_keeps_a_surviving_observation_as_its_reference() {
        let mut rows: Vec<_> = (1..=9)
            .map(|s| event(s * 7, "案内人の説明", "同じ案内人との会話が続く"))
            .collect();
        let mut requests = 0;
        review(&mut rows, "合成例", |_| {
            requests += 1;
            Ok(Edits {
                remove: if requests == 1 {
                    vec![decision(5, 4, Reason::Same)]
                } else {
                    vec![decision(6, 5, Reason::Same), decision(7, 4, Reason::Same)]
                },
            })
        });
        assert_eq!(requests, 2);
        assert_eq!(
            rows.iter().map(|e| e.start).collect::<Vec<_>>(),
            [7, 14, 21, 28, 35, 49, 63]
        );
    }
}
