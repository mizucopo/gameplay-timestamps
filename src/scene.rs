//! Pick pictures for a model's context window without using only hard cuts.
//! Regular coverage preserves stationary conversations and slow reveals.
use anyhow::Result;
use image::ImageFormat;
use std::collections::{BTreeMap, BTreeSet};

use crate::media::Frame;

pub struct Signal {
    luminance: Vec<u8>,
    pub dark: bool,
    pub mean: u32,
    pub change: u32,
}

impl Signal {
    pub fn measure(frame: &Frame, before: Option<&Self>) -> Result<Self> {
        let thumbnail = image::load_from_memory_with_format(&frame.jpeg, ImageFormat::Jpeg)?
            .thumbnail_exact(48, 27)
            .into_luma8()
            .into_raw();
        let n = thumbnail.len() as u32;
        let dark = thumbnail.iter().filter(|&&v| v >= 22).count() * 100 < thumbnail.len() * 3;
        let mean = thumbnail.iter().map(|&v| u32::from(v)).sum::<u32>() / n;
        let change = before.map_or(0, |prev| {
            prev.luminance
                .iter()
                .zip(&thumbnail)
                .map(|(a, b)| u32::from(a.abs_diff(*b)))
                .sum::<u32>()
                / n
        });
        Ok(Self {
            luminance: thumbnail,
            dark,
            mean,
            change,
        })
    }
}

pub fn panels<'a>(frames: &[&'a Frame], signals: &BTreeMap<u64, Signal>) -> Vec<&'a Frame> {
    let mut chosen = BTreeSet::new();
    let width = frames.len().div_ceil(18).max(1);
    for bin in frames.chunks(width) {
        // The first frame on a tie is preferable around long title cards.
        let representative = bin
            .iter()
            .min_by_key(|f| std::cmp::Reverse(signals[&f.second].change))
            .unwrap();
        chosen.insert(representative.second);
    }
    chosen.insert(frames[0].second);
    chosen.insert(frames.last().unwrap().second);
    // Reserve places for significant close transitions, including brief intertitles.
    let mut cuts: Vec<_> = (1..frames.len())
        .filter(|&i| {
            let cur = &signals[&frames[i].second];
            let prev = &signals[&frames[i - 1].second];
            cur.dark != prev.dark || cur.change >= 32
        })
        .collect();
    cuts.sort_by_key(|&i| std::cmp::Reverse(signals[&frames[i].second].change));
    for i in cuts.into_iter().take(8) {
        chosen.insert(frames[i].second);
    }
    frames
        .iter()
        .copied()
        .filter(|f| chosen.contains(&f.second))
        .collect()
}
