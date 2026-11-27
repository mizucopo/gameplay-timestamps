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

// Average change over the image and over its most changed 8 × 6 pixel tile.
fn luma_change(before: &[u8], after: &[u8]) -> (u32, u32) {
    let mut blocks = [0_u32; 12 * 9];
    for (i, (a, b)) in before.iter().zip(after).enumerate() {
        blocks[(i / 96 / 6) * 12 + i % 96 / 8] += u32::from(a.abs_diff(*b));
    }
    let mean = blocks.iter().sum::<u32>() / after.len() as u32;
    let peak = blocks.into_iter().max().unwrap() / 48;
    (mean, peak)
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
        let (change, peak) = before.map_or((0, 0), |prev| luma_change(&prev.luminance, &thumbnail));
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
    // Keep both settled cards and an isolated one-sample overlay. A flash
    // leaves the same background on either side; its useful picture is the
    // middle sample. Requiring a quiet lead-in avoids selecting every beat of
    // a continuously alternating animation.
    const QUIET: u32 = 6;
    let mut returned = false;
    let mut shown = Vec::<&[u8]>::new();
    for (i, pair) in frames.windows(2).enumerate() {
        if i.is_multiple_of(width) {
            shown.clear();
        }
        let current = &signals[&pair[0].second];
        let after = &signals[&pair[1].second];
        let flash = current.local_change && after.local_change && i > 0 && {
            let before = &signals[&frames[i - 1].second];
            before.peak <= QUIET && luma_change(&before.luminance, &after.luminance).1 <= QUIET
        };
        if (flash || (current.local_change && after.peak <= QUIET && !returned))
            && !shown.iter().any(|prior| {
                prior
                    .iter()
                    .zip(&current.luminance)
                    .all(|(a, b)| a.abs_diff(*b) <= 2)
            })
        {
            // An identical blinking indicator only needs one sample per bin.
            // Compare the pixels, not the tile, so different lettering survives.
            chosen.insert(pair[0].second);
            shown.push(&current.luminance);
        }
        returned = flash;
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

    fn selected_seconds(draw: impl Fn(u64, &mut GrayImage)) -> Result<Vec<u64>> {
        let mut frames = Vec::new();
        let mut signals = BTreeMap::new();
        for second in 0..300 {
            let mut picture = GrayImage::from_pixel(192, 108, Luma([64]));
            draw(second, &mut picture);
            let mut jpeg = Vec::new();
            JpegEncoder::new(&mut jpeg).encode_image(&picture)?;
            let frame = Frame { second, jpeg };
            let signal = Signal::measure(&frame, signals.last_key_value().map(|(_, s)| s))?;
            signals.insert(second, signal);
            frames.push(frame);
        }
        let selected = panels(&frames.iter().collect::<Vec<_>>(), &signals);
        Ok(selected.iter().map(|f| f.second).collect())
    }

    fn card(picture: &mut GrayImage, x: u32, glyph: u32) {
        for i in 0..18 {
            for j in 0..9 {
                let shade = if i == glyph && (2..7).contains(&j) {
                    75
                } else {
                    230
                };
                picture.put_pixel(x + i, 74 + j, Luma([shade]));
            }
        }
    }

    #[test]
    fn two_brief_status_cards_between_ocr_samples_reach_the_model() -> Result<()> {
        let seconds = selected_seconds(|second, picture| match second {
            7 | 8 => card(picture, 24, 6),
            11 | 12 => card(picture, 120, 10),
            _ => (),
        })?;
        assert!(seconds.contains(&7) && seconds.contains(&11));
        Ok(())
    }

    #[test]
    fn one_sample_cards_keep_their_own_picture_even_at_the_same_position() -> Result<()> {
        let seconds = selected_seconds(|second, picture| match second {
            7 => card(picture, 24, 6),
            11 => card(picture, 120, 6),
            20 => card(picture, 84, 6),
            24 => card(picture, 84, 10),
            _ => (),
        })?;
        for second in [7, 11, 20, 24] {
            assert!(seconds.contains(&second), "missing {second} in {seconds:?}");
        }
        Ok(())
    }

    #[test]
    fn continuously_alternating_hud_stays_within_one_overview() -> Result<()> {
        let seconds = selected_seconds(|second, picture| {
            card(picture, if second % 2 == 0 { 40 } else { 72 }, 6);
        })?;
        assert!(seconds.len() <= 36, "selected {} / 300", seconds.len());
        Ok(())
    }

    #[test]
    fn a_repeating_single_sample_indicator_has_bounded_coverage() -> Result<()> {
        let seconds = selected_seconds(|second, picture| {
            if second % 3 == 1 {
                card(picture, 64, 8);
            }
        })?;
        assert!(seconds.iter().any(|sec| sec % 3 == 1));
        // At most a regular representative, one distinct flash and the
        // existing continuous-motion representative in each pair of bins.
        assert!(seconds.len() <= 47, "selected {} / 300", seconds.len());
        Ok(())
    }
}
