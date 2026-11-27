//! Sourced spellings for one game and edition. Search runs before the video is read.
use anyhow::{Context, Result, bail, ensure};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, File};
use std::io::{ErrorKind, Read, Seek, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
struct Term {
    name: String,
    #[serde(default)]
    aliases: Vec<String>,
    /// A short form requires one of these cues in the same scene.
    #[serde(default)]
    context: Vec<String>,
    url: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Glossary {
    game: String,
    edition: String,
    entries: BTreeSet<Term>,
}

#[derive(Default)]
pub struct Dictionary {
    entries: BTreeSet<Term>,
}

type Spellings<'a> = BTreeMap<&'a str, BTreeSet<&'a str>>;

/// OCR at the event's proof frame, its visual description, and adjacent OCR for glyph checks.
pub struct Evidence {
    pub ocr: String,
    observed: String,
    adjacent: String,
}

impl Evidence {
    pub fn new(seen: u64, ocr: &str, basis: &str) -> Self {
        let mut frames = BTreeMap::<u64, String>::new();
        let mut second = seen; // Older and single-frame observations may have no time labels.
        for line in ocr.lines() {
            let text = if let Some((label, text)) = line.split_once(' ')
                && let Ok(time) = label.parse::<u64>()
            {
                second = time;
                text
            } else {
                line
            };
            if second.abs_diff(seen) <= 1 {
                let frame = frames.entry(second).or_default();
                if !frame.is_empty() {
                    frame.push('\n');
                }
                frame.push_str(text);
            }
        }
        let ocr = frames.remove(&seen).unwrap_or_default();
        let observed = format!("{}\n{}", compact(&ocr), compact(basis));
        let adjacent = frames
            .into_values()
            .map(|text| compact(&text))
            .collect::<Vec<_>>()
            .join("\n");
        Self {
            ocr,
            observed,
            adjacent,
        }
    }
}

const TOPICS: [(&str, &str); 2] = [
    ("official", "公式サイト 登場人物 ワールド 地名 施設"),
    ("guide", "攻略 クエスト 依頼 ボス 形態 名前"),
];
const PROVIDERS: [&str; 3] = ["brave", "tavily", "command"];
const VIDEO_HOSTS: [&str; 7] = [
    "youtube.com",
    "youtu.be",
    "youtube-nocookie.com",
    "twitch.tv",
    "nicovideo.jp",
    "nico.ms",
    "bilibili.com",
];

fn compact(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_whitespace()).collect()
}

// A stable filename even for very long titles, or titles containing '/'.
// Identity is checked again inside the file before using any entries.
fn cache_id(game: &str, edition: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in game.bytes().chain([0]).chain(edition.bytes()) {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn source_allowed(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    let host = parsed.host_str().unwrap_or_default().trim_end_matches('.');
    matches!(parsed.scheme(), "https" | "http")
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && host.contains('.')
        && !url.chars().any(char::is_whitespace)
        && !VIDEO_HOSTS
            .iter()
            .any(|&blocked| host == blocked || host.ends_with(&format!(".{blocked}")))
}

fn scoped_terms(glossary: Glossary, game: &str, edition: &str) -> BTreeSet<Term> {
    if glossary.game != game || glossary.edition != edition {
        return BTreeSet::new();
    }
    glossary
        .entries
        .into_iter()
        .filter(|term| {
            valid_name(&term.name)
                && source_allowed(&term.url)
                && term.aliases.iter().all(|alias| valid_name(alias))
                && term.context.iter().all(|clue| !clue.trim().is_empty())
        })
        .collect()
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    Ok(Some(serde_json::from_reader(file)?))
}

pub fn load(game: &str, edition: &str, cache: &Path, provider: &str) -> Result<Dictionary> {
    let mut dictionary = Dictionary::default();
    let selected = match provider {
        "off" => return Ok(dictionary),
        "cache" => None,
        "auto" => PROVIDERS.into_iter().find(|name| configured(name)),
        name if PROVIDERS.contains(&name) => {
            if configured(name) {
                Some(name)
            } else {
                eprintln!("{} が未設定です。取得済みの辞書で続行します", setting(name));
                None
            }
        }
        _ => bail!("辞書検索に指定できる値は auto, brave, tavily, command, cache, off です"),
    };
    // Hand-verified aliases and edition-specific context can coexist with fetched entries.
    match read_json::<Vec<Glossary>>(&cache.join("local.json")) {
        Ok(Some(glossaries)) => {
            for glossary in glossaries {
                dictionary
                    .entries
                    .extend(scoped_terms(glossary, game, edition));
            }
        }
        Ok(None) => {}
        Err(error) => eprintln!("辞書 local.json を読み込めません: {error}"),
    }
    let id = cache_id(game, edition);
    for source in PROVIDERS {
        for (topic, words) in TOPICS {
            let path = cache.join(format!("{id}-{source}-{topic}.json"));
            match read_json::<Glossary>(&path) {
                Ok(Some(glossary)) => {
                    dictionary
                        .entries
                        .extend(scoped_terms(glossary, game, edition));
                    continue;
                }
                Ok(None) => {}
                Err(error) => {
                    eprintln!("辞書 {} を読み込めません: {error}", path.display());
                    continue;
                }
            }
            if selected != Some(source) {
                continue;
            }
            let query = format!(
                "{game} {edition} {words} -実況 -タイムスタンプ -site:youtube.com -site:youtu.be"
            );
            let entries = match search(source, &query) {
                Ok(response) => extract(&response, source, game, edition),
                Err(error) => {
                    eprintln!(
                        "{source} の検索を利用できません。取得済みの辞書で続行します: {error}"
                    );
                    continue;
                }
            };
            if !entries.is_empty() {
                let glossary = Glossary {
                    game: game.into(),
                    edition: edition.into(),
                    entries,
                };
                if let Err(error) = save(cache, &path, &glossary) {
                    eprintln!("辞書 {} を保存できません: {error}", path.display());
                }
                dictionary.entries.extend(glossary.entries);
            }
        }
    }
    Ok(dictionary)
}

fn save(cache: &Path, path: &Path, glossary: &Glossary) -> Result<()> {
    fs::create_dir_all(cache)?;
    let mut file = NamedTempFile::new_in(cache)?;
    serde_json::to_writer_pretty(&mut file, glossary)?;
    file.flush()?;
    // Concurrent runs of the same query can share the first complete snapshot.
    match file.persist_noclobber(path) {
        Err(error) if error.error.kind() == ErrorKind::AlreadyExists => Ok(()),
        result => result.map(|_| ()).map_err(Into::into),
    }
}

fn setting(source: &str) -> &'static str {
    match source {
        "brave" => "BRAVE_SEARCH_API_KEY",
        "tavily" => "TAVILY_API_KEY",
        _ => "GAMEPLAY_SEARCH_COMMAND",
    }
}

fn configured(source: &str) -> bool {
    env::var(setting(source)).is_ok_and(|value| !value.trim().is_empty())
}

fn search(source: &str, query: &str) -> Result<Value> {
    let key =
        env::var(setting(source)).with_context(|| format!("{} が未設定です", setting(source)))?;
    ensure!(
        !key.trim().is_empty() && !key.chars().any(char::is_control),
        "検索設定の値が不正です"
    );
    let mut command;
    let input;
    if source == "command" {
        command = Command::new(key);
        input = json!({"query": query}).to_string();
    } else {
        command = Command::new("curl");
        command.args([
            "--disable",
            "--fail",
            "--silent",
            "--show-error",
            "--max-time",
            "20",
            "--header",
            "@-",
        ]);
        if source == "brave" {
            command.args([
                "--get",
                "--data-urlencode",
                &format!("q={query}"),
                "--data",
                "country=JP",
                "--data",
                "count=10",
                "--data",
                "text_decorations=false",
                "https://api.search.brave.com/res/v1/web/search",
            ]);
            input = format!("X-Subscription-Token: {key}\n");
        } else {
            let request = json!({"query":query, "search_depth":"basic", "max_results":10,
                "exclude_domains":VIDEO_HOSTS, "include_answer":false});
            command.args([
                "--header",
                "Content-Type: application/json",
                "--data-binary",
                &request.to_string(),
                "https://api.tavily.com/search",
            ]);
            input = format!("Authorization: Bearer {key}\n");
        }
    }
    // File-backed output also lets us time out an unresponsive local adapter without
    // waiting for an inherited stdout pipe to be closed by one of its descendants.
    let mut response = tempfile::tempfile()?;
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(response.try_clone()?)
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .context("検索プロセスの入力を開けません")?;
    let write = stdin.write_all(input.as_bytes());
    drop(stdin);
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() >= Duration::from_secs(25) {
            child.kill()?;
            child.wait()?;
            bail!("検索の応答を25秒以内に受け取れませんでした");
        }
        thread::sleep(Duration::from_millis(50));
    };
    ensure!(status.success(), "検索プロセス終了 {status}");
    write?;
    ensure!(
        response.metadata()?.len() <= 4 * 1024 * 1024,
        "検索応答のサイズが大きすぎます"
    );
    response.rewind()?;
    let mut bytes = Vec::new();
    response.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    serde_json::from_slice(&bytes).context("検索応答の JSON が不正です")
}

static MARKUP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]*>").unwrap());
static RECORDING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r"(?i)タイムスタンプ|概要欄|実況|プレイ動画|timestamps?|gameplay\s+part|part\s*[0-9０-９]+|(?:^|\s)\d{1,3}:\d{2}(?:\D|$)"
).unwrap()
});
static NAMES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r"[「『]([^「『」』\n]{2,36})[」』]|(?:ボス|地名|場所|依頼|クエスト|登場人物|形態)(?:名)?[：:]\s*([^\s、。/｜|【】「」]{2,32})|([\p{Han}々]+(?:の[\p{Han}々]*)?[ァ-ヶ][ァ-ヶー・]+|[ァ-ヶ][ァ-ヶー・]+[\p{Han}々]*)"
).unwrap()
});
static FORM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([\p{Han}々]{2,}(?:の[\p{Han}々]*)?)([ァ-ヶ][ァ-ヶー・]{2,})$").unwrap()
});
static ALIASES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
    r"[「『]([^「『」』\n]{2,36})[」』]\s*[(（]\s*(?:別名|通称|別称)[：:\s]*[「『]?([^()（）「『」』、。\s]{2,32})"
).unwrap()
});

