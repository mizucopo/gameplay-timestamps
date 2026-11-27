use std::{collections::BTreeSet, fs, io, path::Path};

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};

use crate::analyze::{Analyzer, Event};

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
        return a.run(video, step);
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
            _ => return a.run(video, step),
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
    if !dir.exists() {
        let tmp = tempfile::tempdir_in(cache)?;
        let worker = Analyzer {
            dir: tmp.path(),
            ..*a
        };
        let events = worker.run(video, step)?;
        fs::write(tmp.path().join("events.json"), serde_json::to_vec(&events)?)?;
        if let Err(error) = fs::rename(tmp.path(), &dir)
            && !dir.is_dir()
        {
            return Err(error.into());
        }
    }
    let events: Vec<Event> = serde_json::from_reader(fs::File::open(dir.join("events.json"))?)?;
    ensure!(!events.is_empty(), "映像観察キャッシュに記録がありません");
    fs::create_dir(a.dir.join("frames"))?;
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
        let (src, dst) = (dir.join(path), a.dir.join(path));
        fs::copy(src, dst)?;
    }
    Ok(events)
}
