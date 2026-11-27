//! Exercise the public CLI, actual video timestamps and local model wire format.
use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
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
            } else if prompt.contains("『地名の検証』") || prompt.contains("『早い地名の検証』")
            {
                Some((
                    2,
                    if prompt.contains("『早い地名の検証』") {
                        3
                    } else {
                        6
                    },
                    "宝石宮殿の紹介",
                    "place",
                    "画面に地名「宝石宮殿」が表示された",
                ))
            } else {
                None
            };
            let answer = if prompt.contains("原寸の1コマ") {
                ensure!(!prompt.contains("炎角の鳥") && !prompt.contains("古い実"));
                let url = body["messages"][1]["content"][1]["image_url"]["url"]
                    .as_str()
                    .unwrap();
                let bytes =
                    STANDARD.decode(url.strip_prefix("data:image/jpeg;base64,").unwrap())?;
                let picture = image::load_from_memory(&bytes)?.to_rgb8();
                ensure!(
                    picture.dimensions() == (96, 54),
                    "expected the original evidence image"
                );
                let [r, g, b] = picture.get_pixel(48, 27).0;
                let properties = &body["response_format"]["json_schema"]["schema"]["properties"];
                if properties.get("action").is_some() {
                    ensure!(
                        properties.get("subject").is_none(),
                        "the combat stage has a separate reading"
                    );
                    json!({"action": if g > 140 && b > 140 { "guard" } else { "ongoing" }, "actor":"player"})
                } else if r > 170 && g > 170 && b < 60 {
                    json!({"subject":"result","literal":""})
                } else if g > 90 && r < 60 && b < 60 {
                    json!({"subject":"item","literal":"風見の実"})
                } else if b > 160 && g < 50 {
                    json!({"subject":"equipment","literal":""})
                } else if g > 140 && b > 140 {
                    json!({"subject":"boss_hud","literal":"蒼の騎士"})
                } else {
                    json!({"subject":"other","literal":""})
                }
            } else if prompt.contains("『敵役の検証』") && prompt.contains("対象ID") {
                json!({"remove":[]})
            } else if prompt.contains("『敵役の検証』") && prompt.contains("新規対象") {
                json!({"events":[]})
            } else if let Some((start, seen, heading, kind, basis)) = regression {
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
            if prompt.contains("対象ID") {
                ensure!(body["messages"][1]["content"].as_array().unwrap().len() == 1);
            } else {
                ensure!(
                    body["messages"][1]["content"][1]["image_url"]["url"]
                        .as_str()
                        .unwrap()
                        .starts_with("data:image/jpeg;base64,")
                );
            }
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
elif os.environ["TEST_SCENE"] in ("地名の検証", "早い地名の検証"):
    if rgb[0] > 185 and rgb[1] > 185: print("旧市街\nマップ")
    elif 140 < rgb[0] < 180 and 140 < rgb[1] < 180: print("スキップ")
    elif (35 < rgb[0] < 65 and 35 < rgb[1] < 65) or (rgb[2] > 150 and rgb[0] < 80):
        print("宝石宮殿")
"#,
    )?;
    fs::set_permissions(&ocr, fs::Permissions::from_mode(0o755))?;
    let (base, server) = model()?;
    let palace_scene = "drawbox=color=0xc8c8c8:t=fill,drawbox=color=0x646464:t=fill:enable='eq(t,2)',drawbox=color=0x303030:t=fill:enable='eq(t,3)',drawbox=color=0xa0a0a0:t=fill:enable='between(t,4,5)',drawbox=color=blue:t=fill:enable='gte(t,6)'";
    let palace_headings = "0:00 動画開始\n0:01 旧市街の紹介\n0:02 宝石宮殿の紹介\n";
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
        ("地名の検証", palace_scene, palace_headings),
        ("早い地名の検証", palace_scene, palace_headings),
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
        if game.ends_with("地名の検証") {
            let seen = if game == "早い地名の検証" {
                3
            } else {
                6
            };
            let data = fs::read_to_string(out.join("evidence.jsonl"))?;
            let old_town: Value = serde_json::from_str(data.lines().nth(1).unwrap())?;
            assert_eq!(old_town["start"], 1);
            assert_eq!(old_town["seen"], 1);
            assert_eq!(old_town["evidence_image"], "frames/00000001.jpg");
            let place: Value = serde_json::from_str(data.lines().nth(2).unwrap())?;
            assert_eq!(place["start_image"], "frames/00000002.jpg");
            assert_eq!(place["seen"], seen);
            assert_eq!(place["evidence_image"], format!("frames/{seen:08}.jpg"));
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

#[test]
fn cached_enemy_guesses_are_checked_at_their_original_frame_before_publication() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let video = tmp.path().join("scene.mp4");
    let filter = "drawbox=color=green:t=fill:enable='between(t,2,3)',drawbox=color=blue:t=fill:enable='eq(t,4)',drawbox=color=cyan:t=fill:enable='gte(t,5)',drawbox=x=4:y=4:w=10:h=10:color=white:t=fill:enable='eq(t,2)',drawbox=color=yellow:t=fill:enable='eq(t,8)'";
    ensure!(
        Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=96x54:r=1:d=10",
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
    let (base, server) = model()?;
    let cache = tmp.path().join("cache");
    let run = |name: &str| -> Result<()> {
        let output = Command::new(env!("CARGO_BIN_EXE_gameplay-timestamps"))
            .arg("敵役の検証")
            .arg(&video)
            .args([
                "--api-base",
                &base,
                "--model",
                "fixture",
                "--search",
                "off",
                "--no-ocr",
                "--no-audio",
                "--inference-cache",
            ])
            .arg(&cache)
            .arg("-o")
            .arg(tmp.path().join(name))
            .output()?;
        ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    };
    run("seed")?;
    let obs = fs::read_dir(&cache)?
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .find(|p| p.file_name().to_string_lossy().starts_with("observations-"))
        .unwrap()
        .path();
    // Model the observations persisted before the naming check existed.
    ensure!(
        Command::new("ffmpeg")
            .args(["-nostdin", "-y", "-v", "error", "-i"])
            .arg(&video)
            .args(["-start_number", "0", "-q:v", "3"])
            .arg(obs.join("frames/%08d.jpg"))
            .status()?
            .success()
    );
    let event = |start, seen, heading: &str, basis: &str, ocr: &str| {
        json!({
            "start":start,"seen":seen,"heading":heading,"basis":basis,"ocr":ocr,
            "start_image":format!("frames/{start:08}.jpg"),"evidence_image":format!("frames/{seen:08}.jpg")
        })
    };
    let mut verified = event(
        7,
        7,
        "銀の怪鳥戦開始",
        "2フレームの敵名と体力ゲージ",
        "7 銀の怪鳥",
    );
    verified["screen_label"] = json!("銀の怪鳥");
    let original = json!([
        event(0, 0, "動画開始", "録画の先頭", ""),
        event(0, 4, "ボス「炎角の鳥」登場", "翼を広げたような赤い衣装", ""),
        event(0, 4, "炎角の鳥を撃破", "大きな装飾が消えた", ""),
        event(1, 3, "「古い実」との戦闘開始", "大きな影が映る", ""),
        event(
            1,
            3,
            "依頼「古塔」進行中にボス「古い実」が出現",
            "大きな影が映る",
            ""
        ),
        event(
            1,
            3,
            "探索中の敵遭遇",
            "敵の名前「古い実」が出て戦い始める",
            ""
        ),
        event(
            3,
            3,
            "風見の実を入手",
            "画面にアイテム取得表示",
            "3 風見の実×3"
        ),
        event(3, 3, "石碑の発見", "石碑に文字が刻まれている", ""),
        event(
            5,
            6,
            "ボス「蒼の騎士」の攻撃をガード成功",
            "敵の名前と攻撃・防御が見える",
            "6 敵名：蒼の騎士"
        ),
        // Persisted events can be ordered by introduction, even when a
        // rematch accidentally points to the guard in the previous battle.
        event(
            5,
            9,
            "ボス「蒼の騎士」の攻撃をガード成功",
            "再戦の攻防",
            "9 敵名：蒼の騎士"
        ),
        event(8, 8, "敵との戦闘に敗北", "結果画面の表示", "8 DEFEAT"),
        verified,
        event(7, 7, "白角のうさぎを倒す", "画面上の目的欄", ""),
        event(
            7,
            7,
            "依頼「探索中に敵と遭遇」を受注",
            "新しい依頼が表示",
            ""
        ),
        event(
            7,
            7,
            "衛兵と敵との戦闘について話す",
            "会話欄に話者と台詞を表示",
            ""
        ),
    ]);
    let bytes = serde_json::to_vec(&original)?;
    fs::write(obs.join("events.json"), &bytes)?;
    for dest in ["corrected", "repeat"] {
        run(dest)?;
        let path = tmp.path().join(dest);
        assert_eq!(
            fs::read_to_string(path.join("timestamps.txt"))?,
            "0:00 動画開始\n0:03 風見の実を入手\n0:03 石碑の発見\n0:04 装備のクローズアップ\n0:05 ボス「蒼の騎士」の攻撃をガード成功\n0:07 銀の怪鳥戦開始\n0:07 白角のうさぎを倒す\n0:07 依頼「探索中に敵と遭遇」を受注\n0:07 衛兵と敵との戦闘について話す\n0:08 戦闘に敗北\n0:09 ボス「蒼の騎士」の攻撃をガード成功\n"
        );
        let raw = fs::read_to_string(path.join("evidence.jsonl"))?;
        assert!(!raw.contains("炎角の鳥") && !raw.contains("古い実"));
        let rows = raw
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<serde_json::Result<Vec<_>>>()?;
        assert_eq!(rows[3]["start_image"], rows[3]["evidence_image"]);
        assert_eq!(rows[3]["seen"], 4);
        assert_eq!(rows[4]["start_image"], "frames/00000005.jpg");
        assert_eq!(rows[4]["evidence_image"], "frames/00000006.jpg");
        assert_eq!(rows.last().unwrap()["start_image"], "frames/00000009.jpg");
        assert_eq!(fs::read(obs.join("events.json"))?, bytes);
    }
    let mut stop =
        std::net::TcpStream::connect(base.trim_start_matches("http://").trim_end_matches("/v1"))?;
    stop.write_all(b"GET /stop HTTP/1.1\r\n\r\n")?;
    // The guard is the only event needing a separate combat-stage reading.
    // The cached proof and onset may be identical after MPEG compression.
    assert!((7..=11).contains(&server.join().unwrap()?));
    Ok(())
}
