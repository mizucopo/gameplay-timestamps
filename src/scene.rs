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
    pub local_change: bool,
    peak: u32,
}

impl Signal {
    pub fn measure(frame: &Frame, before: Option<&Self>) -> Result<Self> {
        let thumbnail = image::load_from_memory_with_format(&frame.jpeg, ImageFormat::Jpeg)?
            .thumbnail_exact(96, 54)
            .into_luma8()
            .into_raw();
        let n = thumbnail.len() as u32;
        let dark = thumbnail.iter().filter(|&&v| v >= 22).count() * 100 < thumbnail.len() * 3;
        let mean = thumbnail.iter().map(|&v| u32::from(v)).sum::<u32>() / n;
        let (change, peak) = before.map_or((0, 0), |prev| {
            // Small dialogue cards can change one region while the surrounding scene stays fixed.
            let mut blocks = [0_u32; 12 * 9];
            for (i, (a, b)) in prev.luminance.iter().zip(&thumbnail).enumerate() {
                blocks[(i / 96 / 6) * 12 + i % 96 / 8] += u32::from(a.abs_diff(*b));
            }
            let mean = blocks.iter().sum::<u32>() / n;
            let peak = blocks.into_iter().max().unwrap() / 48;
            (mean, peak)
        });
        Ok(Self {
            luminance: thumbnail,
            dark,
            mean,
            change,
            local_change: peak >= 14 && change <= 9 && peak >= 5 * change,
            peak,
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
    // Read every local change with OCR. Give a settled card its own picture,
    // including cards closer together than the regular bins. During continuous
    // animation use one additional representative per interval on the sheet.
    for pair in frames.windows(2) {
        let current = &signals[&pair[0].second];
        let after = &signals[&pair[1].second];
        if current.local_change && after.peak <= 6 {
            chosen.insert(pair[0].second);
        }
    }
    for bin in frames.chunks(width * 2) {
        if let Some(frame) = bin
            .iter()
            .filter(|f| signals[&f.second].local_change)
            .max_by_key(|f| signals[&f.second].peak)
        {
            chosen.insert(frame.second);
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GrayImage, Luma, codecs::jpeg::JpegEncoder};

    #[test]
    fn two_brief_status_cards_between_ocr_samples_reach_the_model() -> Result<()> {
        let mut frames = Vec::new();
        let mut signals = BTreeMap::new();
        for second in 0..300 {
            let mut picture = GrayImage::from_pixel(192, 108, Luma([64]));
            let card_x = match second {
                7 | 8 => Some(24),
                11 | 12 => Some(120),
                _ => None,
            };
            if let Some(x) = card_x {
                for i in x..x + 18 {
                    for j in 74..83 {
                        picture.put_pixel(i, j, Luma([230]));
                    }
                }
            }
            let mut jpeg = Vec::new();
            JpegEncoder::new(&mut jpeg).encode_image(&picture)?;
            let frame = Frame { second, jpeg };
            let signal = Signal::measure(&frame, signals.last_key_value().map(|(_, s)| s))?;
            signals.insert(second, signal);
            frames.push(frame);
        }
        let selected = panels(&frames.iter().collect::<Vec<_>>(), &signals);
        let seconds: Vec<_> = selected.iter().map(|f| f.second).collect();
        assert!(seconds.contains(&7) && seconds.contains(&11));
        Ok(())
    }
}
