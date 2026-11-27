# 録画ごとのタイムスタンプ評価

採点は生成後にローカルで行う。見本の掲載漏れと生成結果の誤検出を区別するため、録画を通して調べた区間と、見本を起点に個別に確認した出来事を別々に記録する。採点ツールは Python 3 の標準ライブラリで動く。

## 録画と評価条件

最初に『冒険家エリオットの千年物語』Part5 を採点する。次に下表の全作品を録画別に集計し、見本の原文行数が合計 153 件か確認する。[Issue #4](https://github.com/mizucopo/gameplay-timestamps/issues/4)

| 録画 | 見本の原文行数 |
| --- | ---: |
| 冒険家エリオットの千年物語 Part5 | 24 |
| スプラトゥーン レイダース Part3 | 10 |
| ユミアのアトリエ Part4 | 27 |
| ステラーブレイド Part4 | 23 |
| ドラゴンクエスト1&2 DQ2編 Part2 | 16 |
| AI：ソムニウムファイル ニルヴァーナ イニシアチブ Part6 | 33 |
| ゼルダの伝説 知恵のかりもの Part3 | 20 |

1. 録画、生成結果、採点資料の保存先を分ける。生成処理が読む素材は元動画と利用を許可した字幕・辞書だけにする。手作業の見本、採点票、対象動画の概要欄をプロンプト、辞書作成、文字起こしの補完に使わない。
2. 評価者に動画だけを渡し、選定済みの連続区間を始めから終わりまで見てもらう。選定時に見本の空白区間と最終行の後も含める。評価者には見本や生成結果を表示しない。各出来事の開始を許せる最早秒・最遅秒と、それを判断した画面の再生時刻を `blind.json` に記録する。導入が区間の外なら、その場面も見られるよう区間を広げる。
3. この記録を保存してから原文の見本を開く。原文は一字も直さず `original.txt` に保存する。見本にだけある出来事は映像で確認し、必要に応じて公式資料の表記と照合する。追加で確認した出来事を `verified.json` に保存し、原文の各行と照合する。表記の訂正や頭出しの修正理由は採点票の `note` に残す。
4. 同じ録画で、音声あり／なし × 辞書あり／なしの 4 条件を実行する。それぞれの `timestamps.txt` と `evidence.jsonl`、根拠画像を別のディレクトリに保存する。録画を識別できるハッシュ、生成コミット、モデル、サンプリング設定、字幕・発話の取得状況、辞書の版を記録する。比較する条件以外は固定する。利用可能な発話が 0 件だった録画も、その旨を記す。
5. 各条件の開始秒・見出し・根拠画像を元映像と確認し、同じ出来事を指す行に同じイベント ID を付ける。言い換えも意味で対応付ける。再開した依頼、同一地点の別イベント、開始・終了、形態は区別する。同じ表示時刻や名称だけで同一と判断しない。
6. 開発中に使った録画は `development`、凍結した設定の確認は `validation` とする。汎用性の最終確認には、設計、調整、辞書の手修正、見本の参照のいずれにも使っていない録画をあらかじめ `holdout` に取り置く。7 本をすべて開発に使った場合は別の録画を使う。未使用の録画に原文見本がなくても、映像だけで決めた連続区間を採点できる。

概要欄の文字数上限に合わせて行を選んだ録画も、視聴者に渡す `timestamps.txt` を採点する。検出後の取捨選択でどの場面が省かれたかは `timestamps-full.txt` と `evidence-full.jsonl` で追跡できる。

## 採点資料

録画ごとに `case.json` を用意する。各パスはこのファイルの場所が基準。`duration_seconds` は元動画の長さを秒に切り上げた整数とする。以下はファイル構成を示す架空の短い録画の例。

```json
{
  "recording": "評価例 Part1",
  "duration_seconds": 90,
  "split": "development",
  "blind": "blind.json",
  "verified": "verified.json",
  "original": "original.txt",
  "reference_review": "original.tsv",
  "runs": [
    {"audio": true, "dictionary": true, "timestamps": "a1d1/timestamps.txt", "review": "a1d1/review.tsv"},
    {"audio": true, "dictionary": false, "timestamps": "a1d0/timestamps.txt", "review": "a1d0/review.tsv"},
    {"audio": false, "dictionary": true, "timestamps": "a0d1/timestamps.txt", "review": "a0d1/review.tsv"},
    {"audio": false, "dictionary": false, "timestamps": "a0d0/timestamps.txt", "review": "a0d0/review.tsv"}
  ]
}
```

`blind.json` の調査区間 `intervals` は開始秒を含み終了秒を含まない。イベントの開始許容範囲 `start` は両端を含む。調査済み区間で出来事が 0 件なら `events` は空配列でよい。通して判断できた区間がまだない録画は両方を空配列にし、個別に裏付けられた出来事だけを `verified.json` に残す。その場合、未対応行はすべて未調査になる。

```json
{
  "intervals": [[0, 30], [60, 90]],
  "events": [
    {"id": "opening", "heading": "ゲーム開始", "start": [0, 0], "evidence": "0:00 タイトル画面"},
    {"id": "gate", "heading": "東門へ到着", "start": [10, 12], "evidence": "0:14 の地名表示とその直前の移動", "names": {"place": "東門"}}
  ]
}
```

`verified.json` は追加確認したイベントの配列で、各要素は `blind.json` のイベントと同じ形式。追加がなければ `case.json` の `verified` 項目を省ける。個別確認から、前後の区間全体を調査済みとは推定しない。`names` と `numbers` には採点したい固有名詞・数字を、イベント内で一意の欄名と確認済みの表記で記す。訂正した名称の出典 URL や目標欄の再生時刻を `evidence` に添える。

見本と各生成結果について、次のコマンドで空の照合用 TSV を一度ずつ作る。

```sh
python3 tools/evaluate.py sheet /path/to/case/original.txt > /path/to/case/original.tsv
python3 tools/evaluate.py sheet /path/to/case/a1d1/timestamps.txt > /path/to/case/a1d1/review.tsv
```

`line`、`time`、`heading` は入力に対応する。入力を更新したら照合をやり直す。残りの列は次のように記入する。

| 列 | 記入内容 |
| --- | --- |
| `event` | 意味で対応するイベント ID。単に時刻がずれていても同じ ID。確認済みのどの出来事にも対応しない行は `-`。同じ場面か判断できない行は `?` にして理由を `note` に書く。同じ出来事の 2 行目以降にもその ID を付ける。 |
| `correct` | その行で正しく書けている `names` / `numbers` の欄名。意味が同じ数字表現も含む。 |
| `wrong` | 誤った表記・数値の欄名。実際に出た表記は `heading` に残る。 |
| `omitted` | 見出しがその情報に触れていない欄名。 |
| `note` | 頭出し、同一性、訂正などの判断根拠。 |

同じ列に複数の欄名が入る場合は `place|boss` のように `|` でつなぐ。対応するイベントに設定した全欄を 3 種類のいずれかに一度ずつ割り当てる。`event` が `-` または `?` の行では 3 列を空にする。見本のない `holdout` では `original` と `reference_review` を両方省略する。

## 集計

```sh
python3 tools/evaluate.py score /path/to/elliott-part5/case.json > /path/to/elliott-report.json
python3 tools/evaluate.py score /path/to/cases/*/case.json > /path/to/study-report.json
```

`recordings` は録画別の結果で、冒頭の `original_lines` は見本の合計行数。各録画の `reference` に原文の判定、`events` に確認済みの表記・開始許容範囲・根拠、`runs` に各条件の生成結果を保存する。最初の 1 条件から採点できる。残りは `missing_conditions` に出る。

| 項目 | 数え方 |
| --- | --- |
| `matched` / `missed` | 確認済みの出来事ごとの対応件数。`missed` には対応を確認できなかったイベント ID が残る。判定保留の行があれば、`uncertain` の理由と合わせて読める。 |
| `extra` / `unassessed` | 未対応行は開始秒が連続調査区間内なら余分な行、区間外なら未調査。見本の最終行以降にも同じ基準を使う。 |
| `uncertain` | 対応判定を保留した行の件数と理由。調査区間の内外を問わず独立に集計する。 |
| `duplicates` | 同じイベントへの複数の行は、開始許容範囲に最も近い 1 行を代表にする。同距離なら先に出た行を取り、残りを重複として記録する。 |
| `outside_start_window` | 代表行の開始秒が許容範囲から外れた件数。`drift_seconds` は早い場合が負、遅い場合が正、範囲内が 0。録画の長さを越えた行は `out_of_recording` にも数える。 |
| `wrong_names` / `wrong_numbers` | 対応が確認できた全行で誤った固有名詞・数字の欄数。言及を省いた欄は `omitted` として区別する。 |
| `comparisons` | 同一録画で他方の条件を固定した比較。両側で検出した件数 `common`、新規検出 `gained`、検出消失 `lost`、各件数の差を出す。辞書比較の `corrected_count` / `regressed_count` は、両側の代表行で同じイベント・同じ欄の判定が誤→正／正→誤になった件数。新規検出や言及を省いた欄はこの分子に入らない。 |

`surveyed_seconds` と区間リストを数値に添えて報告する。未調査区間の未対応行がある録画では全編を通した誤検出率を確定できない。`holdout` の結果は開発録画と分けて読む。
