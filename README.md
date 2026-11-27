# gameplay-timestamps

ゲームタイトルと録画を渡すと、YouTube の概要欄に貼り付けられる日本語の `timestamps.txt` を生成します。MP4 の映像を先頭から最後まで走査し、導入の再生秒、根拠を確認した再生秒、両方の画像を保存します。

## 実行

Rust、FFmpeg（`ffmpeg` と `ffprobe`）、同じ PC で動く画像入力対応の OpenAI 互換サーバーを用意してください。日本語と英語の言語データを入れた Tesseract があると、画面に表示された名前の照合にも使います。

```sh
cargo build --locked --release
# vLLM を使う場合は別のターミナルで起動
vllm serve Qwen/Qwen3-VL-4B-Instruct --host 127.0.0.1 --port 8000
```

macOS での実録画評価には llama.cpp と Qwen3-VL-8B-Instruct の GGUF を使いました。画像入力用プロジェクターも同じモデルのものを指定します。

```sh
llama-server -m /path/to/Qwen3VL-8B-Instruct-Q4_K_M.gguf \
  --mmproj /path/to/mmproj-Qwen3VL-8B-Instruct-Q8_0.gguf \
  --host 127.0.0.1 --port 8000 -c 16384
```

```sh
./target/release/gameplay-timestamps "ゲームタイトル" "/path/to/Part5.mp4"
```

実行後に表示される `Part5.timestamps/timestamps.txt` を開いてください。先頭は `0:00`、1時間以降は `1:00:00` の形式です。`evidence.jsonl` の各行は同じ順の見出しに対応します。`start` と `start_image` は導入、`seen` と `evidence_image` は判定の根拠です。たとえば会話の途中で依頼名が出た場合、両者の時刻は別になります。映像トラックが遅れて始まる録画では、先頭行の `start_image` は `null`、`evidence_image` は最初に表示されたフレームです。出力がすでにある場合は `-o 別のディレクトリ` を指定してください。

| 用途 | 指定 |
| --- | --- |
| リメイクや移植の版を区別する | `--edition "HD-2D版"` |
| ローカルサーバーの設定を変える | `--api-base http://127.0.0.1:8001/v1 --model モデル名`。サーバー認証は `GAMEPLAY_VLM_API_KEY` |
| 作品資料を検索する | `--search auto` が既定。Brave、Tavily、ローカルの検索コマンドから選択できる。取得済み辞書で実行するには `--search cache` |
| ローカルで発話を取得する | `--transcriber /path/to/command`。コマンドに引数として元動画を渡し、標準出力から元の再生時刻で記述した SRT / WebVTT を読む |
| 別の日本語 OCR を使う | `--ocr-command /path/to/command`。JPEG を標準入力、読み取った UTF-8 テキストを標準出力とする |
| 長い録画の再試行や条件比較 | `--inference-cache /path/to/private-cache`。同一入力の推論応答と完了済みの映像観察を再利用する。モデルの重みや OCR 環境を差し替えたら新しいディレクトリを使う |

映像走査の既定間隔は1秒です。代表画像を時間順にまとめ、固定画面の会話、目標表示、短い画面変化も調べます。依頼やボスの名称が遅れて出たときは前の映像をたどり、前後の拡大画像で見出しを確認します。字幕は同名の `.ja.srt` / `.ja.vtt`、`.srt` / `.vtt`、または内蔵のテキスト字幕から自動で読みます。字幕や文字起こしに物語の節目を示す台詞があると、その前後の元映像を確認し、出来事や会話の導入が画面に写っていれば見出しを追加したり開始時刻を早めたりします。音声なし、無音、文字起こし失敗時も同じコマンドで処理できます。

複数の画面で地名表示や依頼票の名称を確かめた場合、`evidence.jsonl` の `screen_label` に読み取った表記を保存します。

映像と中間画像の処理にはループバック上のモデルを使います。外部の辞書検索に使う情報は作品名・版・照会語です。[作品別辞書と OCR](docs/dictionary.md)・[実録画の採点方法](docs/evaluation.md)・[実録画による評価](docs/evaluation-results.md)・[vLLM の画像入力](https://docs.vllm.ai/en/stable/features/multimodal_inputs/)
