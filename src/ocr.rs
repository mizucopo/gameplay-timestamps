use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;

use anyhow::{Context, Result, ensure};

/// Read screen text from one JPEG. A custom executable receives only the image
/// on stdin and returns UTF-8 lines on stdout.
pub fn read(jpeg: &[u8], command: Option<&Path>) -> Result<String> {
    let program = command.unwrap_or_else(|| Path::new("tesseract"));
    let mut process = Command::new(program);
    if command.is_none() {
        process.args(["stdin", "stdout", "-l", "jpn+eng", "--psm", "11", "tsv"]);
    }
    let mut child = process
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("OCR {} を起動できません", program.display()))?;
    let mut stdin = child.stdin.take().expect("piped OCR stdin");
    let (output, write) = thread::scope(|scope| {
        let write = scope.spawn(move || stdin.write_all(jpeg));
        (
            child.wait_with_output(),
            write.join().expect("OCR input writer"),
        )
    });
    let output = output.context("OCR の応答を取得できません")?;
    let errors = String::from_utf8_lossy(&output.stderr);
    ensure!(
        output.status.success()
            && (command.is_some() || !errors.contains("Failed loading language")),
        "OCR {} ({}) {}",
        program.display(),
        output.status,
        errors.trim().chars().take(400).collect::<String>()
    );
    write.context("OCR に画像を渡せません")?;
    let text = std::str::from_utf8(&output.stdout).context("OCR の出力が UTF-8 ではありません")?;
    if command.is_some() {
        return Ok(text.trim().to_owned());
    }
    tesseract_lines(text)
}

fn tesseract_lines(tsv: &str) -> Result<String> {
    ensure!(
        tsv.starts_with("level\tpage_num\t"),
        "Tesseract の TSV を読み取れません"
    );
    let mut lines = Vec::new();
    let mut words = Vec::new();
    let mut confidence_sum = 0.0;
    let mut letters = 0;
    // Japanese names are often split across several word records, down to one
    // kanji per record. Estimate confidence for the whole line to keep its spelling.
    for row in tsv.lines().skip(1).chain(["4\t"]) {
        let fields: Vec<_> = row.splitn(12, '\t').collect();
        if fields[0] == "4" {
            if letters > 0 && confidence_sum >= 50.0 * letters as f32 {
                lines.push(words.join(" "));
            }
            words.clear();
            confidence_sum = 0.0;
            letters = 0;
        } else if fields[0] == "5" && fields.len() == 12 {
            let word = fields[11].trim();
            let confidence: f32 = fields[10].parse().context("OCR の信頼度を読み取れません")?;
            let count = word.chars().filter(|c| c.is_alphanumeric()).count();
            letters += count;
            confidence_sum += confidence * count as f32;
            words.push(word);
        }
    }
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn custom_command_reads_stdin_and_reports_failure() {
        let text = "東の庭園\n第12章";
        assert_eq!(read(text.as_bytes(), Some(Path::new("cat"))).unwrap(), text);
        assert!(read(b"data", Some(Path::new("false"))).is_err());
    }

    #[test]
    fn tesseract_keeps_short_names_and_digits_in_supported_lines() {
        let tsv = "level\tpage_num\tblock_num\tpar_num\tline_num\tword_num\tleft\ttop\twidth\theight\tconf\ttext\n\
                   4\t1\t1\t1\t1\t0\t0\t0\t10\t10\t-1\t\n\
                   5\t1\t1\t1\t1\t1\t0\t0\t5\t5\t94\t灯\n\
                   5\t1\t1\t1\t1\t2\t0\t0\t5\t5\t28\t塔\n\
                   4\t1\t1\t1\t2\t0\t0\t0\t10\t10\t-1\t\n\
                   5\t1\t1\t1\t2\t1\t0\t0\t5\t5\t88\t2\n\
                   4\t1\t1\t1\t3\t0\t0\t0\t10\t10\t-1\t\n\
                   5\t1\t1\t1\t3\t1\t0\t0\t5\t5\t95\t@#\n\
                   4\t1\t1\t1\t4\t0\t0\t0\t10\t10\t-1\t\n\
                   5\t1\t1\t1\t4\t1\t0\t0\t5\t5\t3\tnoise";
        assert_eq!(tesseract_lines(tsv).unwrap(), "灯 塔\n2");
    }
}
