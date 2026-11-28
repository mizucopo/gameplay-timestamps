//! Locate the interaction that began a conversation whose outcome was only
//! identifiable later. Keep the original proof and the time on its picture.
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;
use image::{ImageFormat, codecs::jpeg::JpegEncoder, imageops::FilterType};

use crate::{
    analyze::{Analyzer, Event, save},
    media::{Cue, Frame},
    speech::frames::sample,
};

const LOOKBACK: u64 = 180;
const MIN_LEAD: u64 = 6;

fn later_outcome(e: &Event) -> bool {
    e.start > 0
        && e.start <= e.seen
        && e.screen_label.is_none()
        && (crate::editor::conversation(e)
            || e.heading.contains("会話")
            || e.heading.contains("対話")
            || (e.basis.contains('「') && (e.basis.contains("応え") || e.basis.contains("発し"))))
        && [
            "感謝",
            "礼を",
            "お礼",
            "御礼",
            "報告",
            "事情",
            "打ち明け",
            "明か",
            "告げ",
            "依頼",
            "引き受",
            "協力を求め",
            "情報",
            "教え",
            "約束",
            "和解",
            "仲直り",
            "謝罪",
            "相談",
            "頼み",
        ]
        .iter()
        .any(|word| e.heading.contains(word))
}

/// A conversation with a separate introduction and outcome gives viewers a
/// chapter to seek to even when the new start is close to another entry.
pub(crate) fn has_conversation_intro(e: &Event) -> bool {
    later_outcome(e)
        && e.seen.saturating_sub(e.start) >= MIN_LEAD
        && e.start_image
            .as_deref()
            .is_some_and(|image| image != e.evidence_image)
}

fn letters(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).collect()
}

fn spoken(s: &str) -> bool {
    s.chars().filter(|c| c.is_alphanumeric()).count() >= 4
        && s.chars().any(|c| ('ぁ'..='ん').contains(&c))
}

// A quiet interval nominates an onset, but voiced gameplay and commentary
// also have pauses. The actual subtitle at the beginning must match the audio.
fn audio_start(cues: &[Cue], floor: u64, seen: u64) -> Option<u64> {
    let end = cues.partition_point(|c| c.start <= (seen + 2) as f64);
    let mut opening = (seen + 2) as f64;
    let mut turns = 0;
    for cue in cues[..end].iter().rev().filter(|c| spoken(&c.text)) {
        if cue.end + if turns == 0 { 4.0 } else { 9.0 } < opening {
            break;
        }
        if cue.start < floor as f64 {
            return None;
        }
        opening = cue.start;
        turns += 1;
    }
    (turns >= 3 && opening + 5.0 <= seen as f64).then_some(opening.floor() as u64)
}

fn shares_words(a: &str, b: &str) -> bool {
    let a: Vec<_> = letters(a).chars().collect();
    let b = letters(b);
    a.windows(4)
        .any(|w| b.contains(&w.iter().collect::<String>()))
}

fn topic(text: &str, heading: &str) -> bool {
    let subject = heading
        .split(['が', 'と', 'に', 'へ', 'を', 'の'])
        .next()
        .unwrap_or("");
    (subject.chars().count() >= 2 && text.contains(subject))
        || text.lines().any(|row| {
            let word = letters(row);
            (word.chars().count() >= 2 && heading.contains(&word)) || shares_words(&word, heading)
        })
}

// No voice track or fade is required for an in-place NPC interaction.
// Follow the displayed dialogue back across short page turns, then inspect
// only the seconds bracketing the first caption for a disappearing command.
fn first_caption(
    frames: &BTreeMap<u64, Frame>,
    floor: u64,
    seen: u64,
    a: &Analyzer<'_>,
) -> Result<Option<u64>> {
    let mut first = None;
    for (&t, f) in frames.range(floor..=seen).rev().step_by(3) {
        if first.unwrap_or(seen) - t > if first.is_some() { 15 } else { 6 } {
            break;
        }
        let words = read_region(f, false, a)?;
        if crate::map::cursor_control(&words) {
            break;
        }
        if words.lines().any(spoken) {
            first = Some(t);
        }
    }
    Ok(first.filter(|t| *t + 5 < seen))
}

struct View {
    dark: bool,
    scene: Vec<u8>,
}

impl View {
    fn read(frame: &Frame) -> Result<Self> {
        let view = image::load_from_memory_with_format(&frame.jpeg, ImageFormat::Jpeg)?
            .thumbnail_exact(32, 18)
            .into_luma8();
        let dark = view.iter().filter(|&&v| v >= 22).count() * 100 < view.len() * 3;
        // Compare background above the subtitle panel and away from the HUD
        // edges. Moving faces and actors can occupy part of a fixed shot.
        let scene = (2..12)
            .flat_map(|y| (2..30).map(move |x| (x, y)))
            .map(|(x, y)| view[(x, y)][0])
            .collect();
        Ok(Self { dark, scene })
    }

