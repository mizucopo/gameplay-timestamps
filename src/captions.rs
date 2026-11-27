//! Name visually established events using timed speech and sourced spellings.
//! The expensive observation pass is identical across the four evaluation conditions.
use anyhow::Result;
use serde::Deserialize;
use serde_json::json;

use crate::{
    analyze::Event,
    dictionary::{Dictionary, Evidence},
    media::Cue,
    model::Model,
};

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Titles {
    #[schemars(length(max = 8))]
    headings: Vec<Title>,
}
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Title {
    id: usize,
    #[schemars(length(max = 36))]
    heading: String,
}

fn preserves_travel(original: &str, proposed: &str) -> bool {
    // Renaming may clarify the destination. Opening a map or discussing a
    // place does not become an actual arrival when a subtitle names it.
    crate::map::movement_destination(proposed).is_none()
        || crate::map::movement_destination(original).is_some()
}

pub fn enrich(
    events: &mut [Event],
    cues: &[Cue],
    dictionary: &Dictionary,
    model: &Model,
    game: &str,
) -> Result<()> {
    // media::cues orders by start; prefix maxima also handle overlapping
    // captions and ASR segments with different end times.
    let latest_end: Vec<_> = cues
        .iter()
        .scan(0.0_f64, |end, cue| {
            *end = end.max(cue.end);
            Some(*end)
        })
        .collect();
    let near = |time: u64| {
        let lower = time.saturating_sub(3) as f64;
        let from = latest_end.partition_point(|&end| end <= lower);
        let until = cues.partition_point(|cue| cue.start <= time as f64 + 4.);
        (from..until).filter(move |&i| cues[i].end > lower)
    };
    for batch in events.chunks_mut(8) {
        let evidence: Vec<_> = batch
            .iter()
            .map(|e| Evidence::new(e.seen, &e.ocr, &e.basis))
            .collect();
        let mut requests = Vec::new();
        for (id, event) in batch.iter_mut().enumerate() {
            if matches!(event.heading.as_str(), "動画開始" | "プレイ再開") {
                continue;
            }
            let screen = &evidence[id];
            let hints = dictionary.hints(&event.heading, screen);
            event.heading = dictionary.resolve(&event.heading, screen);
            if event.screen_label.is_some() {
                continue;
            }
            let mut indices: Vec<_> = near(event.start)
                .take(8)
                .chain(near(event.seen).take(8))
                .collect();
            indices.sort_unstable();
            indices.dedup();
            let speech = indices
                .into_iter()
                .take(8)
                .map(|i| &cues[i])
                .map(|c| json!({"sec":c.start,"text":c.text.chars().take(180).collect::<String>()}))
                .collect::<Vec<_>>();
            if !speech.is_empty() || !hints.is_empty() {
                // When proposing dictionary spellings, show the OCR that grounds them.
                let ocr = if hints.is_empty() {
                    &event.ocr
                } else {
                    &screen.ocr
                };
                requests.push(
                    json!({"id":id,"heading":event.heading,"visual":event.basis,"ocr":ocr,
                                     "spellings":hints,"captions":speech}),
                );
            }
        }
        if requests.is_empty() {
            continue;
        }
        let prompt = format!(
            r#"ゲーム『{game}』。各 id は前後の画像で確認済みの独立した出来事です。映像根拠 visual/ocr と一致する登場人物・地名等だけ、同じ秒の字幕/発話 captions と辞書 spellings で表記を照合し、日本語の短い見出しを整えてください。矛盾する発話・似た別の形態・攻略情報は採用しません。十分な根拠が無ければ元の見出しを返すこと。出来事を新しく作らないこと。各 id に1件だけ JSON {{"headings":[{{"id":整数,"heading":"日本語見出し"}}]}}。
{}"#,
            serde_json::to_string(&requests)?
        );
        let result: Titles = model.infer(&prompt, &[])?;
        for title in result.headings {
            if !requests
                .iter()
                .any(|r| r["id"].as_u64() == Some(title.id as u64))
            {
                continue;
            }
            let event = &mut batch[title.id];
            let heading = title
                .heading
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if crate::analyze::japanese(&heading) && preserves_travel(&event.heading, &heading) {
                event.heading = dictionary.resolve(&heading, &evidence[title.id]);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captions_can_name_a_journey_without_turning_a_map_or_a_conversation_into_one() {
        for title in [
            "雪の港に到着",
            "雪の港への到着",
            "雪の港に着いた",
            "雪の港への移動",
        ] {
            assert!(!preserves_travel("港の地図を開く", title));
            assert!(!preserves_travel("船員と行き先を話す", title));
            assert!(preserves_travel("次の港へ移動", title));
        }
        assert!(preserves_travel("港の地図を開く", "雪の港の地図を開く"));
    }
}
