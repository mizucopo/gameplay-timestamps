use std::{collections::BTreeMap, fs, path::Path};

use anyhow::{Context, Result, ensure};

use crate::media::{self, Frame};

/// Read only the requested whole playback seconds. Keep the keyframe before
/// each target: an accurately input-trimmed seek could discard a picture that
/// is still on screen during a VFR gap. FFmpeg names each JPEG by its output
/// PTS, so missing video at the beginning or end never shifts another label.
pub(crate) fn sample(video: &Path, mut seconds: Vec<u64>) -> Result<BTreeMap<u64, Frame>> {
    seconds.sort_unstable();
    seconds.dedup();
    let tmp = tempfile::tempdir()?;
    let mut frames = BTreeMap::new();
    for group in seconds.chunk_by(|a, b| b - a <= 8) {
        let first = group[0];
        let end = group[group.len() - 1] + 1;
        // trim on the integer playback grid already selects a contiguous
        // interval. One eq() per second can exceed FFmpeg's expression depth.
        let select = if end - first == group.len() as u64 {
            String::new()
        } else {
            let tests = group
                .iter()
                .map(|s| format!("eq(t,{s})"))
                .collect::<Vec<_>>()
                .join("+");
            format!(",select='{tests}'")
        };
        let filter = format!(
            "{},trim=start={first}:end={end}{select},{}",
            media::PLAYBACK_FPS,
            media::FRAME_SCALE
        );
        let output = media::playback_input(video, Some(first))
            .args([
                "-map",
                "0:V:0",
                "-vf",
                &filter,
                "-fps_mode",
                "passthrough",
                "-enc_time_base",
                "1:1",
                "-c:v",
                "mjpeg",
                "-q:v",
                "3",
                "-pix_fmt",
                "yuvj420p",
                "-frame_pts",
                "1",
            ])
            .arg(tmp.path().join("%d.jpg"))
            .output()
            .context("字幕の時刻に対応する映像を開けません")?;
        ensure!(
            output.status.success(),
            "ffmpeg: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        for &second in group {
            let path = tmp.path().join(format!("{second}.jpg"));
            if path.try_exists()? {
                frames.insert(
                    second,
                    Frame {
                        second,
                        jpeg: fs::read(path)?,
                    },
                );
            }
        }
    }
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn a_long_contiguous_seek_keeps_each_playback_second() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let video = dir.path().join("long.mp4");
        let status = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=32x32:r=1:d=192",
                "-vf",
                "drawbox=color=blue:t=fill:enable='gte(t,140)'",
                "-c:v",
                "mpeg4",
                "-g",
                "8",
            ])
            .arg(&video)
            .status()?;
        ensure!(status.success(), "long interval fixture");
        let seconds: Vec<_> = (8..=188).collect();
        let frames = sample(&video, seconds.clone())?;
        assert_eq!(frames.keys().copied().collect::<Vec<_>>(), seconds);
        assert!(pixel(&frames[&139])?[0] > 230);
        assert!(pixel(&frames[&140])?[2] > 230);
        Ok(())
    }

    #[test]
    fn seeking_keeps_the_held_vfr_picture_and_the_container_playback_origin() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let video = dir.path().join("offset-vfr.mp4");
        // Audio starts at PTS 7, video at 8.25. The red picture is held until
        // PTS 10.5, i.e. it must still be visible when seeking to playback 3.
        let status = Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-f", "lavfi", "-i", "color=c=red:s=32x32:r=4:d=1",
                   "-f", "lavfi", "-i", "color=c=blue:s=32x32:r=4:d=1",
                   "-f", "lavfi", "-i", "anullsrc=d=8", "-filter_complex",
                   "[0:v][1:v]concat=n=2:v=1:a=0,select='eq(n,0)+eq(n,4)+eq(n,5)',setpts='(1.25+if(eq(N,0),0,if(eq(N,1),2.25,3.25)))/TB'[picture]",
                   "-map", "[picture]", "-map", "2:a", "-c:a", "aac", "-c:v", "mpeg4", "-g", "1",
                   "-enc_time_base", "1:4", "-fps_mode", "vfr", "-output_ts_offset", "7"])
            .arg(&video).status()?;
        ensure!(status.success(), "offset/VFR fixture");
        let frames = sample(&video, vec![1, 2, 3, 4, 5, 30])?;
        assert_eq!(frames.keys().copied().collect::<Vec<_>>(), [2, 3, 4]);
        assert_eq!(frames[&2].jpeg, frames[&3].jpeg);
        let mut full = Vec::new();
        media::scan(&video, 1, |frame| {
            assert_eq!(pixel(&frame)?, pixel(&frames[&frame.second])?);
            full.push(frame.second);
            Ok(())
        })?;
        assert_eq!(full, [2, 3, 4]);
        let sought = sample(&video, vec![3, 4])?;
        assert!(pixel(&sought[&3])?[0] > 210);
        assert!(pixel(&sought[&4])?[2] > 210);
        Ok(())
    }

    fn pixel(frame: &Frame) -> Result<[u8; 3]> {
        Ok(image::load_from_memory(&frame.jpeg)?
            .into_rgb8()
            .get_pixel(16, 16)
            .0)
    }

    #[test]
    fn delayed_half_second_video_does_not_backfill_zero_or_show_a_future_picture() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let video = dir.path().join("half-second.mp4");
        // Red at 0.5, blue at 1.25 and green at 2.5 (each is held until the next PTS).
        let status = Command::new("ffmpeg").args([
            "-nostdin","-v","error","-f","lavfi","-i","color=c=red:s=32x32:r=4:d=1",
            "-f","lavfi","-i","color=c=blue:s=32x32:r=4:d=1","-f","lavfi","-i","color=c=lime:s=32x32:r=4:d=1",
            "-f","lavfi","-i","anullsrc=d=5","-filter_complex",
            "[0:v][1:v][2:v]concat=n=3:v=1:a=0,select='eq(n,0)+eq(n,4)+eq(n,8)+eq(n,9)',setpts='if(eq(N,0),0.5,if(eq(N,1),1.25,if(eq(N,2),2.5,3.5)))/TB'[v]",
            "-map","[v]","-map","3:a","-c:a","aac","-c:v","mpeg4","-g","1","-enc_time_base","1:4","-fps_mode","vfr",
        ]).arg(&video).status()?;
        ensure!(status.success(), "fractional start fixture");
        let isolated = sample(&video, vec![0, 1, 2, 3, 4])?;
        assert_eq!(isolated.keys().copied().collect::<Vec<_>>(), [1, 2, 3]);
        for (step, expected) in [
            (1, vec![(1, 0), (2, 2), (3, 1)]),
            (2, vec![(1, 0), (3, 1)]),
            (60, vec![(1, 0)]),
        ] {
            let mut pictures = Vec::new();
            media::scan(&video, step, |frame| {
                let rgb = pixel(&frame)?;
                assert_eq!(rgb, pixel(&isolated[&frame.second])?);
                let channel = rgb.iter().position(|&v| v > 210).expect("a primary color");
                pictures.push((frame.second, channel));
                Ok(())
            })?;
            assert_eq!(pictures, expected);
        }
        Ok(())
    }

    #[test]
    fn a_track_entirely_between_whole_seconds_yields_no_evidence() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let video = dir.path().join("between-seconds.mp4");
        let status = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-itsoffset",
                "0.5",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=32x32:r=4:d=0.25",
                "-f",
                "lavfi",
                "-i",
                "anullsrc=d=2",
                "-map",
                "0:v",
                "-map",
                "1:a",
                "-c:v",
                "mpeg4",
                "-c:a",
                "aac",
                "-fps_mode",
                "vfr",
            ])
            .arg(&video)
            .status()?;
        ensure!(status.success(), "subsecond fixture");
        let error = media::scan(&video, 1, |_| {
            panic!("no frame exists on the sampling grid")
        })
        .unwrap_err();
        assert!(error.to_string().contains("整数の再生秒"), "{error}");
        assert!(sample(&video, vec![0, 1, 2])?.is_empty());
        Ok(())
    }
}