    fn same_shot(&self, other: &Self) -> bool {
        self.scene
            .iter()
            .zip(&other.scene)
            .filter(|(a, b)| a.abs_diff(**b) < 18)
            .count()
            * 3
            >= self.scene.len() * 2
    }
}

fn read_region(frame: &Frame, control: bool, a: &Analyzer<'_>) -> Result<String> {
    let image = image::load_from_memory_with_format(&frame.jpeg, ImageFormat::Jpeg)?;
    let (w, h) = (image.width(), image.height());
    let region = if control {
        // Interaction prompts can be small and attached to the character.
        image
            .crop_imm(w / 4, h * 3 / 10, (w / 2).max(1), (h / 2).max(1))
            .resize_exact(w, h, FilterType::Triangle)
    } else {
        image.crop_imm(0, h * 3 / 5, w, h - h * 3 / 5)
    };
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, 92).encode_image(&region)?;
    crate::ocr::read(&jpeg, a.ocr_command)
}

fn talk_control(text: &str) -> bool {
    text.lines().any(|line| {
        let words = letters(line).to_uppercase();
        ["話す", "話しかける", "会話する", "TALK", "SPEAK"]
            .iter()
            .any(|cmd| {
                words.strip_suffix(cmd).is_some_and(|button| {
                    matches!(
                        button,
                        "" | "A"
                            | "B"
                            | "X"
                            | "Y"
                            | "L"
                            | "R"
                            | "E"
                            | "RT"
                            | "LT"
                            | "ZR"
                            | "ZL"
                            | "R2"
                            | "L2"
                    )
                })
            })
    })
}

fn interaction(
    frames: &BTreeMap<u64, Frame>,
    seconds: impl IntoIterator<Item = u64>,
    a: &Analyzer<'_>,
) -> Result<Option<u64>> {
    let mut last = None;
    let mut count = 0;
    let mut action = None;
    for t in seconds {
        let Some(frame) = frames.get(&t) else {
            continue;
        };
        let text = read_region(frame, true, a)?;
        if talk_control(&text) || talk_control(&crate::ocr::read(&frame.jpeg, a.ocr_command)?) {
            count = if last == t.checked_sub(1) {
                count + 1
            } else {
                1
            };
            last = Some(t);
        } else if count >= 2 && last == t.checked_sub(1) {
            action = last;
            count = 0;
        }
    }
    Ok(action)
}

fn near_captions(
    frames: &BTreeMap<u64, Frame>,
    centers: &[u64],
    floor: u64,
    seen: u64,
    a: &Analyzer<'_>,
) -> Result<Option<u64>> {
    let seconds: BTreeSet<_> = centers
        .iter()
        .flat_map(|t| t.saturating_sub(2).max(floor)..=t.saturating_add(7).min(seen))
        .collect();
    interaction(frames, seconds, a)
}

fn corroborated(
    frames: &BTreeMap<u64, Frame>,
    start: u64,
    seen: u64,
    heading: &str,
    cues: &[Cue],
    a: &Analyzer<'_>,
) -> Result<(bool, bool)> {
    let mut first = None;
    let mut last = start;
    let mut lines = BTreeSet::new();
    let mut early_topic = false;
    let mut echo = false;
    for (&t, frame) in frames.range(start.saturating_add(1)..=seen) {
        // A silent game may clear its text box between every short page. Read
        // densely at the opening and whenever a sparse sample loses the text.
        if t - start > 22 && t + 3 < seen && t - last < 8 && !(t - start).is_multiple_of(3) {
            continue;
        }
        let text = read_region(frame, false, a)?;
        if crate::map::cursor_control(&text) || crate::quest::is_offer_screen(&text) {
            return Ok((false, false));
        }
        if t - start <= 22 && topic(&text, heading) {
            early_topic = true;
        }
        if text.lines().any(spoken) {
            if t - last > 21 {
                return Ok((false, false));
            }
            first.get_or_insert(t);
            last = t;
            lines.insert(letters(&text));
            if t - start <= 9 {
                let lower = cues.partition_point(|c| c.start < t.saturating_sub(12) as f64);
                let upper = cues.partition_point(|c| c.start <= (t + 2) as f64);
                echo |= cues[lower..upper]
                    .iter()
                    .any(|c| c.end + 2.0 >= t as f64 && shares_words(&c.text, &text));
            }
        }
    }
    let continuous = first.is_some_and(|t| t - start <= 13) && last + 6 >= seen && lines.len() >= 3;
    Ok((continuous && (early_topic || echo), echo))
}

