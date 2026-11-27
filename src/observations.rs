use std::{collections::BTreeSet, fs, io, path::Path};

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};

use crate::analyze::{Analyzer, Event, Scan};

fn file_hash(path: &Path) -> Result<String> {
    let mut hash = Sha256::new();
    io::copy(&mut fs::File::open(path)?, &mut hash)?;
    Ok(format!("{:x}", hash.finalize()))
}

pub fn read_or_scan(
    a: &Analyzer<'_>,
    video: &Path,
    step: u32,
    cache: Option<&Path>,
) -> Result<Vec<Event>> {
    let Some(cache) = cache else {
        return Ok(a.run(video, step)?.events);
    };
    let mut hash = Sha256::new();
    for src in [
        include_str!("analyze.rs"),
        include_str!("ocr.rs"),
        include_str!("quest.rs"),
        include_str!("location.rs"),
        include_str!("model.rs"),
        include_str!("sheet.rs"),
        include_str!("scene.rs"),
        include_str!("media.rs"),
    ] {
        hash.update(src);
    }
    // A command name may be resolved through PATH, or OCR may be unavailable. In
    // either case we can scan normally without reusing an unidentified adapter.
    let adapter = match a.ocr_command {
        Some(path) => match file_hash(path) {
            Ok(hash) if path.components().count() > 1 => Some(hash),
            _ => return Ok(a.run(video, step)?.events),
        },
        None => None,
    };
    hash.update(serde_json::to_vec(&(
        file_hash(video)?,
        a.game,
        a.edition,
        step,
        a.model.identity(),
        a.use_ocr,
        adapter,
    ))?);
    let dir = cache.join(format!("observations-{:x}", hash.finalize()));
    if dir.exists() {
        return load(&dir, a.dir);
    }
    let tmp = tempfile::tempdir_in(cache)?;
    let worker = Analyzer {
        dir: tmp.path(),
        ..*a
    };
    let scan = worker.run(video, step)?;
    finish(tmp.path(), &dir, a.dir, scan)
}

fn finish(work: &Path, cache: &Path, output: &Path, scan: Scan) -> Result<Vec<Event>> {
    if scan.complete {
        fs::write(work.join("events.json"), serde_json::to_vec(&scan.events)?)?;
        if let Err(error) = fs::rename(work, cache)
            && !cache.is_dir()
        {
            return Err(error.into());
        }
    } else if !cache.is_dir() {
        // Return this invocation's evidence without publishing a degraded scan.
        return materialize(work, output, scan.events);
    }
    // A concurrent successful scan may have already published its complete evidence.
    load(cache, output)
}

fn load(cache: &Path, output: &Path) -> Result<Vec<Event>> {
    let events: Vec<Event> = serde_json::from_reader(fs::File::open(cache.join("events.json"))?)?;
    ensure!(!events.is_empty(), "映像観察キャッシュに記録がありません");
    materialize(cache, output, events)
}

fn materialize(source: &Path, output: &Path, events: Vec<Event>) -> Result<Vec<Event>> {
    fs::create_dir(output.join("frames"))?;
    let images = events
        .iter()
        .flat_map(|e| e.start_image.iter().chain([&e.evidence_image]))
        .collect::<BTreeSet<_>>();
    for path in images {
        // Only numeric frame names produced by the observation pass may be materialized.
        let name = path
            .strip_prefix("frames/")
            .and_then(|s| s.strip_suffix(".jpg"))
            .context("キャッシュ画像の名前が不正です")?;
        ensure!(
            !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()),
            "キャッシュ画像の番号が不正です"
        );
        let (src, dst) = (source.join(path), output.join(path));
        fs::copy(src, dst)?;
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn incomplete_scan_returns_evidence_but_cannot_replace_a_complete_scan() -> Result<()> {
        let root = tempdir()?;
        let cache = root.path().join("observations");
        let frame = "frames/00000000.jpg";
        let scan = |complete, evidence: &str| -> Result<_> {
            let work = tempfile::tempdir_in(root.path())?;
            let output = tempfile::tempdir_in(root.path())?;
            fs::create_dir(work.path().join("frames"))?;
            fs::write(work.path().join(frame), evidence)?;
            let event = Event {
                start: 0,
                seen: 0,
                heading: evidence.into(),
                basis: evidence.into(),
                start_image: Some(frame.into()),
                evidence_image: frame.into(),
                screen_label: None,
                ocr: String::new(),
            };
            let returned = finish(
                work.path(),
                &cache,
                output.path(),
                Scan {
                    events: vec![event],
                    complete,
                },
            )?;
            assert_eq!(
                fs::read_to_string(output.path().join(frame))?,
                returned[0].heading
            );
            Ok(returned.into_iter().next().unwrap().heading)
        };
        assert_eq!(scan(false, "映像のみ")?, "映像のみ");
        assert!(!cache.exists());
        assert_eq!(scan(true, "完了済み")?, "完了済み");
        assert_eq!(scan(false, "別実行のOCR失敗")?, "完了済み");
        assert_eq!(scan(true, "並行した走査")?, "完了済み");
        assert_eq!(fs::read_to_string(cache.join(frame))?, "完了済み");
        Ok(())
    }
}
