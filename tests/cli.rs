//! Exercise the public CLI, actual video timestamps and local model wire format.
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    process::Command,
    thread,
};

fn model() -> Result<(String, thread::JoinHandle<Result<usize>>)> {
    let server = TcpListener::bind("127.0.0.1:0")?;
    let base = format!("http://{}/v1", server.local_addr()?);
    let thread = thread::spawn(move || {
        let mut count = 0;
        loop {
            let (mut conn, _) = server.accept()?;
            let mut reader = BufReader::new(conn.try_clone()?);
            let mut line = String::new();
            reader.read_line(&mut line)?;
            let stop = line.starts_with("GET /stop ");
            let mut length = 0;
            loop {
                line.clear();
                reader.read_line(&mut line)?;
                if line.trim().is_empty() {
                    break;
                }
                if let Some((key, val)) = line.split_once(':')
                    && key.eq_ignore_ascii_case("content-length")
                {
                    length = val.trim().parse()?;
                }
            }
            if stop {
                break;
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body)?;
            let body: Value = serde_json::from_slice(&body)?;
            let prompt = body["messages"][1]["content"][0]["text"].as_str().unwrap();
            let candidate = |s, h: &str, update: Option<usize>| json!({"start":s,"seen":s,"heading":h,"update":update});
            let regression = if prompt.contains("『先頭の検証』") {
                Some((0, 5, "依頼「灯台の配達」", "quest", "画面上の依頼を確認"))
            } else if prompt.contains("『再開の検証』") {
                Some((6, 8, "橋の修理の依頼を再開", "quest", "画面上の依頼を確認"))
            } else if prompt.contains("『地名の検証』") {
                Some((
                    2,
                    6,
                    "宝石宮殿の紹介",
                    "place",
                    "画面に地名「宝石宮殿」が表示された",
                ))
            } else {
                None
            };
            let answer = if let Some((start, seen, heading, kind, basis)) = regression {
                if prompt.contains("新規対象") {
                    let mut events = Vec::new();
                    if kind == "place" {
                        events.push(
                            json!({"start":1,"seen":1,"heading":"旧市街の紹介","kind":"place"}),
                        );
                    }
                    events.push(json!({"start":start,"seen":seen,"heading":heading,"kind":kind}));
                    json!({"events":events})
                } else if prompt.contains("スキップ可能な演出に切り替わりました")
                {
                    json!({"kind":"location","picture":"建物の全景が見える"})
                } else if prompt.contains("個々の台詞") {
                    let events = if kind == "place" {
                        vec![
                            json!({"id":0,"start":1,"heading":"旧市街の紹介","basis":"旧市街の地名が表示された"}),
                            json!({"id":1,"start":start,"heading":heading,"basis":basis}),
                        ]
                    } else {
                        vec![json!({"id":0,"start":start,"heading":heading,"basis":basis})]
                    };
                    json!({"events":events})
                } else {
                    anyhow::bail!("unexpected request: {prompt}");
                }
            } else if prompt.contains("新規対象 0〜299") {
                json!({"events":[json!({"start":9,"seen":13,"heading":"再開"}), candidate(13,"港の紹介",Some(1)),candidate(196,"依頼の導入",None)]})
            } else if prompt.contains("新規対象 300〜") {
                json!({"events":[candidate(308,"依頼名の表示",Some(3))]})
            } else if prompt.contains("個々の台詞") {
                let inputs: Vec<Value> = serde_json::from_str(prompt.lines().last().unwrap())?;
                let rows=inputs.iter().map(|input| {
                    let seen=input["seen"].as_u64().unwrap();
                    let (start,heading)=match seen {9=>(9,"プレイ再開"),13 if input["id"] == 0 => (9,"プレイ再開"),
                         13=>(13,"港の紹介"),196=>(196,"依頼の導入"),
                        308=>(308,"時計職人の依頼"), _=>panic!("unexpected proof")};
                    json!({"id":input["id"],"start":start,"heading":heading,"basis":"画面の文字を前後で照合"})
                }).collect::<Vec<_>>();
                json!({"events":rows})
            } else {
                anyhow::bail!("unexpected request: {prompt}");
            };
            ensure!(
                body["messages"][1]["content"][1]["image_url"]["url"]
                    .as_str()
                    .unwrap()
                    .starts_with("data:image/jpeg;base64,")
            );
            let bytes = serde_json::to_vec(
                &json!({"choices":[{"message":{"content":answer.to_string()}}]}),
            )?;
            write!(
                conn,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len()
            )?;
            conn.write_all(&bytes)?;
            count += 1;
        }
        Ok(count)
    });
    Ok((base, thread))
}

