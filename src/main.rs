mod analyze;
mod battle;
mod captions;
mod description;
mod dictionary;
mod editor;
mod location;
mod map;
mod media;
mod model;
mod objective;
mod observations;
mod ocr;
mod opponent;
mod quest;
mod scene;
mod sheet;
mod speech;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use clap::Parser;

/// ゲームの録画を全編走査し、YouTube 概要欄用の日本語タイムスタンプを作成します。
#[derive(Parser)]
#[command(version)]
struct Args {
    /// ゲームの正式タイトル
    game: String,
    /// ローカルの動画ファイル
    video: PathBuf,
    /// リメイク・プラットフォームなどの版
    #[arg(long, default_value = "")]
    edition: String,
    /// 出力先（新規ディレクトリ。既定は動画の隣の「ファイル名.timestamps」）
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// 作品別辞書の保存先（既定はユーザーのキャッシュディレクトリ）
    #[arg(long, env = "GAMEPLAY_DICTIONARIES")]
    cache: Option<PathBuf>,
    /// 辞書検索の方式
    #[arg(long, default_value = "auto", value_parser = ["auto","brave","tavily","command","cache","off"])]
    search: String,
    /// 同一 PC の OpenAI 互換 API
    #[arg(
        long,
        env = "GAMEPLAY_VLM_BASE",
        default_value = "http://127.0.0.1:8000/v1"
    )]
    api_base: String,
    /// 視覚モデルの served model name（モデル1個のときは自動）
    #[arg(long, env = "GAMEPLAY_VLM_MODEL")]
    model: Option<String>,
    /// 走査間隔（秒）。短い出来事を残す既定値は 1
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=60))]
    step: u32,
    /// ローカル文字起こし実行ファイル。第1引数に動画を渡し、stdout に SRT/VTT を受け取る
    #[arg(long, env = "GAMEPLAY_TRANSCRIBER")]
    transcriber: Option<PathBuf>,
    /// 発話の利用を止める。利用可能な字幕は残す
    #[arg(long)]
    no_audio: bool,
    /// 日本語OCRの実行ファイル。stdin JPEG, stdout UTF-8（既定は tesseract）
    #[arg(long, env = "GAMEPLAY_OCR_COMMAND", conflicts_with = "no_ocr")]
    ocr_command: Option<PathBuf>,
    /// 同一モデル・同一入力の推論応答を再利用するローカルディレクトリ
    #[arg(long, env = "GAMEPLAY_INFERENCE_CACHE")]
    inference_cache: Option<PathBuf>,
    /// 日本語OCRを停止する
    #[arg(long)]
    no_ocr: bool,
}

fn clock(sec: u64) -> String {
    let (hours, minutes, seconds) = (sec / 3600, (sec / 60) % 60, sec % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn default_cache() -> PathBuf {
    let root = std::env::var_os("XDG_CACHE_HOME")
        .or_else(|| std::env::var_os("LOCALAPPDATA"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| Path::new(&p).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    root.join("gameplay-timestamps").join("dictionaries")
}

fn generate(args: Args) -> Result<PathBuf> {
    ensure!(!args.game.trim().is_empty(), "ゲームタイトルが空です");
    let video = args
        .video
        .canonicalize()
        .context("動画ファイルを開けません")?;
    ensure!(video.is_file(), "動画ファイルを指定してください");
    let output = args
        .output
        .unwrap_or_else(|| video.with_extension("timestamps"));
    ensure!(
        !output.try_exists()?,
        "出力先 {} は既に存在します。別の --output を指定してください",
        output.display()
    );
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let work = tempfile::Builder::new()
        .prefix(".gameplay-timestamps-")
        .tempdir_in(parent)?;
    // Repeated native frames and prompts can occur in several passes of the
    // same recording. Keep those answers for this run even without a shared cache.
    let temporary_responses = args
        .inference_cache
        .is_none()
        .then(tempfile::tempdir)
        .transpose()?;
    let response_cache = args
        .inference_cache
        .as_deref()
        .or_else(|| temporary_responses.as_ref().map(|dir| dir.path()));
    let model = model::Model::connect(&args.api_base, args.model, response_cache)?;
    let dictionary = dictionary::load(
        &args.game,
        &args.edition,
        &args.cache.unwrap_or_else(default_cache),
        &args.search,
    )?;
    let subtitles = media::cues(&video, !args.no_audio, args.transcriber.as_deref());
    let analyzer = analyze::Analyzer {
        game: &args.game,
        edition: &args.edition,
        model: &model,
        dir: work.path(),
        use_ocr: !args.no_ocr,
        ocr_command: args.ocr_command.as_deref(),
    };
    let mut scan = observations::read_or_scan(
        &analyzer,
        &video,
        args.step,
        args.inference_cache.as_deref(),
    )?;
    speech::supplement(&analyzer, &video, &subtitles, &mut scan)?;
    let mut events = scan.events;
    editor::refine(&mut events, &model, &args.game);
    let opponent_claims: Vec<_> = events.iter().map(opponent::needs_check).collect();
    captions::enrich(&mut events, &subtitles, &dictionary, &model, &args.game)?;
    opponent::verify(
        &mut events,
        &subtitles,
        &dictionary,
        &analyzer,
        &video,
        &opponent_claims,
    );
    // Keep the source opening until every later proof has had a chance to move
    // a provisional event away from zero. A meaningful zero-second event can
    // now stand in for the generic opening.
    if events
        .iter()
        .any(|e| e.start == 0 && e.heading != "動画開始")
    {
        events.retain(|e| !(e.start == 0 && e.heading == "動画開始" && e.basis == "録画の先頭"));
    }
    description::write(work.path(), events)?;
    fs::rename(work.path(), &output).context("結果の出力先を確定できません")?;
    Ok(output)
}

fn main() -> Result<()> {
    println!(
        "{}",
        generate(Args::parse())?.join("timestamps.txt").display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn youtube_clock_preserves_close_events_and_long_recordings() {
        assert_eq!(
            [0, 69, 73, 3599, 3600, 37_201].map(clock),
            ["0:00", "1:09", "1:13", "59:59", "1:00:00", "10:20:01"]
        );
    }
}
