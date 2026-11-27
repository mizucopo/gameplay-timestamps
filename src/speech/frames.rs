use std::{collections::BTreeMap, fs, path::Path, process::Command};

use anyhow::{Context, Result, ensure};

use crate::media::Frame;

/// Read only the requested whole playback seconds. Keep the keyframe before
/// each target: an accurately input-trimmed seek could discard a picture that
/// is still on screen during a VFR gap. FFmpeg names each JPEG by its output
/// PTS, so missing video at the beginning or end never shifts another label.
pub(super) fn sample(video: &Path, mut seconds: Vec<u64>) -> Result<BTreeMap<u64, Frame>> {
    seconds.sort_unstable();
    seconds.dedup();
    let tmp = tempfile::tempdir()?;
    let mut frames = BTreeMap::new();
    for group in seconds.chunk_by(|a, b| b - a <= 8) {
        let first = group[0];
        let end = group[group.len() - 1] + 1;
        let select = group
            .iter()
            .map(|s| format!("eq(t,{s})"))
            .collect::<Vec<_>>()
            .join("+");
        let filter = format!(
            "fps=1:round=up:eof_action=pass,trim=start={first}:end={end},select='{select}',\
             scale=w='min(iw,1920)':h='min(ih,1920)':force_original_aspect_ratio=decrease:force_divisible_by=2"
        );
        let output = Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-xerror",
                "-protocol_whitelist",
                "file,pipe",
                "-copyts",
                "-start_at_zero",
                "-noaccurate_seek",
                "-ss",
                &first.to_string(),
                "-i",
            ])
            .arg(video)
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
        let sought = sample(&video, vec![3, 4])?;
        let pixel = |second| -> Result<_> {
            Ok(image::load_from_memory(&sought[&second].jpeg)?
                .into_rgb8()
                .get_pixel(16, 16)
                .0)
        };
        assert!(pixel(3)?[0] > 210);
        assert!(pixel(4)?[2] > 210);
        Ok(())
    }
}