fn valid_name(name: &str) -> bool {
    let n = name.chars().count();
    (2..=48).contains(&n)
        && name == name.trim()
        && !name.chars().any(char::is_control)
        && !name.contains([
            '。', '、', '！', '？', '「', '」', '『', '』', '<', '>', '/', '\\',
        ])
}

fn extract(response: &Value, source: &str, game: &str, edition: &str) -> BTreeSet<Term> {
    let rows = if source == "brave" {
        &response["web"]["results"]
    } else {
        &response["results"]
    };
    let mut terms = BTreeSet::new();
    let game_key = compact(game);
    let edition_key = compact(edition);
    for row in rows.as_array().into_iter().flatten().take(20) {
        let Some(url) = row["url"].as_str().filter(|url| source_allowed(url)) else {
            continue;
        };
        let title = row["title"].as_str().unwrap_or_default();
        let snippet = row[if source == "brave" {
            "description"
        } else {
            "content"
        }]
        .as_str()
        .unwrap_or_default();
        let text = format!("{title}\n{snippet}");
        let text = MARKUP.replace_all(&text, "");
        let scope = compact(&text);
        if !scope.contains(&game_key) || !scope.contains(&edition_key) || RECORDING.is_match(&text)
        {
            continue;
        }
        let mut names: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for hit in NAMES.captures_iter(&text) {
            if let Some(name) = hit.iter().skip(1).flatten().next() {
                names.entry(name.as_str().trim().to_owned()).or_default();
            }
        }
        for hit in ALIASES.captures_iter(&text) {
            if valid_name(&hit[2]) {
                names
                    .entry(hit[1].to_owned())
                    .or_default()
                    .insert(hit[2].to_owned());
            }
        }
        for (name, mut aliases) in names {
            if !valid_name(&name) || compact(&name) == game_key {
                continue;
            }
            let mut context = Vec::new();
            if let Some(form) = FORM.captures(&name)
                && ![
                    "ゲーム",
                    "ボス",
                    "クエスト",
                    "攻略",
                    "チャート",
                    "イベント",
                    "キャラクター",
                    "マップ",
                    "サイト",
                    "ガイド",
                ]
                .contains(&&form[2])
            {
                aliases.insert(form[2].to_owned());
                context.push(form[1].to_owned());
            }
            terms.insert(Term {
                name,
                aliases: aliases.into_iter().collect(),
                context,
                url: url.into(),
            });
        }
    }
    terms
}