fn revise(e: &mut Event, a: &Analyzer<'_>, video: &Path, cues: &[Cue], floor: u64) -> Result<()> {
    let floor = floor.max(e.seen.saturating_sub(LOOKBACK));
    if floor >= e.start || floor + 6 >= e.seen {
        return Ok(());
    }
    let frames = sample(video, (floor..=e.seen).collect())?;
    let mut views = BTreeMap::new();
    let mut found_dark = false;
    for (&t, frame) in frames.iter().rev() {
        let view = View::read(frame)?;
        let before = found_dark && !view.dark;
        found_dark |= view.dark;
        views.insert(t, view);
        if before {
            break;
        }
    }
    let Some(proof) = views.get(&e.seen) else {
        return Ok(());
    };
    if proof.dark {
        return Ok(());
    }
    let fade_end = views
        .range(..e.seen)
        .rev()
        .find(|(_, v)| v.dark)
        .map(|(&t, _)| t);
    let field = fade_end.map_or(floor, |t| t + 1);
    let audio = audio_start(cues, field, e.seen);
    let mut centers = Vec::new();
    if let Some(t) = audio {
        centers.push(t);
    }
    if fade_end.is_some() {
        centers.push(field);
    }
    let action = near_captions(&frames, &centers, field, e.seen, a)?;
    let fade = fade_end.and_then(|end| {
        let before = views
            .range(..end)
            .rev()
            .find(|(_, v)| !v.dark)
            .map(|(&t, v)| (t, v))?;
        let first = before.0 + 1;
        if field + 5 >= e.seen {
            return None;
        }
        let mut stable = views.range((field + 1)..=e.seen).step_by(3).map(|(_, v)| v);
        (end - first < 8 && !before.1.same_shot(proof) && stable.all(|v| v.same_shot(proof)))
            .then_some(first)
    });
    let action = if action.is_none()
        && audio.is_none()
        && fade.is_none()
        && let Some(first) = first_caption(&frames, field, e.seen, a)?
    {
        near_captions(&frames, &[first.saturating_sub(7)], field, e.seen, a)?
    } else {
        action
    };
    let Some(proposed) = action.or(audio).or(fade) else {
        return Ok(());
    };
    if proposed >= e.start || e.seen.saturating_sub(proposed) < MIN_LEAD {
        return Ok(());
    }
    // A second talk command may begin another character's conversation
    // without a fade or a long pause between the two sets of subtitles.
    let action = interaction(&frames, proposed + 1..=e.seen, a)?.or(action);
    let start = action.unwrap_or(proposed);
    if start >= e.start || e.seen.saturating_sub(start) < MIN_LEAD {
        return Ok(());
    }
    let (connected, echo) = corroborated(&frames, start, e.seen, &e.heading, cues, a)?;
    if !connected || (action.is_none() && audio == Some(start) && !echo) {
        return Ok(());
    }
    let Some(frame) = frames.get(&start) else {
        return Ok(());
    };
    let image = save(a.dir, frame)?;
    let basis = if action.is_some() {
        "話す操作の表示から会話に切り替わる"
    } else if audio.is_some() {
        "時刻付き発話の開始を画面の台詞と照合"
    } else {
        "暗転から同じ画面で続く会話に切り替わる"
    };
    e.start = start;
    e.start_image = Some(image);
    if !e.basis.ends_with(['。', '！', '？']) {
        e.basis.push('。');
    }
    e.basis.push_str(&format!("{start}秒に{basis}。"));
    Ok(())
}