#[cfg(unix)]
#[test]
fn scene_changes_preserve_the_opening_nearby_quests_and_later_evidence() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir()?;
    let ocr = tmp.path().join("read-color-card");
    fs::write(
        &ocr,
        r#"#!/usr/bin/env python3
import os, subprocess, sys
rgb = subprocess.check_output(
    ["ffmpeg", "-v", "error", "-i", "pipe:0", "-vf", "scale=1:1", "-frames:v", "1",
     "-pix_fmt", "rgb24", "-f", "rawvideo", "pipe:1"], input=sys.stdin.buffer.read())
if os.environ["TEST_SCENE"] == "先頭の検証" and rgb[2] > 120:
    print("依頼名：灯台の配達\n依頼人：館長\n報酬：金貨\n達成条件：古地図を届ける")
elif os.environ["TEST_SCENE"] == "再開の検証":
    if rgb[2] > 120: print("ロード スロット")
    if rgb[1] > 85: print("橋の修理の依頼を再開")
elif os.environ["TEST_SCENE"] == "地名の検証":
    if rgb[0] > 185 and rgb[1] > 185: print("旧市街\nマップ")
    elif 140 < rgb[0] < 180 and 140 < rgb[1] < 180: print("スキップ")
    elif rgb[2] > 150 and rgb[0] < 80: print("宝石宮殿")
"#,
    )?;
    fs::set_permissions(&ocr, fs::Permissions::from_mode(0o755))?;
    let (base, server) = model()?;
    for (game, filter, expected) in [
        (
            "先頭の検証",
            "drawbox=color=blue:t=fill:enable='gte(t,4)'",
            "0:00 動画開始\n0:04 依頼「灯台の配達」\n",
        ),
        (
            "再開の検証",
            "drawbox=color=blue:t=fill:enable='lte(t,2)',drawbox=color=black:t=fill:enable='between(t,3,4)',drawbox=color=green:t=fill:enable='gte(t,5)'",
            "0:00 動画開始\n0:05 プレイ再開\n0:06 橋の修理の依頼を再開\n",
        ),
        (
            "地名の検証",
            "drawbox=color=0xc8c8c8:t=fill,drawbox=color=0x646464:t=fill:enable='eq(t,2)',drawbox=color=black:t=fill:enable='eq(t,3)',drawbox=color=0xa0a0a0:t=fill:enable='between(t,4,5)',drawbox=color=blue:t=fill:enable='gte(t,6)'",
            "0:00 動画開始\n0:01 旧市街の紹介\n0:02 宝石宮殿の紹介\n",
        ),
    ] {
        let video = tmp.path().join(format!("{game}.mp4"));
        ensure!(
            Command::new("ffmpeg")
                .args([
                    "-nostdin",
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=red:s=64x48:r=1:d=13",
                    "-vf",
                    filter,
                    "-an",
                    "-c:v",
                    "mpeg4"
                ])
                .arg(&video)
                .status()?
                .success()
        );
        let out = tmp.path().join(game);
        let result = Command::new(env!("CARGO_BIN_EXE_gameplay-timestamps"))
            .arg(game)
            .arg(&video)
            .args([
                "--api-base",
                &base,
                "--model",
                "fixture",
                "--no-audio",
                "--search",
                "off",
                "--ocr-command",
            ])
            .arg(&ocr)
            .arg("-o")
            .arg(&out)
            .env("TEST_SCENE", game)
            .output()?;
        ensure!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(fs::read_to_string(out.join("timestamps.txt"))?, expected);
        if game == "地名の検証" {
            let data = fs::read_to_string(out.join("evidence.jsonl"))?;
            let old_town: Value = serde_json::from_str(data.lines().nth(1).unwrap())?;
            assert_eq!(old_town["start"], 1);
            assert_eq!(old_town["seen"], 1);
            assert_eq!(old_town["evidence_image"], "frames/00000001.jpg");
            let place: Value = serde_json::from_str(data.lines().nth(2).unwrap())?;
            assert_eq!(place["start_image"], "frames/00000002.jpg");
            assert_eq!(place["seen"], 6);
            assert_eq!(place["evidence_image"], "frames/00000006.jpg");
            assert!(place["ocr"].as_str().unwrap().contains("宝石宮殿"));
        }
    }
    let mut stop =
        std::net::TcpStream::connect(base.trim_start_matches("http://").trim_end_matches("/v1"))?;
    stop.write_all(b"GET /stop HTTP/1.1\r\n\r\n")?;
    server.join().unwrap()?;
    Ok(())
}

#[test]
fn silent_recording_keeps_distinct_starts_and_links_later_evidence() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let video = tmp.path().join("録画.mp4");
    let mut ffmpeg = Command::new("ffmpeg");
    ffmpeg
        .args([
            "-nostdin",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=blue:s=64x48:r=1:d=312",
            "-an",
            "-c:v",
            "mpeg4",
        ])
        .arg(&video);
    ensure!(ffmpeg.status()?.success());
    let (base, server) = model()?;
    for name in ["first", "second"] {
        let out = tmp.path().join(name);
        let result = Command::new(env!("CARGO_BIN_EXE_gameplay-timestamps"))
            .arg("サンプル")
            .arg(&video)
            .args([
                "--api-base",
                &base,
                "--model",
                "test",
                "--search",
                "off",
                "--no-ocr",
                "--no-audio",
            ])
            .arg("--inference-cache")
            .arg(tmp.path().join("cache"))
            .arg("-o")
            .arg(&out)
            .output()?;
        ensure!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            fs::read_to_string(out.join("timestamps.txt"))?,
            "0:00 動画開始\n0:09 プレイ再開\n0:13 港の紹介\n3:16 時計職人の依頼\n"
        );
        let rows = fs::read_to_string(out.join("evidence.jsonl"))?
            .lines()
            .map(serde_json::from_str)
            .collect::<serde_json::Result<Vec<Value>>>()?;
        assert_eq!(rows[3]["start"], 196);
        assert_eq!(rows[3]["seen"], 308);
        for row in rows {
            for key in ["start_image", "evidence_image"] {
                assert!(out.join(row[key].as_str().unwrap()).is_file());
            }
        }
    }
    let mut stop =
        std::net::TcpStream::connect(base.trim_start_matches("http://").trim_end_matches("/v1"))?;
    stop.write_all(b"GET /stop HTTP/1.1\r\n\r\n")?;
    assert_eq!(server.join().unwrap()?, 4);
    Ok(())
}