impl Term {
    fn supported(&self, evidence: &str) -> bool {
        evidence.contains(&compact(&self.name))
            || (self
                .aliases
                .iter()
                .any(|alias| evidence.contains(&compact(alias)))
                && (self.context.is_empty()
                    || self
                        .context
                        .iter()
                        .any(|cue| evidence.contains(&compact(cue)))))
    }
}

impl Dictionary {
    fn grounded<'a>(&'a self, evidence: &str) -> (BTreeSet<&'a str>, Spellings<'a>) {
        let mut exact = BTreeSet::new();
        let mut aliases: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for term in &self.entries {
            if term.supported(evidence) {
                if evidence.contains(&compact(&term.name)) {
                    exact.insert(term.name.as_str());
                }
                for alias in &term.aliases {
                    aliases.entry(alias).or_default().insert(&term.name);
                }
            }
        }
        (exact, aliases)
    }

    pub fn hints(&self, heading: &str, evidence: &Evidence) -> String {
        let (mut names, aliases) = self.grounded(&evidence.observed);
        for (alias, targets) in aliases {
            if targets.len() == 1 {
                let target = *targets.first().unwrap();
                let mut matches = heading.match_indices(alias).peekable();
                if matches.peek().is_none()
                    || matches.any(|(i, _)| {
                        !inside_read_name(heading, i, alias, target, &evidence.observed)
                    })
                {
                    names.insert(target);
                }
            }
        }
        names.into_iter().take(32).collect::<Vec<_>>().join("、")
    }

    pub fn resolve(&self, heading: &str, evidence: &Evidence) -> String {
        if self.entries.is_empty() {
            return heading.to_owned();
        }
        let (mut exact, mut replacements) = self.grounded(&evidence.observed);
        for term in &self.entries {
            // A spelling seen one frame away can corroborate a mistyped glyph, but cannot
            // select a boss form or supply context for expanding a shared alias.
            if evidence.adjacent.contains(&compact(&term.name)) {
                exact.insert(&term.name);
            }
            if !term.context.is_empty()
                && !term.supported(&evidence.observed)
                && let Some(base) = term.aliases.iter().find(|alias| {
                    term.name.ends_with(alias.as_str())
                        && evidence.observed.contains(&compact(alias))
                })
            {
                // An unconfirmed epithet can be replaced by a witnessed form or the witnessed base name.
                let witnessed = replacements
                    .get(base.as_str())
                    .and_then(|targets| (targets.len() == 1).then(|| *targets.first().unwrap()))
                    .unwrap_or(base);
                replacements
                    .entry(&term.name)
                    .or_default()
                    .insert(witnessed);
            } else {
                // Consume a complete name before considering any shorter component of it.
                replacements.entry(&term.name).or_default();
            }
        }
        // Consume the original once so corrections cannot expand each other.
        let mut rest = heading;
        let mut result = String::with_capacity(heading.len());
        while let Some(first) = rest.chars().next() {
            if let Some((&word, targets)) = replacements
                .iter()
                .filter(|(word, _)| rest.starts_with(**word))
                .max_by_key(|(word, _)| word.len())
            {
                let at = heading.len() - rest.len();
                let target = targets
                    .first()
                    .copied()
                    .filter(|target| {
                        targets.len() == 1
                            && !inside_read_name(heading, at, word, target, &evidence.observed)
                    })
                    .unwrap_or(word);
                result.push_str(target);
                rest = &rest[word.len()..];
                continue;
            }
            // For OCR-only mistakes the actual spelling must be visible in the accompanying frames.
            let mut candidates = exact
                .iter()
                .filter_map(|name| confused_prefix(rest, name).map(|len| (len, *name)));
            if let Some((len, name)) = candidates.next()
                && candidates.next().is_none()
            {
                result.push_str(name);
                rest = &rest[len..];
            } else {
                result.push(first);
                rest = &rest[first.len_utf8()..];
            }
        }
        result
    }
}