pub(crate) fn refine(events: &mut [Event], a: &Analyzer<'_>, video: &Path, cues: &[Cue]) {
    if !a.use_ocr {
        return;
    }
    let boundaries: BTreeSet<_> = events
        .iter()
        .filter(|e| {
            e.screen_label.is_some()
                || crate::editor::resumes_play(e)
                || crate::battle::result_at_proof(e)
        })
        .map(|e| e.seen)
        .collect();
    for e in events.iter_mut().filter(|e| later_outcome(e)) {
        let floor = boundaries
            .range(..e.seen)
            .next_back()
            .copied()
            .unwrap_or_default();
        if let Err(error) = revise(e, a, video, cues, floor) {
            eprintln!("{}秒の会話の導入を再確認できません ({error})", e.seen);
        }
    }
    events.sort_by_key(|e| e.start);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_unrelated_prompts_and_earlier_speech() {
        assert!(talk_control("RT 話す") && talk_control("A TALK"));
        assert!(!talk_control("店主と話すのが好きだ。") && !talk_control("会話ログ"));
        let cues: Vec<_> = [
            (1, 4, "道案内をお願いします"),
            (20, 23, "お久しぶりですね"),
            (24, 29, "薬草を持ってきました"),
            (32, 36, "ありがとうございました"),
        ]
        .into_iter()
        .map(|(start, end, text)| Cue {
            start: start as f64,
            end: end as f64,
            text: text.to_owned(),
        })
        .collect();
        assert_eq!(audio_start(&cues, 0, 34), Some(20));
        assert_eq!(audio_start(&cues, 27, 34), None);
    }

    #[cfg(unix)]
    #[test]
    fn silent_in_place_interaction_preserves_the_separate_outcome_picture() -> Result<()> {
        use std::{fs, os::unix::fs::PermissionsExt, process::Command};
        let dir = tempfile::tempdir()?;
        let video = dir.path().join("conversation.mp4");
        let ocr = dir.path().join("game-ocr");
        let filters = [
            "drawbox=color=0xbf7038:t=fill:enable='between(t,9,10)'",
            "drawbox=color=0x305a93:t=fill:enable='between(t,11,19)'",
            "drawbox=color=0x3072a5:t=fill:enable='between(t,20,27)'",
            "drawbox=color=0x3091ad:t=fill:enable='gte(t,28)'",
        ]
        .join(",");
        assert!(
            Command::new("ffmpeg")
                .args([
                    "-nostdin",
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=gray:s=96x54:r=1:d=37",
                    "-vf",
                    &filters,
                    "-c:v",
                    "mpeg4"
                ])
                .arg(&video)
                .status()?
                .success()
        );
        fs::write(
            &ocr,
            r#"#!/usr/bin/env python3
import subprocess,sys
pixel=subprocess.check_output(["ffmpeg","-v","error","-i","pipe:0","-vf","scale=1:1","-frames:v","1","-f","rawvideo","-pix_fmt","rgb24","pipe:1"],input=sys.stdin.buffer.read())
r,g,b=pixel
if r>150 and g<150: print("A 話す")
if 110<r<150 and b>130: print("リオ\n初めてこの街へ来たのですか。")
if r<80 and b>125:
    print("セナ")
    print("誰かと思ったら、こんばんは。" if g<103 else "珍しい薬草を預かりました。" if g<125 else "持ち帰ってくださって、ありがとう。")
"#,
        )?;
        fs::set_permissions(&ocr, fs::Permissions::from_mode(0o755))?;
        fs::create_dir(dir.path().join("frames"))?;
        let model =
            crate::model::Model::connect("http://127.0.0.1:1/v1", Some("unused".into()), None)?;
        let analyzer = Analyzer {
            game: "試験",
            edition: "",
            model: &model,
            dir: dir.path(),
            use_ocr: true,
            ocr_command: Some(&ocr),
        };
        let originals = sample(&video, vec![8, 10, 33, 34])?;
        let evidence = save(dir.path(), &originals[&34])?;
        let event = |heading: &str| Event {
            start: 34,
            seen: 34,
            heading: heading.into(),
            basis: "礼を言う台詞が表示された".into(),
            ocr: "34 セナ　持ち帰ってくださって、ありがとう。".into(),
            screen_label: None,
            start_image: Some(evidence.clone()),
            evidence_image: evidence.clone(),
        };
        let mut unrelated = event("港の船長との和解");
        revise(&mut unrelated, &analyzer, &video, &[], 0)?;
        assert_eq!(unrelated.start, 34);
        let mut partial = event("セナとの感謝の会話");
        partial.start = 33;
        partial.start_image = Some(save(dir.path(), &originals[&33])?);
        let mut earlier = event("セナとの感謝の会話");
        earlier.start = 8;
        earlier.start_image = Some(save(dir.path(), &originals[&8])?);
        let mut rows = [partial, earlier];
        refine(&mut rows, &analyzer, &video, &[]);
        assert_eq!((rows[0].start, rows[0].seen), (8, 34));
        assert_eq!((rows[1].start, rows[1].seen), (10, 34));
        assert_eq!(
            fs::read(dir.path().join(rows[1].start_image.as_ref().unwrap()))?,
            originals[&10].jpeg
        );
        assert_eq!(
            fs::read(dir.path().join(&rows[1].evidence_image))?,
            originals[&34].jpeg
        );
        let second = dir.path().join("two-conversations.mp4");
        let changes = [
            "drawbox=color=0x83719c:t=fill:enable='between(t,11,20)'",
            "drawbox=color=0xbf7038:t=fill:enable='between(t,21,22)'",
            "drawbox=color=0x305a93:t=fill:enable='between(t,23,26)'",
        ]
        .join(",");
        assert!(
            Command::new("ffmpeg")
                .args(["-nostdin", "-v", "error", "-i"])
                .arg(&video)
                .args(["-vf", &changes, "-c:v", "mpeg4"])
                .arg(&second)
                .status()?
                .success()
        );
        let mut last_speaker = event("セナとの感謝の会話");
        revise(&mut last_speaker, &analyzer, &second, &[], 0)?;
        assert_eq!((last_speaker.start, last_speaker.seen), (22, 34));
        Ok(())
    }
}
