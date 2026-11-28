# 作品別の表記辞書

CLI は作品名と版を使って公式資料・攻略情報を検索します。人物・地名・依頼・ボスの表記を、その出典 URL とともにローカルに保存します。次の Part を同じ作品名・版・辞書ディレクトリで処理すると、取得済みの辞書を再利用します。同名作品やリメイクには `--edition` で「HD-2D」など資料上でも確認できる版を指定してください。

| 検索設定 | 動作 |
| --- | --- |
| `auto` | `BRAVE_SEARCH_API_KEY`、`TAVILY_API_KEY`、`GAMEPLAY_SEARCH_COMMAND` の順に最初の設定を使います。未設定ならキャッシュを使います。 |
| `brave` | `BRAVE_SEARCH_API_KEY` を使って Brave Web Search を呼び出します。 |
| `tavily` | `TAVILY_API_KEY` を使って Tavily Search の basic 検索を呼び出します。 |
| `command` | `GAMEPLAY_SEARCH_COMMAND` で指定した実行ファイルを使います。契約に含まれる検索 API や自前の検索インデックスを接続できます。 |
| `cache` | 保存済みの検索結果とローカルの確認済み表記を使います。 |
| `off` | 辞書を無効にします。辞書の有無で結果を比較するときに使います。 |

組み込みの検索には `curl` が必要です。最初の実行では作品名・版に公式資料向けと攻略向けの固定語を添え、最大2件の検索を行います。キーのプランに応じて検索料金が発生します。検索先に渡すデータはこの検索文と検索オプションです。検索失敗時も取得済みの用語とローカル映像で続行します。[Brave Search API](https://api-dashboard.search.brave.com/app/documentation/web-search)・[Tavily Search API](https://docs.tavily.com/documentation/api-reference/endpoint/search)

`command` にはシェルを経由せず、実行ファイルの標準入力に `{"query":"…"}` を渡します。標準出力に `{"results":[{"title":"作品名と版を含むページタイトル","url":"https://…","content":"検索抜粋"}]}` を UTF-8 で返してください。実行ファイルでは契約先の認証を扱えます。録画・評価用の見本・動画説明欄をインデックスに含めず、公式資料や攻略資料を対象としてください。

## 画面との照合

検索結果に作品名と指定の版の両方が現れることを確認し、日本語の引用符・名称ラベル・漢字とカナの複合語などから表記を抽出します。動画サイト、実況・タイムスタンプを含む検索結果は除外します。検索結果から直接タイムスタンプを作成する処理はありません。

辞書候補を出せるのは、同じ場面の証拠に対応する表記・別名がある場合です。称号のあるボスは画面にある称号や話者・場所・進捗を示す登録済みの語で区別します。別フレームで資料上の表記が読めた場合には、濁点などのよく似た1字を含む見出しも訂正できます。複数候補が残る略称は元の表記を維持します。

たとえば『ユミアのアトリエ』の目標欄と隣のフレームで「アルボール植物園」を確認できれば、施設名を見出しにも使えます。[コーエーテクモゲームスのワールド紹介](https://atelier.games/yumia/jp/world/)

『冒険家エリオットの千年物語』の「反逆者カイフリード」と「開闢の憎悪カイフリード」は、共通名に加えてその場面で読める称号を使って区別します。[AppMedia の当該ボス攻略](https://appmedia.jp/el1000/80024932)

### 画面文字の読み取り

標準では Tesseract の日本語・英語データ（`jpn` と `eng`）を使用します。`tesseract --list-langs` でインストール状況を確認できます。認識した単語を画面上の行にまとめ、信頼度の低い行と記号だけの行を除きます。[Tesseract のコマンドラインと TSV 出力](https://tesseract-ocr.github.io/tessdoc/Command-Line-Usage.html)

日本語認識に対応する macOS では、Apple Vision を使う OCR コマンドもビルドできます。リポジトリのルートで次を実行してください。

```sh
mkdir -p target
swiftc -O tools/vision-ocr.swift -o target/vision-ocr
cargo run --release -- "ゲームタイトル" recording.mp4 --ocr-command ./target/vision-ocr
```

`--ocr-command` にはローカルの実行ファイルを指定します。各画像を引数なしのコマンドの標準入力に JPEG として渡し、標準出力から UTF-8 のテキストを読みます。読み取りが成功して文字がなければ空の出力、コマンドが失敗したときは非ゼロの終了コードを返してください。[Apple Vision の文字認識](https://developer.apple.com/documentation/vision/vnrecognizetextrequest)

同梱の `vision-ocr --batch` は複数画像を同じプロセスで順番に処理できます。各 JPEG の前に4バイトのビッグエンディアン符号なし整数でバイト長を付け、標準入力に連結して送ります。画像ごとに認識した文字列を JSON でエンコードし、改行で区切って標準出力に返します。

## 確認済みの別名を取り込む

検索キャッシュには作品・版・提供元・照会の種類ごとに JSON ファイルができます。新版の資料で再検索するときは該当する検索キャッシュを退避して再実行してください。手元で確認した表記は、辞書ディレクトリの `local.json` にも追加できます。次の例の2件は別の Part でも使えます。

```json
[
  {
    "game": "冒険家エリオットの千年物語",
    "edition": "",
    "entries": [
      {
        "name": "反逆者カイフリード",
        "aliases": ["カイフリード"],
        "context": ["反逆者"],
        "url": "https://appmedia.jp/el1000/80024932"
      },
      {
        "name": "開闢の憎悪カイフリード",
        "aliases": ["カイフリード"],
        "context": ["開闢の憎悪"],
        "url": "https://appmedia.jp/el1000/80024932"
      }
    ]
  }
]
```

`aliases` と `context` は省略できます。`context` があると、別名から正式名を選ぶには同じ場面の証拠に少なくとも1つの語が必要です。正式名そのものを画面で読めた場合は直接使用できます。