/// Do not expand a short alias inside a longer name that the heading and screen already
/// agree on, even when the longer name has no dictionary entry of its own.
fn inside_read_name(heading: &str, at: usize, alias: &str, target: &str, observed: &str) -> bool {
    let after = at + alias.len();
    observed.match_indices(alias).any(|(i, _)| {
        (!target.starts_with(alias)
            && same_name_edge(heading[..at].chars().rev(), observed[..i].chars().rev()))
            || (!target.ends_with(alias)
                && same_name_edge(
                    heading[after..].chars(),
                    observed[i + alias.len()..].chars(),
                ))
    })
}

fn same_name_edge(
    heading: impl Iterator<Item = char>,
    mut screen: impl Iterator<Item = char>,
) -> bool {
    let mut heading = heading.filter(|c| !c.is_whitespace());
    match (heading.next(), screen.next()) {
        // A kana particle or name separator alone is not evidence of a shared qualifier.
        (Some(a), Some(b)) if a == b && matches!(a, 'ぁ'..='ゖ' | '・' | '-' | '＝') => {
            matches!((heading.next(), screen.next()), (Some(a), Some(b)) if a == b && a.is_alphanumeric())
        }
        (Some(a), Some(b)) => a == b && a.is_alphanumeric(),
        _ => false,
    }
}

