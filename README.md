# gameplay-timestamps

ゲームタイトルと録画を渡すと、YouTube の概要欄に貼り付けられる日本語の `timestamps.txt` を生成します。MP4 の映像を先頭から最後まで走査し、導入の再生秒、根拠を確認した再生秒、両方の画像を保存します。

## 実行

Rust、FFmpeg（`ffmpeg` と `ffprobe`）、同じ PC で動く画像入力対応の OpenAI 互換サーバーを用意してください。日本語と英語の言語データを入れた Tesseract があると、画面に表示された名前の照合にも使います。

```sh
cargo build --locked --release
# vLLM を使う場合は別のターミナルで起動
vllm serve Qwen/Qwen3-VL-8B-Instruct --host 127.0.0.1 --port 8000
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

タイムスタンプだけを貼ったときに日本語の動画説明欄へ収まるよう、改行を含めて半角換算5,000文字までにします。超える場合は録画の先頭と最後、プレイ再開とその直後の場面紹介、依頼や戦闘などの節目を優先し、同じ優先度では録画全体に分散させます。予約文字の `<` と `>` は貼り付け用には全角にします。行を選んだり表記を調整した場合、元の全行と根拠を `timestamps-full.txt` と `evidence-full.jsonl` に保存します。[YouTube ヘルプの日本語の文字数制限](https://support.google.com/youtube/answer/12948449?hl=ja)

| 用途 | 指定 |
| --- | --- |
| リメイクや移植の版を区別する | `--edition "HD-2D版"` |
| ローカルサーバーの設定を変える | `--api-base http://127.0.0.1:8001/v1 --model モデル名`。サーバー認証は `GAMEPLAY_VLM_API_KEY` |
| 作品資料を検索する | `--search auto` が既定。Brave、Tavily、ローカルの検索コマンドから選択できる。取得済み辞書で実行するには `--search cache` |
| ローカルで発話を取得する | `--transcriber /path/to/command`。コマンドに引数として元動画を渡し、標準出力から元の再生時刻で記述した SRT / WebVTT を読む |
| 別の日本語 OCR を使う | `--ocr-command /path/to/command`。JPEG を標準入力、読み取った UTF-8 テキストを標準出力とする |
| 長い録画の再試行や条件比較 | `--inference-cache /path/to/private-cache` または環境変数 `GAMEPLAY_INFERENCE_CACHE` で、同一入力の推論応答と完了済みの映像観察を次回も再利用する。モデルの重みや OCR 環境を差し替えたら新しいディレクトリを使う |

映像走査の既定間隔は1秒です。代表画像を時間順にまとめ、固定画面の会話、目標表示、短い画面変化も調べます。依頼やボスの名称が遅れて出たときは前の映像をたどり、前後の拡大画像で見出しを確認します。字幕は同名の `.ja.srt` / `.ja.vtt`、`.srt` / `.vtt`、または内蔵のテキスト字幕から自動で読みます。字幕や文字起こしに物語の節目を示す台詞があると、その前後の元映像を確認し、出来事や会話の導入が画面に写っていれば見出しを追加したり開始時刻を早めたりします。音声なし、無音、文字起こし失敗時も同じコマンドで処理できます。

同じ実行中に同じ画像・指示・応答形式の組み合わせが再び必要になった場合は、最初の推論結果を読み戻します。共有キャッシュを指定しなかった場合、この一時保存はコマンドの終了時に片付けます。

複数の画面で地名表示や依頼票の名称を確かめた場合、`evidence.jsonl` の `screen_label` に読み取った表記を保存します。

映像と中間画像の処理にはループバック上のモデルを使います。外部の辞書検索に使う情報は作品名・版・照会語です。[作品別辞書と OCR](docs/dictionary.md)・[vLLM の画像入力](https://docs.vllm.ai/en/stable/features/multimodal_inputs/)

開発用7作品で音声・辞書を併用したとき、手作業の見本153件中52件に対応しました。設定を固定して試した別録画では、連続調査で確認した7件中1件に対応しました。[作品別の全29条件の結果](docs/evaluation-comparison.md)・[頭出しの実例と失敗例](docs/evaluation-results.md)・[採点方法](docs/evaluation.md)

### Apple Silicon Mac で日本語の発話を使う

FFmpeg と、ローカルに保存した MLX Whisper モデルを用意してください。実録画の評価では `kaiinui/kotoba-whisper-v2.0-mlx` を使いました。リポジトリのルートで以下を実行し、`GAMEPLAY_WHISPER_MODEL` に `config.json` と重みが入ったディレクトリを指定します。

```sh
python3 -m venv .venv
source .venv/bin/activate
python -m pip install 'mlx-whisper==0.4.3'
export GAMEPLAY_WHISPER_MODEL="/path/to/kotoba-whisper-v2.0-mlx"
export GAMEPLAY_TRANSCRIBER="$PWD/tools/transcribe-mlx.py"
./target/release/gameplay-timestamps "ゲームタイトル" "/path/to/Part5.mp4"
```

アダプターは先頭の音声トラックを 16 kHz のモノラルに変換し、450秒ごとに前後2秒を添えて Mac 内で認識します。SRT の時刻は音声トラックの開始が遅い場合も録画の再生開始から数えます。全区間を処理すると標準出力に SRT を書き出します。字幕だけを保存するには `"$GAMEPLAY_TRANSCRIBER" "/path/to/Part5.mp4" > Part5.srt` を実行してください。
