//! Name visually established events using timed speech and sourced spellings.
//! The expensive observation pass is identical across the four evaluation conditions.
use anyhow::Result;
use serde::Deserialize;
use serde_json::json;

use crate::{analyze::Event, dictionary::Dictionary, media::Cue, model::Model};

#[derive(Deserialize, schemars::JsonSchema)]
struct Titles {
    headings: Vec<Title>,
}
#[derive(Deserialize, schemars::JsonSchema)]
struct Title {
    id: usize,
    heading: String,
}

pub fn enrich(
    events: &mut [Event],
    cues: &[Cue],
    dictionary: &Dictionary,
    model: &Model,
    game: &str,
) -> Result<()> {
    for batch in events.chunks_mut(8) {
        let mut requests = Vec::new();
        for (id, event) in batch.iter_mut().enumerate() {
            if matches!(event.heading.as_str(), "動画開始" | "プレイ再開") {
                continue;
            }
            let evidence = format!("{} {}", event.ocr, event.basis);
            let hints = dictionary.hints(&evidence);
            event.heading = dictionary.resolve(&event.heading, &evidence);
            let relevant = |cue: &Cue| {
                [event.start, event.seen].into_iter().any(|time| {
                    cue.start <= time as f64 + 4. && cue.end > time.saturating_sub(3) as f64
                })
            };
            let speech = cues
                .iter()
                .filter(|c| relevant(c))
                .take(8)
                .map(|c| json!({"sec":c.start,"text":c.text.chars().take(180).collect::<String>()}))
                .collect::<Vec<_>>();
            if !speech.is_empty() || !hints.is_empty() {
                requests.push(
                    json!({"id":id,"heading":event.heading,"visual":event.basis,"ocr":event.ocr,
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
            if crate::analyze::japanese(&heading) {
                event.heading =
                    dictionary.resolve(&heading, &format!("{} {}", event.ocr, event.basis));
            }
        }
    }
    Ok(())
}
