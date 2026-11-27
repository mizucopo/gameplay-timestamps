//! Read local recordings on the player's timeline. Frames are streamed from a
//! single FFmpeg decoder, including when audio and subtitle tracks are absent.

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};

pub struct Frame {
    pub second: u64,
    pub jpeg: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cue {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

#[derive(Deserialize)]
struct Probe {
    streams: Vec<Stream>,
    format: Format,
}

#[derive(Deserialize)]
struct Stream {
    index: usize,
    codec_type: String,
    start_time: Option<String>,
    tags: Option<Tags>,
}

#[derive(Deserialize)]
struct Format {
    start_time: Option<String>,
}

#[derive(Deserialize)]
struct Tags {
    language: Option<String>,
}

fn probe(video: &Path, only_video: bool) -> Result<Probe> {
    let mut cmd = Command::new("ffprobe");
    cmd.args(["-v", "error", "-protocol_whitelist", "file,pipe"]);
    if only_video {
        cmd.args(["-select_streams", "V:0"]);
    }
    let output = cmd
        .args([
            "-show_entries",
            "stream=index,codec_type,start_time:stream_tags=language:format=start_time",
            "-of",
            "json",
            "-i",
        ])
        .arg(video)
        .output()
        .context("ffprobe を起動できません")?;
    ensure!(
        output.status.success(),
        "ffprobe: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    serde_json::from_slice(&output.stdout).context("ffprobe の出力を読み取れません")
}

fn ffmpeg_input(video: &Path) -> Command {
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-nostdin",
        "-v",
        "error",
        "-xerror",
        "-protocol_whitelist",
        "file,pipe",
        "-i",
    ])
    .arg(video);
    cmd
}

/// Sample the screen displayed at each interval from the first video PTS.
/// FPS conversion uses presentation times, so a held VFR image remains
/// visible until the new image's PTS. A video track beginning after audio
/// retains that delay in the returned whole seconds.
pub fn scan(video: &Path, step: u32, mut on_frame: impl FnMut(Frame) -> Result<()>) -> Result<()> {
    ensure!(step > 0, "映像の走査間隔は 1 秒以上にしてください");
    let info = probe(video, true)?;
    let stream = info.streams.first().context("映像トラックがありません")?;
    // ffprobe reports start_time in microseconds. Subtract in that unit so
    // a one-second delay cannot round just below the second boundary.
    let micros = |raw: &Option<String>| {
        let seconds = raw.as_deref()?.parse::<f64>().ok()?;
        Some((seconds * 1_000_000.0).round() as i64)
    };
    let origin = micros(&info.format.start_time).unwrap_or_default();
    let first_second = micros(&stream.start_time)
        .unwrap_or(origin)
        .saturating_sub(origin)
        .max(0) as u64
        / 1_000_000;
    let filter = format!(
        "setpts=PTS-STARTPTS,fps=1/{step}:start_time=0:round=up:eof_action=pass,\
         scale=w='min(iw,1920)':h='min(ih,1920)':force_original_aspect_ratio=decrease:force_divisible_by=2"
    );
    let mut child = ffmpeg_input(video)
        .args([
            "-map", "0:V:0", "-vf", &filter, "-c:v", "mjpeg", "-q:v", "3", "-f", "mpjpeg", "pipe:1",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .context("ffmpeg を起動できません")?;
    let stdout = child.stdout.take().expect("piped ffmpeg stdout");
    let read_frames = || -> Result<()> {
        let mut source = BufReader::new(stdout);
        let mut second = first_second;
        while let Some(jpeg) = next_jpeg(&mut source)? {
            on_frame(Frame { second, jpeg })?;
            second += u64::from(step);
        }
        ensure!(
            second > first_second,
            "録画から映像フレームを取得できません"
        );
        Ok(())
    };
    let outcome = read_frames();
    if outcome.is_err() {
        let _ = child.kill();
    }
    let status = child.wait();
    outcome?;
    ensure!(status?.success(), "ffmpeg が映像の走査に失敗しました");
    Ok(())
}

// The mpjpeg muxer writes a Content-length header for each JPEG and a final
// boundary at EOF. Read exactly that many bytes, even if the payload itself
// contains bytes resembling a marker.
fn next_jpeg(source: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    let mut line = String::new();
    let mut length = None;
    loop {
        line.clear();
        if source.read_line(&mut line)? == 0 {
            ensure!(length.is_none(), "JPEG のヘッダーが途中で終了しました");
            return Ok(None);
        }
        let header = line.trim_end();
        if let Some(value) = header.strip_prefix("Content-length:") {
            length = Some(value.trim().parse::<usize>()?);
        } else if header.is_empty()
            && let Some(bytes) = length
        {
            ensure!(bytes > 0, "空の JPEG フレームです");
            let mut jpeg = vec![0; bytes];
            source
                .read_exact(&mut jpeg)
                .context("JPEG フレームが途中で終了しました")?;
            return Ok(Some(jpeg));
        }
    }
}

/// Read UTF-8 SRT/WebVTT beside the video (Japanese variants first), or one
/// embedded subtitle track, preferring Japanese. When enabled, the optional
/// local speech recognizer receives the video as its sole argument and must
/// emit SRT or WebVTT on stdout, relative to playback zero.
pub fn cues(video: &Path, use_audio: bool, transcriber: Option<&Path>) -> Vec<Cue> {
    let mut cues = ["ja.srt", "ja.vtt", "jpn.srt", "jpn.vtt", "srt", "vtt"]
        .iter()
        .filter_map(|ext| fs::read_to_string(video.with_extension(ext)).ok())
        .map(|text| parse_cues(&text))
        .find(|cues| !cues.is_empty())
        .unwrap_or_default();
    let Ok(info) = probe(video, false) else {
        return cues;
    };
    if cues.is_empty() {
        let mut tracks: Vec<_> = info
            .streams
            .iter()
            .filter(|s| s.codec_type == "subtitle")
            .collect();
        tracks.sort_by_key(|s| {
            let lang = s
                .tags
                .as_ref()
                .and_then(|t| t.language.as_deref())
                .unwrap_or("");
            !matches!(lang, "ja" | "jpn" | "ja-JP")
        });
        for track in tracks {
            let mut cmd = ffmpeg_input(video);
            cmd.args(["-map", &format!("0:{}", track.index), "-f", "srt", "pipe:1"]);
            if let Ok(found) = timed_output(&mut cmd)
                && !found.is_empty()
            {
                cues = found;
                break;
            }
        }
    }
    if let Some(program) = transcriber
        && use_audio
        && info.streams.iter().any(|s| s.codec_type == "audio")
    {
        match timed_output(Command::new(program).arg(video)) {
            Ok(speech) => cues.extend(speech),
            Err(error) => eprintln!("文字起こしをスキップします: {error}"),
        }
    }
    cues.sort_by(|a, b| {
        a.start
            .total_cmp(&b.start)
            .then(a.end.total_cmp(&b.end))
            .then(a.text.cmp(&b.text))
    });
    cues.dedup();
    cues
}

fn timed_output(command: &mut Command) -> Result<Vec<Cue>> {
    let output = command.stdin(Stdio::null()).output()?;
    ensure!(output.status.success(), "{}", output.status);
    Ok(parse_cues(std::str::from_utf8(&output.stdout)?))
}

fn parse_cues(input: &str) -> Vec<Cue> {
    let mut cues = Vec::new();
    let mut block = Vec::new();
    for line in input
        .trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .chain([""])
    {
        if line.is_empty() {
            if let Some(cue) = cue_block(&block) {
                cues.push(cue);
            }
            block.clear();
        } else {
            block.push(line);
        }
    }
    cues
}

fn cue_block(lines: &[&str]) -> Option<Cue> {
    let (first, rest) = lines.split_first()?;
    if matches!(
        first.split_whitespace().next()?,
        "WEBVTT" | "NOTE" | "STYLE" | "REGION"
    ) {
        return None;
    }
    let (timing, body) = if first.contains("-->") {
        (first, rest)
    } else {
        rest.split_first()?
    };
    let (start, end) = timing.split_once("-->")?;
    let start = subtitle_time(start.trim())?;
    let end = subtitle_time(end.split_whitespace().next()?)?;
    let mut text = String::new();
    let mut in_tag = false;
    for ch in body.join(" ").chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => text.push(ch),
            _ => {}
        }
    }
    let text = text
        .trim()
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    (start < end && !text.is_empty()).then_some(Cue { start, end, text })
}

fn subtitle_time(value: &str) -> Option<f64> {
    let mut fields = value.rsplit(':');
    let sec: f64 = fields.next()?.replace(',', ".").parse().ok()?;
    let min: u32 = fields.next()?.parse().ok()?;
    let hours: u64 = fields.next().unwrap_or("0").parse().ok()?;
    (fields.next().is_none() && min < 60 && (0.0..60.0).contains(&sec))
        .then_some(hours as f64 * 3600.0 + f64::from(min) * 60.0 + sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timed_caption_blocks_keep_playback_times_and_drop_non_cues() {
        let vtt = "\u{feff}WEBVTT\r\n\r\nNOTE memo\r\n00:00.000 --> 00:01.000\r\n余白\r\n\r\nline-A\r\n02:40:10.125 --> 02:40:12.200 align:start\r\n<v 旅人>ここは\r\nどこ？</v>\r\n\r\n00:90.000 --> 00:91.000\r\n無効\r\n";
        assert_eq!(
            parse_cues(vtt),
            vec![Cue {
                start: 9610.125,
                end: 9612.2,
                text: "ここは どこ？".to_string(),
            }]
        );
    }

    #[test]
    fn an_incomplete_jpeg_is_never_emitted_as_evidence() {
        assert!(next_jpeg(&mut &b"Content-length: 4\r\n\r\nab"[..]).is_err());
    }

    fn make_video(args: &[&str], path: &Path) -> Result<()> {
        let output = Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error"])
            .args(args)
            .arg(path)
            .output()?;
        ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    #[test]
    fn silent_vfr_samples_the_screen_at_real_elapsed_seconds() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let video = dir.path().join("recording.mp4");
        make_video(
            &[
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=32x32:r=4:d=1",
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:s=32x32:r=4:d=1",
                "-filter_complex",
                "[0:v][1:v]concat=n=2:v=1:a=0,select='eq(n,0)+eq(n,4)+eq(n,5)',setpts='if(eq(N,0),0,if(eq(N,1),2.25,3.25))/TB'",
                "-c:v",
                "mpeg4",
                "-enc_time_base",
                "1:4",
                "-fps_mode",
                "vfr",
                "-output_ts_offset",
                "7",
            ],
            &video,
        )?;
        let mut frames = Vec::new();
        scan(&video, 1, |frame| {
            frames.push(frame);
            Ok(())
        })?;
        assert_eq!(
            frames.iter().map(|f| f.second).collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );
        assert_eq!(frames[0].jpeg, frames[2].jpeg);
        assert_ne!(frames[2].jpeg, frames[3].jpeg); // First blue PTS is 2.25 s.
        assert!(cues(&video, true, None).is_empty());
        fs::write(
            video.with_extension("ja.vtt"),
            "WEBVTT\n\n00:01.000 --> 00:02.250\n洞窟へ\n",
        )?;
        assert_eq!(cues(&video, false, None)[0].text, "洞窟へ");
        Ok(())
    }

    #[test]
    fn delayed_video_and_embedded_text_share_the_audio_playback_origin() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let video = dir.path().join("recording.mp4");
        let subtitles = dir.path().join("track.srt");
        fs::write(&subtitles, "1\n00:00:00,800 --> 00:00:01,300\n出発\n")?;
        make_video(
            &[
                "-itsoffset",
                "1.25",
                "-f",
                "lavfi",
                "-i",
                "color=c=blue:s=32x32:r=4:d=2.25",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=d=4.5",
                "-i",
                subtitles.to_str().unwrap(),
                "-map",
                "0:v",
                "-map",
                "1:a",
                "-map",
                "2:s",
                "-c:v",
                "mpeg4",
                "-c:a",
                "aac",
                "-c:s",
                "mov_text",
                "-fps_mode",
                "passthrough",
            ],
            &video,
        )?;
        let mut times = Vec::new();
        scan(&video, 1, |frame| {
            times.push(frame.second);
            Ok(())
        })?;
        assert_eq!(times, [1, 2, 3]);
        let expected = vec![Cue {
            start: 0.8,
            end: 1.3,
            text: "出発".to_string(),
        }];
        assert_eq!(cues(&video, false, None), expected);
        assert_eq!(
            cues(&video, true, Some(&dir.path().join("missing-transcriber"))),
            expected
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let recognizer = dir.path().join("local-recognizer");
            fs::write(
                &recognizer,
                "#!/bin/sh\ntest -f \"$1\" || exit 1\nprintf 'WEBVTT\\n\\n00:02.000 --> 00:02.750\\n発話です\\n'\n",
            )?;
            fs::set_permissions(&recognizer, fs::Permissions::from_mode(0o700))?;
            assert_eq!(cues(&video, false, Some(&recognizer)), expected);
            let with_speech = cues(&video, true, Some(&recognizer));
            assert_eq!(with_speech.len(), 2);
            assert_eq!(
                with_speech[1],
                Cue {
                    start: 2.0,
                    end: 2.75,
                    text: "発話です".to_string()
                }
            );
        }
        Ok(())
    }
}
