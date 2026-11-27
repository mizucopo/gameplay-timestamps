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
            let candidate = |s, h: &str, update: Option<usize>| json!({"start":s,"seen":s,"heading":h,"basis":h,"update":update});
            let answer = if prompt.contains("新規対象 0〜299") {
                json!({"events":[candidate(9,"再開",None), candidate(13,"港の紹介",None),candidate(196,"依頼の導入",None)]})
            } else if prompt.contains("新規対象 300〜") {
                json!({"events":[candidate(308,"依頼名の表示",Some(3))]})
            } else if prompt.contains("個々の台詞") {
                let rows = [(9,"プレイ再開"),(13,"港の紹介"),(196,"依頼の導入"),(308,"時計職人の依頼")]
                    .into_iter().filter(|(s,_)|prompt.contains(&format!("\"seen\":{s},")))
                    .map(|(s,h)|json!({"seen":s,"start":s,"heading":h,"basis":"画面の文字を前後で照合"})).collect::<Vec<_>>();
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