fn confused_prefix(text: &str, name: &str) -> Option<usize> {
    const CONFUSABLE: [&str; 14] = [
        "ホボポ",
        "ハバパ",
        "カガ",
        "トド",
        "ヘベペ",
        "シジ",
        "ソン",
        "シツ",
        "ロ口",
        "夕タ",
        "カ力",
        "エ工",
        "ュユ",
        "ッツ",
    ];
    if name.chars().count() < 5 {
        return None;
    }
    let mut chars = text.chars();
    let mut different = false;
    for expected in name.chars() {
        let actual = chars.next()?;
        if expected != actual {
            if different
                || !CONFUSABLE
                    .iter()
                    .any(|group| group.contains(expected) && group.contains(actual))
            {
                return None;
            }
            different = true;
        }
    }
    different.then(|| text.len() - chars.as_str().len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(text: &str) -> Evidence {
        Evidence::new(0, text, "")
    }

    #[test]
    fn only_sources_for_the_requested_game_and_edition_are_used() {
        let response = json!({"web":{"results":[
            {"title":"星の旅 HD版 公式", "url":"https://game.example.jp/world", "description":"<b>植物研究施設</b>「赤の植物園」（別名「紅苑」）"},
            {"title":"星の旅 原作", "url":"https://game.example.jp/original", "description":"「古い植物園」"},
            {"title":"月の旅 HD版", "url":"https://game.example.jp/moon", "description":"「月の庭」"},
            {"title":"星の旅 HD版", "url":"https://www.youtube.com/watch?v=test", "description":"「動画の掲示板」"},
            {"title":"星の旅 HD版", "url":"https://blog.example.jp/tips", "description":"0:00 「動画の目次」"}
        ]}});
        let entries = extract(&response, "brave", "星の旅", "HD版");
        assert!(
            entries
                .iter()
                .all(|e| e.url == "https://game.example.jp/world")
        );
        let term = entries.iter().find(|e| e.name == "赤の植物園").unwrap();
        assert_eq!(term.aliases, ["紅苑"]);
        let dictionary = Dictionary { entries };
        assert_eq!(
            dictionary.resolve("紅苑に到着", &frame("場所：紅苑")),
            "赤の植物園に到着"
        );
        assert!(
            dictionary
                .hints("酒場を出る", &frame("酒場を出る"))
                .is_empty()
        );
    }

    #[test]
    fn disambiguates_boss_forms_from_the_same_scene_without_inventing_an_epithet() {
        let game = "冒険家エリオットの千年物語";
        let response = json!({"results":[{
            "title":format!("{game} ボス攻略"),"url":"https://appmedia.jp/el1000/80024932",
            "content":"反逆者カイフリードの攻略。開闢の憎悪カイフリードの攻略。"
        }]});
        let dictionary = Dictionary {
            entries: extract(&response, "tavily", game, ""),
        };
        assert_eq!(
            dictionary.resolve("カイフリード戦", &frame("ボス：カイフリード")),
            "カイフリード戦"
        );
        assert_eq!(
            dictionary.resolve("カイフリード戦", &frame("ボス：カイフリード　称号：反逆者")),
            "反逆者カイフリード戦"
        );
        assert_eq!(
            dictionary.resolve("反逆者カイフリード戦", &frame("開闢の憎悪　カイフリード")),
            "開闢の憎悪カイフリード戦"
        );
        assert_eq!(
            dictionary.resolve("反逆者カイフリード戦", &frame("カイフリード")),
            "カイフリード戦"
        );
        assert_eq!(
            dictionary.resolve(
                "開闢の憎悪カイフリード戦",
                &frame("開闢の憎悪 カイフリード")
            ),
            "開闢の憎悪カイフリード戦"
        );
        assert_eq!(
            dictionary.resolve(
                "カイフリード戦",
                &frame("反逆者カイフリード と 開闢の憎悪カイフリード")
            ),
            "カイフリード戦"
        );
    }

    #[test]
    fn corrects_a_glyph_only_with_a_confirming_spelling_and_preserves_numbers() {
        let game = "ユミアのアトリエ";
        let response = json!({"results":[{
            "title":format!("{game} ワールド"),"url":"https://atelier.games/yumia/jp/world/",
            "content":"リグナス地方の施設「アルボール植物園」"
        }]});
        let dictionary = Dictionary {
            entries: extract(&response, "command", game, ""),
        };
        let heading = "目標2 アルホール植物園の探索";
        assert_eq!(
            dictionary.resolve(heading, &frame("目標欄：アルホール植物園")),
            heading
        );
        assert_eq!(
            dictionary.resolve(
                heading,
                &Evidence::new(
                    27,
                    "27 目標欄：アル ホール植物園\n28 場所表示：アル ボール 植物園",
                    ""
                )
            ),
            "目標2 アルボール植物園の探索"
        );
        assert_eq!(
            dictionary.resolve(
                heading,
                &Evidence::new(
                    27,
                    "3 場所表示：アルボール植物園\n27 目標欄：アルホール植物園",
                    ""
                )
            ),
            heading
        );
    }

    #[test]
    fn a_shared_alias_needs_context_at_the_proof_second_and_must_preserve_a_read_compound() {
        let dictionary = Dictionary {
            entries: [Term {
                name: "北の案内人".into(),
                aliases: vec!["案内人".into()],
                context: vec!["北".into()],
                url: "https://game.example.jp/guide".into(),
            }]
            .into(),
        };
        let heading = "南の案内人と話す";
        let change = Evidence::new(
            37,
            "36 北の案内人\n37 人物の名札\n南の案内人\n38 次の人物\n北の案内人",
            "名札に「南の案内人」とある",
        );
        assert_eq!(change.ocr, "人物の名札\n南の案内人");
        assert!(dictionary.hints(heading, &change).is_empty());
        assert_eq!(dictionary.resolve(heading, &change), heading);
        let next_only = Evidence::new(37, "37 \n38 北の案内人", "案内人の名札が点灯");
        assert_eq!(
            dictionary.resolve("案内人と話す", &next_only),
            "案内人と話す"
        );

        // The heading's longer name can be real even if it has no dictionary entry.
        let same = frame("NPC：南の案内人　北から到着");
        assert!(dictionary.hints(heading, &same).is_empty());
        assert_eq!(dictionary.resolve(heading, &same), heading);
        for name in ["古き案内人", "ミラ・案内人", "南の 案内人"] {
            let proof = frame(&format!("名札：{name}　北から到着"));
            assert_eq!(dictionary.resolve(name, &proof), name);
        }
        assert_eq!(
            dictionary.resolve(heading, &frame("名札：南の案内人・北の案内人")),
            heading
        );

        let identified = Evidence::new(37, "37 名札：案内人\n所属：北", "");
        assert_eq!(dictionary.hints("案内人と話す", &identified), "北の案内人");
        assert_eq!(
            dictionary.resolve("案内人と話す", &identified),
            "北の案内人と話す"
        );
    }

    #[test]
    fn cache_is_reusable_across_parts_and_off_disables_all_sources() {
        let dir = tempfile::tempdir().unwrap();
        let game = "テスト/作品";
        let glossary = Glossary {
            game: game.into(),
            edition: "移植版".into(),
            entries: [Term {
                name: "夜明けの塔".into(),
                aliases: vec!["暁塔".into()],
                context: vec![],
                url: "https://game.example.jp/area".into(),
            }]
            .into(),
        };
        let path = dir
            .path()
            .join(format!("{}-brave-official.json", cache_id(game, "移植版")));
        save(dir.path(), &path, &glossary).unwrap();
        save(
            dir.path(),
            &path,
            &Glossary {
                game: game.into(),
                edition: "移植版".into(),
                entries: BTreeSet::new(),
            },
        )
        .unwrap();
        let load_for = |edition, mode| {
            load(game, edition, dir.path(), mode)
                .unwrap()
                .resolve("暁塔で探索", &frame("場所：暁塔"))
        };
        assert_eq!(load_for("移植版", "cache"), "夜明けの塔で探索");
        assert_eq!(load_for("原作", "cache"), "暁塔で探索");
        assert_eq!(load_for("移植版", "off"), "暁塔で探索");
    }
}
