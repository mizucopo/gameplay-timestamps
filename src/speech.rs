//! Timed language can nominate a moment missed by the full visual scan. Only a
//! second, image-based observation can add an event or move its introduction.
mod frames;

use std::{collections::BTreeSet, fs, path::Path};

use anyhow::Result;
use serde::Deserialize;
use serde_json::json;

use crate::{
    analyze::{Analyzer, Event, japanese},
    media::{Cue, Frame},
    sheet,
};

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Proposals {
    #[schemars(length(max = 4))]
    intervals: Vec<Interval>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct Interval {
    first: usize,
    last: usize,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct Moment {
    lead: u64,
    proof_start: u64,
    proof_end: u64,
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
    seen: u64,
    #[schemars(length(max = 36))]
    heading: String,
    #[schemars(length(max = 64))]
    basis: String,
    existing: Option<usize>,
}

pub fn supplement(
    a: &Analyzer<'_>,
    video: &Path,
    cues: &[Cue],
    events: &mut Vec<Event>,
) -> Result<()> {
    inspect(
        a,
        video,
        cues,
        events,
        |text| a.model.infer(text, &[]),
        |text, image| a.model.infer(text, image),
    )
}

fn inspect(
    a: &Analyzer<'_>,
    video: &Path,
    cues: &[Cue],
    events: &mut Vec<Event>,
    mut propose: impl FnMut(&str) -> Result<Proposals>,
    mut confirm: impl FnMut(&str, &[u8]) -> Result<Confirmations>,
) -> Result<()> {
    if cues.is_empty() {
        return Ok(());
    }
    let mut cursor = 0;
    let mut limit = 64;
    while cursor < cues.len() {
        // Bounded prompts even for dense speech; keep a little previous
        // dialogue so exchanges crossing a batch boundary retain their onset.
        let prior = cursor.saturating_sub((limit / 4).min(8));
        let begin =
            prior + cues[prior..cursor].partition_point(|c| c.start + 45. < cues[cursor].start);
        let end = cursor
            + cues[cursor..(cursor + limit).min(cues.len())]
                .partition_point(|c| c.start < cues[cursor].start + 240.);
        let batch = &cues[begin..end];
        let rows: Vec<_> = batch.iter().enumerate().map(|(id, cue)|
            json!({"id":id,"sec":cue.start,"text":cue.text.chars().take(96).collect::<String>()})
        ).collect();
        let new = cursor - begin;
        let prompt = format!(
            r#"作品『{}』版『{}』の時刻付き字幕・発話。物語の会話、依頼、発見、目的の変化など、映像を改めて調べる価値のある具体的な節目だけを最大4件選ぶ。移動中の実況・挨拶・相づち・繰り返し・推測・ゲーム外の話は省く。同じ会話はまとめる。各区間の first は導入を示す台詞 id、last は節目が分かる台詞 id。last は {new} 以上。根拠が弱ければ空配列。JSON {{"intervals":[{{"first":整数,"last":整数}}]}}。
{}"#,
            a.game,
            a.edition,
            serde_json::to_string(&rows)?
        );
        let proposed = match propose(&prompt) {
            Err(error) if end - cursor > 1 && crate::model::needs_smaller_input(&error) => {
                limit = (end - cursor).div_ceil(2);
                continue;
            }
            response => response?,
        };
        let mut intervals = BTreeSet::new();
        for p in proposed.intervals.into_iter().take(4) {
            if p.first > p.last || !(new..batch.len()).contains(&p.last) {
                continue;
            }
            let (lead, proof) = (&batch[p.first], &batch[p.last]);
            if proof.start - lead.start <= 180. {
                intervals.insert(Moment {
                    lead: lead.start.floor() as u64,
                    proof_start: proof.start.floor() as u64,
                    proof_end: (proof.end.ceil() as u64)
                        .saturating_sub(1)
                        .min(proof.start.ceil() as u64 + 1),
                });
            }
        }
        for group in intervals.into_iter().collect::<Vec<_>>().chunks(2) {
            verify(a, video, group, events, &mut confirm)?;
        }
        cursor = end;
        limit = (limit + 1).min(64);
    }
    events.sort_by_key(|event| event.start);
    Ok(())
}

struct View {
    starts: Vec<u64>,
    proofs: Vec<u64>,
    known: Vec<usize>,
}

fn verify(
    a: &Analyzer<'_>,
    video: &Path,
    intervals: &[Moment],
    events: &mut Vec<Event>,
    confirm: &mut impl FnMut(&str, &[u8]) -> Result<Confirmations>,
) -> Result<()> {
    let mut seconds = Vec::new();
    let mut views = Vec::new();
    for &Moment {
        lead,
        proof_start: proof,
        proof_end: end,
    } in intervals
    {
        let starts: Vec<_> = (lead.saturating_sub(3)..=lead + 1).collect();
        let proofs: Vec<_> = (proof..=end).collect();
        let mut nearby: Vec<_> = events
            .iter()
            .enumerate()
            .filter(|(_, e)| !matches!(e.heading.as_str(), "動画開始" | "プレイ再開"))
            .map(|(id, e)| {
                (
                    e.start
                        .abs_diff(lead)
                        .min(e.start.abs_diff(proof))
                        .min(e.seen.abs_diff(proof)),
                    id,
                )
            })
            .filter(|(distance, _)| *distance <= 12)
            .collect();
        nearby.sort_unstable();
        let known: Vec<_> = nearby.into_iter().take(2).map(|(_, id)| id).collect();
        seconds.extend(starts.iter().chain(&proofs).copied());
        seconds.extend(known.iter().map(|&id| events[id].seen));
        views.push(View {
            starts,
            proofs,
            known,
        });
    }
    let pictures = frames::sample(video, seconds)?;
    let mut rows = Vec::new();
    for (id, view) in views.iter_mut().enumerate() {
        view.starts.retain(|s| pictures.contains_key(s));
        view.proofs.retain(|s| pictures.contains_key(s));
        view.known
            .retain(|&i| pictures.contains_key(&events[i].seen));
        if view.starts.is_empty() || view.proofs.is_empty() {
            continue;
        }
        let known: Vec<_> = view
            .known
            .iter()
            .map(|&i| json!({"id":i,"seen":events[i].seen,"heading":events[i].heading}))
            .collect();
        rows.push(json!({"id":id,"starts":view.starts,"proofs":view.proofs,"known":known}));
    }
    if rows.is_empty() {
        return Ok(());
    }
    let prompt = format!(
        r#"作品『{}』版『{}』。元映像の画像だけから、各 id の近辺で独立に確認できる新しい節目を判定する。各コマの数字は再生秒。会話欄と人物の登場、画面に示された依頼や発見など、何が始まったかを特定できる画像が必要。変化のない探索・単なる動作・同じ会話の続き・画面から判別できない事柄は除き、根拠がなければ空配列。画面に現れない名前や事情を補わない。
同じ id の starts から導入が写る秒、proofs から根拠が写る秒を選ぶ。known の根拠画像と同じ出来事であれば existing にその id を返す。別の出来事は時刻が近くても独立して扱い、existing は null。複数の区間にまたがる同じ出来事は1件にまとめる。heading は日本語の短い見出し、basis は元の画像で確認した具体的な文字や変化。JSON {{"events":[{{"id":整数,"start":整数,"seen":整数,"heading":"見出し","basis":"画面上の根拠","existing":null}}]}}。
{}"#,
        a.game,
        a.edition,
        serde_json::to_string(&rows)?
    );
    let panels: Vec<_> = pictures.values().collect();
    let checked = match confirm(&prompt, &sheet::render(&panels, 3, 448)?) {
        Err(error) if intervals.len() > 1 && crate::model::needs_smaller_input(&error) => {
            for single in intervals.chunks(1) {
                verify(a, video, single, events, confirm)?;
            }
            return Ok(());
        }
        response => response?,
    };
    for c in checked.events.into_iter().take(4) {
        let Some(view) = views.get(c.id) else {
            continue;
        };
        let heading = c.heading.split_whitespace().collect::<Vec<_>>().join(" ");
        let basis = c.basis.split_whitespace().collect::<Vec<_>>().join(" ");
        if !view.starts.contains(&c.start)
            || !view.proofs.contains(&c.seen)
            || c.start > c.seen
            || !japanese(&heading)
            || matches!(heading.as_str(), "動画開始" | "プレイ再開")
            || basis.is_empty()
        {
            continue;
        }
        if let Some(id) = c.existing {
            if view.known.contains(&id)
                && c.start < events[id].start
                && events[id].screen_label.is_none()
            {
                let existing = &mut events[id];
                existing.start_image = Some(save(a.dir, &pictures[&c.start])?);
                existing.start = c.start;
            }
            continue;
        }
        if events
            .iter()
            .any(|e| e.heading == heading && (e.seen == c.seen || e.start == c.start))
        {
            continue;
        }
        let event = Event {
            start: c.start,
            seen: c.seen,
            heading,
            basis,
            start_image: Some(save(a.dir, &pictures[&c.start])?),
            evidence_image: save(a.dir, &pictures[&c.seen])?,
            screen_label: None,
            ocr: String::new(),
        };
        events.push(event);
    }
    Ok(())
}

fn save(dir: &Path, frame: &Frame) -> Result<String> {
    let relative = format!("frames/speech-{:08}.jpg", frame.second);
    let file = dir.join(&relative);
    if !file.try_exists()? {
        fs::write(file, &frame.jpeg)?;
    }
    Ok(relative)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Model;
    use anyhow::ensure;
    use std::process::Command;

    fn base() -> Vec<Event> {
        [("動画開始", 0, 0), ("門を開く依頼", 8, 9)]
            .map(|(heading, start, seen)| Event {
                heading: heading.into(),
                start,
                seen,
                basis: "元の画面の根拠".into(),
                ocr: "原文".into(),
                start_image: Some(format!("frames/{start:08}.jpg")),
                evidence_image: format!("frames/{seen:08}.jpg"),
                screen_label: None,
            })
            .into()
    }

    #[test]
    fn a_long_transcript_visits_every_cue_in_bounded_batches() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let model = Model::connect("http://localhost", Some("fixture".into()), None)?;
        let a = Analyzer {
            game: "架空の作品",
            edition: "",
            model: &model,
            dir: tmp.path(),
            use_ocr: false,
            ocr_command: None,
        };
        let cues: Vec<_> = (0..30 * 3600)
            .step_by(10)
            .map(|sec| Cue {
                start: sec as f64,
                end: sec as f64 + 2.,
                text: "村の風景についての実況".into(),
            })
            .collect();
        let mut visited = BTreeSet::new();
        let mut calls = 0;
        inspect(
            &a,
            &tmp.path().join("unused.mp4"),
            &cues,
            &mut base(),
            |prompt| {
                let rows: Vec<serde_json::Value> =
                    serde_json::from_str(prompt.lines().last().unwrap())?;
                assert!(rows.len() <= 72);
                visited.extend(rows.iter().map(|row| row["sec"].as_f64().unwrap() as u64));
                calls += 1;
                Ok(Proposals { intervals: vec![] })
            },
            |_, _| panic!("no nominated scene"),
        )?;
        assert_eq!(visited.len(), cues.len());
        assert!(calls < 500);
        Ok(())
    }

    #[test]
    fn cues_nominate_a_quiet_event_and_earlier_onset_only_with_matching_source_images() -> Result<()>
    {
        let tmp = tempfile::tempdir()?;
        fs::create_dir(tmp.path().join("frames"))?;
        let video = tmp.path().join("stories.mp4");
        let status = Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-f", "lavfi", "-i", "color=c=red:s=128x72:r=1:d=40",
                   "-vf", "drawbox=x=0:y=0:w=iw:h=ih:color=blue:t=fill:enable='between(t,3,5)',drawbox=x=0:y=0:w=iw:h=ih:color=green:t=fill:enable='between(t,6,10)'",
                   "-an", "-c:v", "mpeg4"])
            .arg(&video).status()?;
        ensure!(status.success(), "synthetic dialogue fixture");
        let model = Model::connect("http://localhost", Some("fixture".into()), None)?;
        let a = Analyzer {
            game: "架空の作品",
            edition: "",
            model: &model,
            dir: tmp.path(),
            use_ocr: false,
            ocr_command: None,
        };
        let mut silent = base();
        inspect(
            &a,
            &video,
            &[],
            &mut silent,
            |_| panic!("no timed language"),
            |_, _| panic!("no candidate"),
        )?;
        assert_eq!(silent.len(), 2);
        assert_eq!(silent[1].start, 8);
        assert_eq!(fs::read_dir(tmp.path().join("frames"))?.count(), 0);

        let cues: Vec<_> = [
            (3.2, "町長の話"),
            (4.1, "手紙を託そう"),
            (6., "門に来て"),
            (9., "開けてください"),
            (33., "竜と戦ったのかな"),
        ]
        .into_iter()
        .map(|(start, text)| Cue {
            start,
            end: start + 2.,
            text: text.into(),
        })
        .collect();
        let mut events = base();
        let propose = |text: &str| -> Result<Proposals> {
            assert!(text.contains("手紙を託そう"));
            Ok(Proposals {
                intervals: vec![
                    Interval { first: 0, last: 1 },
                    Interval { first: 2, last: 3 },
                    Interval { first: 4, last: 4 },
                ],
            })
        };
        let mut visually_checked = 0;
        inspect(&a, &video, &cues, &mut events, propose, |prompt, image| {
            assert!(!cues.iter().any(|cue| prompt.contains(&cue.text)));
            let rows: Vec<serde_json::Value> =
                serde_json::from_str(prompt.lines().last().unwrap())?;
            let labels: BTreeSet<_> = rows
                .iter()
                .flat_map(|row| {
                    row["starts"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .chain(row["proofs"].as_array().unwrap())
                        .map(|v| v.as_u64().unwrap())
                        .chain(
                            row["known"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .map(|e| e["seen"].as_u64().unwrap()),
                        )
                })
                .collect();
            let pixels = image::load_from_memory(image)?.into_rgb8();
            let color = |second| {
                let i = labels.iter().position(|&s| s == second).unwrap() as u32;
                pixels.get_pixel((i % 3) * 448 + 224, (i / 3) * 272 + 146).0
            };
            visually_checked += 1;
            if labels.contains(&33) {
                assert!(color(33)[0] > 220); // Gameplay stays red throughout this cue.
                return Ok(Confirmations { events: Vec::new() });
            }
            assert!(color(3)[2] > 220 && color(4)[2] > 220);
            assert!(color(6)[1] > 100 && color(9)[1] > 100);
            let found = |id, start, seen, heading: &str, existing| Confirmation {
                id,
                start,
                seen,
                heading: heading.into(),
                basis: "画面に切り替わった色付きの表示".into(),
                existing,
            };
            Ok(Confirmations {
                events: vec![
                    found(0, 3, 4, "町長の手紙", None),
                    found(0, 3, 4, "町長の手紙", None), // Repeated finding.
                    found(1, 6, 9, "門を開く依頼", Some(1)),
                    found(0, 3, 39, "架空の撃破", None), // No such proof panel.
                ],
            })
        })?;
        assert_eq!(visually_checked, 2);
        let headings: Vec<_> = events
            .iter()
            .map(|e| (e.start, e.seen, e.heading.as_str()))
            .collect();
        assert_eq!(
            headings,
            [
                (0, 0, "動画開始"),
                (3, 4, "町長の手紙"),
                (6, 9, "門を開く依頼")
            ]
        );
        assert_eq!(events[2].evidence_image, "frames/00000009.jpg");
        assert_eq!(events[2].ocr, "原文");
        for path in [
            events[1].start_image.as_ref().unwrap(),
            &events[1].evidence_image,
            events[2].start_image.as_ref().unwrap(),
        ] {
            assert!(image::open(tmp.path().join(path))?.width() > 0);
        }
        Ok(())
    }
}
