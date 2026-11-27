use std::io::Cursor;

use anyhow::{Context, Result, ensure};
use image::{Rgb, RgbImage, codecs::jpeg::JpegEncoder, imageops};

use crate::media::Frame;

// Each panel has a playback-second label even when the model sees only a single tiled image.
const DIGITS: [[u8; 5]; 10] = [
    [7, 5, 5, 5, 7],
    [2, 6, 2, 2, 7],
    [7, 1, 7, 4, 7],
    [7, 1, 7, 1, 7],
    [5, 5, 7, 1, 1],
    [7, 4, 7, 1, 7],
    [7, 4, 7, 5, 7],
    [7, 1, 1, 1, 1],
    [7, 5, 7, 5, 7],
    [7, 5, 7, 1, 7],
];

pub fn render(frames: &[&Frame], columns: usize, width: u32) -> Result<Vec<u8>> {
    ensure!(!frames.is_empty(), "画像がありません");
    let columns = columns.min(frames.len());
    let height = width * 9 / 16;
    let rows = frames.len().div_ceil(columns);
    let mut sheet = RgbImage::new(columns as u32 * width, rows as u32 * (height + 20));
    for (index, frame) in frames.iter().enumerate() {
        let x = (index % columns) as u32 * width;
        let y = (index / columns) as u32 * (height + 20);
        let panel = image::load_from_memory_with_format(&frame.jpeg, image::ImageFormat::Jpeg)
            .context("フレーム JPEG のデコード")?
            .thumbnail(width, height)
            .into_rgb8();
        imageops::replace(
            &mut sheet,
            &panel,
            (x + (width - panel.width()) / 2).into(),
            (y + 20).into(),
        );
        for (pos, digit) in frame.second.to_string().bytes().enumerate() {
            for (row, bits) in DIGITS[(digit - b'0') as usize].iter().enumerate() {
                for col in 0..3 {
                    if bits & (4 >> col) != 0 {
                        for dy in 0..3 {
                            for dx in 0..3 {
                                sheet.put_pixel(
                                    x + 4 + pos as u32 * 12 + col * 3 + dx,
                                    y + 2 + row as u32 * 3 + dy,
                                    Rgb([255, 255, 255]),
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    let mut bytes = Cursor::new(Vec::new());
    JpegEncoder::new_with_quality(&mut bytes, 86).encode_image(&sheet)?;
    Ok(bytes.into_inner())
}
