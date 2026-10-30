# voice-agent 設計

2026-10-28 に作った設計案です。下の「決まったこと」と「P0 で設計から変えたこと」のあとに、設計案をそのまま残しています（【事実】はコードやログで確かめた内容、【推測】は確かめていない見立てです）。

## 決まったこと（2026-10-28）

- UI は Ghostty の専用ウィンドウ 1 枚で動く TUI にする
- 「マイクを切って」はマイクのデバイスごと止める。戻すのは m キー
- 声で割り込んだときは、読み上げだけを止める。返答の生成も止めたいときは「生成もストップ」と言う
- 人格は今はつけない（`persona.md` の差し込み口だけ用意する）
- 置き場所は新しいプロジェクト `~/voice-agent`

## P0 で設計から変えたこと

- 声のコマンド（`voice/commands.rs`）は P0 で入れた。言ったことの全体が一致したときだけ働き、「サーバーを止めて」のような頼みごとは Claude に届く
- 「止めて」のあとは、その返答の残りを画面に出すだけで読み上げない（返答は続くので、止めないと次の文から読み上げが再開してしまうため）
- コマンドを動かすツールは、承認ツールでの判定に加えて、実行の直前にもポリシーを通す。承認ツールで許可された呼び出しは覚えておき、同じ入力の実行では二度聞かない。承認ツールを通らずに届いた呼び出しは、その場で確認する
- 設定ファイルの場所を変える `VA_CONFIG` をテストの入口に足した（Whisper と AivisSpeech を起動せずに通しの確認をするため）
- 同じ実行時ディレクトリで 2 つ目が起動しないよう、`voice-agent.lock` で排他にした
- `ghostty.rs`、`show_shell`、`launch`、`doctor` は、使う段階（P5、P7）で入れる
- 終了時（q、ウィンドウを閉じたとき）は、裏のシェルとジョブも止める（設計の「決めていただきたいこと」8）。Esc と「止めて」ではジョブは止めない
- レビューを受けて変えたこと（2026-10-29）
  - 承認の返事は、全体が「はい」系のときだけ許可し、否定の言葉が入っていれば拒否する（日本語は否定が後ろに来るため、先頭だけを見ると「許可しません」が許可になっていた）。承認が出る前に話し始めた発話は答えにしない
  - 承認が重なったら古いものから順に、表示・読み上げ・回答をそろえる（表示は新しいもの、回答は古いもの、とずれていた）
  - 承認待ちの「静かにして」は読み上げを止めるだけにする。「止めて」は拒否
  - 話し始めたら、返答の残りは読み上げない。録音中に出た承認は、話し終えてから読み上げる
  - MCP の設定（トークン入り）は、claude の引数ではなく 0600 のファイルで渡す（macOS では他のアカウントからも引数が見える）
  - ジョブは自分のセッションで動かし（端末を持たない）、終了時は SIGKILL まで待つ
  - 裏のシェルは `!` を展開せず、履歴を残さない

---

コードの変更、ファイルの作成、コミット、ダウンロードはしていません。`~/edex-rs`（develop）を読み、ターミナルで状況を確かめ、Web を少し検索しただけです。
- 【事実】はコードやログで確かめた内容です。
- 【推測】は確かめていない見立てです。

---

## 1. 移植元の調査結果

### 1.1 ~/edex-rs の描画方式【事実】

- **TUI です。** 描画は `src/ui/canvas.rs` が ratatui の `Buffer` に描き、ANSI のバイト列にしたものを、Ghostty の split で動く `edex hud` プロセスに unix ソケットで送っています。
- シェルは Ghostty の本物のサーフェスです。edex は pty を素通しで中継するだけです（`src/relay`）。split は `src/ghostty.rs` が osascript で AppleScript を呼んで作ります。
- `web/globe` は three.js の地球儀で、Carbonyl で見るための実験です。本体の UI ではありません。

### 1.2 terminal-browser のしくみ【事実】

- 実体は `~/.local/share/terminal-browser/app` の v0.13.4（stable）です。Electron 上の Chromium で、絵をターミナルに描きます。
- `open <url> [--split right]`、`ls [--all --json]`、`action [--browser KEY --tab ID] -- snapshot|click|fill|eval`、`shutdown` があります。
- `action` は同梱の `agent-browser` を `--cdp <port>` 付きで呼ぶラッパーです。
- Ghostty は `TERM_PROGRAM` などで見分け、自動操作には Ghostty 1.3 以上が必要です（main.js 653〜669 行）。
- `new-tab` のヘルプには「TTY の無いシェルから実行すると右に split して開く」とあります。
- シェルの `terminal-browser` 関数には `TMUX` を偽る仕掛けが残っていますが、これは Go 版の頃のものです。

### 1.3 既存ログからの事実

- `~/Library/Caches/edex/edex.log` には `aivis: cold: 2.4〜3.2s for 2.3s of audio` が多数あります。
- Whisper モデル（large-v3-turbo q5_0）は 573.40 MB です。
- `aivis.rs` の `acquire()` は、busy フラグと condvar（`notify_one`）だけで使用権を取ります。優先度が無いので、温め（`warm_pass`）が本番の `synth` より先に使用権を取れます。ご指摘の問題は、コード上この作りが原因です。

### 1.4 仕分け表

再利用度は、高＝ほぼそのまま、中＝一部手直し、低＝考え方だけ使う、です。

| パス | 行数 | 依存 | 扱い | 再利用度・内容 |
|---|---|---|---|---|
| src/ai/stt.rs | 213 | net, util, regex | 持っていく | 高。whisper-server を常駐させる起動処理と、無音時の作り話を捨てる処理はそのまま。先行実行のため、結果が古くなったら捨てる印を足す |
| src/ai/aivis.rs | 540 | audio, playback, tts::Cancel, net | 一部作り直す | 中。エンジンの起動、`request`、エラー分類は持っていく。`acquire`、`keep_warm`、`warm_pass` は優先度付きの調停役に作り直す |
| src/ai/tts.rs | 991 | aivis, playback, regex | 持っていく＋手直し | 中〜高。`SentenceChunker`、`speakable`、`take_chunk`、`clause_cut`、先読み合成（最大 3 つ先）は持っていく。定型文キャッシュ、計測、音量を下げる経路、最初の区切りを短くする調整を足す |
| src/ai/playback.rs | 355 | cpal, audio | 一部作り直す | 中。途切れない再生の queue/marks は持っていく。音量（滑らかに変える）、エコー除去用の参照信号の取り出し口、最初の音を出した時刻の記録を足す |
| src/ai/audio.rs | 369 | cpal | 一部作り直す | 低〜中。WAV の作成・読み取り・長さの関数は持っていく。`Recorder`（押している間だけ録音、callback 内で Mutex を使う）は常時録音と環状バッファに作り直す。単純平均のダウンサンプラーも置き換える |
| src/ai/claude.rs | 388 | stream, agents, usage | 持っていく | 高。常駐する stream-json セッション、`--resume`、interrupt |
| src/ai/stream.rs | 199 | serde | 持っていく | 高。そのまま |
| src/ai/agents.rs | 597 | stream, util | 持っていく | 高。サブエージェントの追跡はマスコットと会話欄の 1 行表示に使う（AGENTS ビューは捨てる） |
| src/ai/usage.rs | 440 | stream, util | 持っていく | 高。そのまま |
| src/ai/mcp.rs | 240 | tiny_http | 持っていく | 高。サーバー名を `va` に変える |
| src/ai/policy.rs | 203 | regex, util | 持っていく＋手直し | 高。危険パターン、settings.json の deny 再適用、`parse_consent` は持っていく。ツール名の変更、git 履歴改変を「確認」ではなく「禁止」にする固定ルール、ブラウザ操作の判定を足す |
| src/ai/mod.rs | 1307 | 全部 | 作り直す | 低〜中。Controller は状態機械として作り直す。approve、ask_approval、describe_tool、emit、Event の型、Claude の起動引数とサブエージェント定義（67〜80、347〜428 行）は流用する。全体の 4 割ほど |
| src/ai/panes.rs | 241 | proto | 作り直す | 低。Ghostty ペインのモデルを、裏のシェルの一覧に置き換える |
| src/ai/world.rs | 259 | weather | 捨てる | 地震と地球儀 |
| src/relay/bridge.rs | 534 | regex, proto | 持っていく | 高。OSC 633 でコマンドを打ち込み、終了コードを受け取る部分。画面無しでもそのまま使える |
| src/relay/mod.rs | 604 | nix, vt100 | 一部持っていく | 中。pty の起動と vt100 の画面モデルは持っていく。stdout への素通し、プレフィックスキー、talk mode は捨てる |
| src/relay/zsh/ | 93 | — | 持っていく | 高。Ghostty 連携を読み込む部分は外す |
| src/relay/shell.rs | 190 | proto | 捨てる | Ghostty の split で動くシェル |
| src/ui/canvas.rs | 573 | ratatui | 持っていく | 高 |
| src/ui/mascot.rs | 578 | canvas | 持っていく | 高。状態の割り当てに「判定中」「マイクオフ」を足す |
| src/ui/usage.rs | 278 | canvas, view::heat | 持っていく | 中〜高 |
| src/ui/view.rs / ui/mod.rs | 1079 / 952 | 多数 | 作り直す | 低。AI パネルの会話描画、折り返し、スクロールだけ流用（200〜300 行ほど） |
| src/ui/input.rs | 241 | — | 任意 | 中。文字入力（`t`）を残すなら |
| src/ui/agents.rs / globe.rs | 232 / 570 | — | 捨てる | |
| src/hud.rs / proto.rs | 194 / 350 | — | 一部持っていく | 中。kitty キーの解釈（Space 長押し）と、フレームの出力 |
| src/ghostty.rs | 225 | osascript | 持っていく | 高。split、new window、focus、close。ブラウザとシェル表示の split に使う |
| src/sysmon.rs | 272 | sysinfo | 作り直す | 低。CPU だけ残す（50 行ほど） |
| src/config.rs / util.rs / net.rs | 137 / 124 / 118 | serde, ureq | 持っていく | 中 / 高 / 高 |
| src/core.rs / main.rs | 1017 / 26 | — | 作り直す | |
| src/weather/*、web/globe | 約 1194 + web | — | 捨てる | |

- 持っていく量はおよそ 6〜7 千行です。
- Go 版（~/edex-tui）は参考にとどめます。Rust 版に無いものは見当たりませんでした（細かくは確認していません）。
- 【事実】`--tools` に TaskStop と SendMessage を足した変更（policy.rs の AUTO_ALLOW も）は、edex-rs では**まだコミットされていません**。新プロジェクトには必ず入れます。

### 1.5 UI の作り方の比較

| 観点 | A. Ghostty 1 ウィンドウを使う TUI | B. 小さなネイティブウィンドウ（egui など） | C. Web ビュー（wry/WKWebView） |
|---|---|---|---|
| 流用 | canvas、マスコット、使用量表示がそのまま使える | マスコットの画素データだけ | ほぼ書き直し |
| メモリ（8GB 機）【推測】 | 数 MB | 50〜100MB | 100〜200MB |
| マイクの許可 | Ghostty の許可をそのまま使う（取得済み） | 自前の .app、Info.plist、署名が要る | B と同じ |
| terminal-browser | 同じウィンドウの隣に split（既存の ghostty.rs で済む） | 別に Ghostty ウィンドウを開く | 同じく別 |
| 表現力 | 半ブロックのドット絵まで | 滑らかなアニメ | いちばん高い（将来のアバター向き） |
| 日本語の表示 | Ghostty に任せる | CJK フォントの読み込みが要る | そのまま |
| 新しい依存 | なし | eframe 一式（要承認） | wry など（要承認） |

**推奨は A です。** Ghostty の専用ウィンドウ 1 枚で動く TUI にします。

- 理由: マイクの許可、ブラウザの split、手持ちコードの流用、メモリのすべてで有利です。キー操作が最小限で常時待ち受けなので、フォーカスもほとんど要りません。
- 描画は `View` トレイトの裏に置き、`UiState` のスナップショットだけを受け取る形にします。将来 Grok のようなアバターが欲しくなったら、C の Web 表示を後から足せます。

---

## 2. プロジェクト構成

- **置き場所と名前**: `~/voice-agent`。実行ファイルは `voice-agent`。Rust 2024 です。
- **クレート**: edex-rs と同じく単一クレートにして、モジュールで分けることを推奨します。sherpa-onnx はビルド済みの静的ライブラリをリンクするだけなので、別クレートに分ける利点は小さいです。ビルドが遅くなったら workspace に分けます。

```
src/
  main.rs            サブコマンドの振り分け
  app.rs             状態機械（待機/聞き取り/判定/文字起こし/考え中/実行/読み上げ/承認待ち/マイクオフ）
  config.rs  util.rs  net.rs  log.rs（計測ログ）
  audio/  capture.rs（cpal → SPSC → 16kHz）  ring.rs（約 35 秒）  resample.rs
          playback.rs（途切れない再生、音量を下げる、参照信号の取り出し）  wav.rs  aec.rs（トレイト。初めは素通し）
  voice/  vad.rs（Silero）  endpoint.rs（550/1000ms、30 秒の状態機械）  speaker.rs（埋め込み、点数、しきい値）
          enroll.rs  calibrate.rs  rescue.rs（保留 1 件、30 秒）  commands.rs（「止めて」などの声のコマンド）
  speech/ stt.rs  aivis.rs  arbiter.rs（優先度付きでエンジンの使用権を配る）  tts.rs  phrases.rs（定型文キャッシュ）
  agent/  claude.rs  stream.rs  agents.rs  usage.rs  mcp.rs  policy.rs  tools.rs  prompt.rs（人格を差し込む口）
  shell/  pty.rs  bridge.rs  zsh/  registry.rs  jobs.rs
  browser/ terminal.rs（terminal-browser）  chrome.rs（CDP）
  ghostty.rs
  ui/     canvas.rs  mascot.rs  usage.rs  view.rs  keys.rs
```

**サブコマンド**

| コマンド | 内容 |
|---|---|
| `voice-agent` | 起動 |
| `launch` | Ghostty の新しいウィンドウで自分を起動する |
| `enroll` | 声紋の登録 |
| `calibrate [--model campplus\|wespeaker]` | しきい値を出し直す。EER も表示 |
| `score <wav>…` | 点数を確かめる |
| `view <shell>` | 裏のシェルを Ghostty の split に映す |
| `doctor` | 必要なものと許可の点検 |

**ファイルの場所**

| 種類 | 場所 |
|---|---|
| 設定 | `~/.config/voice-agent/config.json`（既定値は edex と同じ: まお 888753760、速度 1.3、Kyoko 260、large-v3-turbo、ja） |
| 人格（今は空） | `~/.config/voice-agent/persona.md` |
| 声紋 | `~/.config/voice-agent/voiceprint/`（ディレクトリ 0700、中身 0600）。`profile.json`（モデル名と sha256、重心、区間ごとの埋め込み、しきい値、分布）と、登録音声 `enroll/NN-kind.wav`。モデルの sha が変わったら WAV から自動で計算し直す |
| モデル | `~/.local/share/voice-agent/models/` |
| ログ（案） | `~/Library/Logs/voice-agent/voice-agent.log` と `latency.jsonl` |
| キャッシュ | `~/Library/Caches/voice-agent/`（phrases/、shells/*.log、jobs/*.log） |

**ビルドと配布**

- `cargo build --release`、または `cargo install --path .` で入れます。
- 起動は `voice-agent launch` です。AppleScript の `new window with configuration {command:"voice-agent"}` で開きます。
- sherpa-onnx は静的リンクです。【事実】ビルドスクリプトが prebuilt アーカイブをダウンロードします。`SHERPA_ONNX_ARCHIVE_DIR` に取得済みのアーカイブを置き、ハッシュを固定することを推奨します。

---

## 3. 声の処理の組み直し（決定済みの設計を新しい構成に当てはめる）

**流れ**: cpal callback → SPSC → 聞き取りスレッド（16kHz に変換 → エコー除去（任意）→ VAD → 発話の区切り → 話者判定）→ app に知らせる。
- ロックを使わない SPSC には std の channel を使い、新しい依存は足しません。
- 変換には sherpa の LinearResampler を使えます【事実: 機能一覧にある】。

**発話の区切り（endpoint.rs）**

| 段階 | 動き |
|---|---|
| 声を検出 | 0.3 秒さかのぼって取り込む |
| 有声 1 秒 | 早期の話者判定。明らかな他人ならここで捨て、回数を 1 増やす |
| 無音 550ms | 全体の話者判定。通ったら Whisper を先行実行し、温めを 1 回だけ許す |
| 1000ms までに声が戻る | 先行結果は古いとして捨てる（whisper-server は途中で止められないので、同時に走らせるのは 1 本まで） |
| 無音 1000ms | 確定 |
| 30 秒 | 強制確定 |

- 【推測/要確認】sherpa の `VoiceActivityDetector` からフレームごとの判定が取れること。取れれば、時間の管理は自前の状態機械で行います。

**話者判定（speaker.rs）**

点数の段階は、捨てる / 境界 / 通す / 短い発話用（厳しい）/ 承認用（いちばん高い）の 5 つです。
- 境界の発話は音声をメモリに 1 件だけ 30 秒置きます（文字起こしはしない）。「今のは自分だよ」と言われたら、そのとき文字起こしして送ります。
- 1 秒未満の発話は、承認待ちの間か、AI が話し終えて 8 秒以内だけ、厳しいしきい値で通します。
- 声での承認は、承認用しきい値を超え、かつ `parse_consent` が「はい」か「いいえ」を返したときだけ受けます。
- 【事実】今の edex は、承認待ちに「はい/いいえ」以外の言葉が来ると**拒否として扱います**（mod.rs 562〜566 行）。新しい仕様では「無視」に変えます。

**登録（約 60 秒）**: 通常文 6、短い語 10（はい、いいえ、止めて、マイクを切って、今のは自分だよ など）、小声 3、少し離れて 3。
- 録音のたびに SN 比、音割れ、有声の割合を確かめ、だめなら録り直します。
- 案内は画面の文字だけにします。読み上げると、その声がマイクに入るためです。

**しきい値の決め方（calibrate.rs）**
- 本人側: 登録した区間ごとに、ほかの区間の重心と比べた点数（1 つずつ外して計算）。
- 他人側: まお と Kyoko の合成音を各 15 文ほど。デジタルのまま使うものと、任意で 20 秒ほどスピーカーから流してマイクで録るものの 2 通り。
- 分布が重なったら、警告して wespeaker と比べます。
- 【推測】CAM++ の zh_en モデルを日本語に使うと精度が落ちる可能性があり、比べる理由になります。

**声での割り込み**
1. 読み上げ中に声が始まったら、30ms 以内に約 −12dB まで下げます。
2. 約 1 秒分の有声で判定します。本人なら読み上げを止め、違えば 200ms かけて元の音量に戻します。
3. 第一段の対策は簡単なエコー判定です。登録時に「再生音に対するマイク音の比」を測っておき、マイクの音量が想定される回り込みの量以下なら声の開始とみなしません。

**エコー除去の候補（後の段階）**

| 候補 | 導入の重さ | 内容 |
|---|---|---|
| sonora / sonora-aec3 / aec3 | 軽い（純 Rust） | WebRTC AEC3 の移植。参照信号は自前の再生から取れる。成熟度は不明 |
| VoiceProcessingIO（coreaudio-rs など） | 中 | OS がエコー除去を行う。入出力とも cpal から置き換える必要がある。ほかのアプリの音も下げてしまう問題、macOS 12 での不具合報告がある。登録も同じ経路で録り直す必要がある |
| webrtc-audio-processing 2.x bundled | 重い | meson、ninja、clang が要る。2.0.3 で bundled ビルドが壊れるという報告がある |
| speexdsp | 軽い | 純 Rust 版が既定。2022 年から更新が無く、品質は低めとされる |

推奨は、まず割り込み中の点数の低下を測り、下がっていれば sonora、次に VPIO を feature で切り替えて試すことです。

---

## 4. 返答の高速化

1. **計測ログ**: 1 ターンごとに `latency.jsonl` へ 1 行書きます。
   - 項目: 声の終わり、Whisper の開始と終了（先行か確定か）、Claude への送信、最初の文字、最初の区切り、合成の開始と終了、最初の音（再生 callback で記録）。
2. **温めの修正**: arbiter.rs で、使用権に優先度を付けます（本番の合成 > キャッシュの作成 > 温め）。
   - 温めるのは「本人と判定された後」に 1 発話 1 回だけです。Whisper の実行中とサブエージェントの実行中は温めません。
   - 温めの文は短くします（例「はい。」）。【事実】AivisSpeech は合成を途中で止められないので、本番を待たせる時間を短く抑えるためです。
3. **定型文のキャッシュ**: 起動後の空き時間に低い優先度で合成し、PCM を保存します。区切り処理を通さずに直接再生します。
   - 【事実】コード上の固定文は、危険理由 15 + 作業ディレクトリ外 1 + 「エラーが発生しました。」で 17 種です。ご指摘の 17 文と数が合います。
   - 確認の読み上げは「確認です。」「{理由}。」「実行していいですか？」の 3 つに分けてキャッシュします。声のコマンドへの返事（「止めました」など）も加えます。
   - キーは（声の ID、速度、サンプルレート、文、エンジンのバージョン）です。
4. **Whisper の先行実行**: 3 章の 550ms の段階です。
5. **最初の区切りを短くする**: `FIRST_MIN` は今 14 文字です。8〜10 文字にして、計測しながら調整します。

---

## 5. 裏のシェルとブラウザ

**裏のシェル**: pty + zsh + bridge（OSC 633）+ vt100 で動かし、画面には出しません。

- 名前を付けて複数持てます。既定は `main` で、作業ディレクトリは設定の `workdir` か起動したディレクトリです。cd した結果は残ります。
- 出力は全部 `shells/<名前>.log` に書きます。ツールの返り値は最後の 400 行・30KB までに切り、ログのパスを添えます。
- **ツール**

| ツール | 内容 |
|---|---|
| `shell_run` | 既定 120 秒、最大 600 秒 |
| `shell_screen` / `shell_list` / `shell_new` | 画面の読み取り / 一覧 / 新しいシェル |
| `shell_input` | 入力待ちに文字を送る。毎回確認 |
| `job_start` / `job_status` / `job_tail` / `job_stop` | 長時間の処理用。pty を通さず子プロセスとして動かし、ログはファイルへ |
| `show_shell` | Ghostty の split で `voice-agent view <shell>` を開き、vt100 の画面を 100ms ごとに描き直して見せる |

- **承認**: `shell_run`、`job_start`、`shell_input` はすべて policy の判定を通します。git の履歴改変（`reset --hard`、`branch -D`、force push、rebase、`commit --amend`）は固定で禁止にし、settings.json の deny が無くても止まるようにします。
- **中断**: Esc か「止めて」で、全シェルに Ctrl-C を送り、Claude を interrupt します。サブエージェントの停止は、Claude に TaskStop を使わせます。
- Claude Code の作業ディレクトリは起動時に決まります。外を編集するときは確認が出ます。`--add-dir` を使って再起動するかは未決です。

**ブラウザ**
- **見せるとき（localhost、HTML のレポートなど）**: `ghostty::split(自分のウィンドウ, "right", "terminal-browser open URL")` で隣に開きます。操作は `terminal-browser ls --all --json` で key を取り、`action --browser KEY -- …` で行います。
  - 【推測/要検証】TTY の無い裏シェルから `action` が使えること。
  - 閉じるときは `shutdown` でメモリを空けます。
- **重いサイトやログインが要るとき**: Chrome を `--user-data-dir=~/Library/Application Support/voice-agent/chrome --remote-debugging-port=<空きポート>` で起動し、同梱の agent-browser を `--cdp` 付きで使います。
  - 【推測】Chrome は普段のプロファイルでのリモートデバッグを制限しているので、専用プロファイルにします（ログイン状態は使えません）。
- **ブラウザ操作の承認（案）**: snapshot と eval（読み取り）は許可。localhost 以外での click、fill、type は確認。

---

## 6. 画面の設計（TUI 1 ウィンドウ）

```
┌ VOICE-AGENT ──────────────────────────────── CPU ▂▃▅▂ 23% ┐
│ あなた  テストを走らせて                         │  マスコット │
│ AI      テストを実行します。…                    │  (20 桁)   │
│   [exec] cargo test   ✓ opus-worker 調査 · 2m    │            │
├────────────────────────────────────────────────────────────┤
│ ● 聞き取り中 ▮▮▮▯▯  他の声 ×3  保留 1(23s) │ $0.42 · 5H 73% · 7D 24% · CTX 23% │
│ [確認] git push origin main —「はい/いいえ」か y/n            │  ← 承認待ちのときだけ
└────────────────────────────────────────────────────────────┘
```

- 状態の表示: 待機 / 聞き取り中 / 判定中 / 考え中 / 実行中 / 読み上げ中 / 承認待ち / マイクオフ / 声紋なし（長押し）。
- キー: Esc（全部止める）、y/n（承認）、m（マイク）、t（文字入力）、q（終了）。Space 長押しは声紋が無いときだけ使います。

---

## 7. 段階の分け方

| 段階 | 内容 | 完了条件 |
|---|---|---|
| P0 | 移植の土台。Claude、ポリシー、MCP、裏のシェル、TTS、TUI、長押し（声紋なしのとき） | 文字で頼むと裏のシェルで実行し、読み上げる。`cargo fmt/clippy/test` が通る |
| P1 | 計測ログ、温めの修正、定型文キャッシュ、最初の区切り | 全項目が latency.jsonl に出る。偽エンジンを使ったテストで、温めが本番を追い越さない。キャッシュした文は 100ms 以内に鳴る |
| P2 | 常時待ち受け（環状バッファ、VAD、区切り、Whisper の先行実行） | WAV 入力で 550/1000ms/30 秒の動きを確認。先行実行の分だけ短くなったことがログに出る |
| P3 | 話者認証（enroll、calibrate、早期に捨てる、救済、短い発話、声での承認） | 本人/他人/AI の WAV で、通す・捨てるが期待どおり。声紋が無ければ長押しに戻る |
| P4 | 声での割り込みと声のコマンド | 擬似エコーを混ぜた状態で、AI の声では止まらず、本人の声なら約 1 秒で止まる |
| P5 | ブラウザ（terminal-browser の split、Chrome + CDP） | 開く・読む・閉じるが声の依頼で通る |
| P6 | エコー除去の評価と導入 | 割り込み中の点数の低下が許せる範囲に入る |
| P7 | 人格を差し込む口、`launch`、`doctor` | |

**テストの入口**
- `VA_TEST_STEPS='[{"at":3000,"text":"…"},{"at":9000,"wav":"/x.wav","as":"mic"}]'`
  - `as:"mic"` は VAD と話者判定を通します。`direct` は文字起こしだけです。
- `VA_MIC_WAV`（WAV をマイクの代わりに実時間で流す）
- `VA_LOOPBACK=1`（読み上げ音を遅らせ、音量を変えてマイクに混ぜる）
- `VA_TTS_OUT`、`VA_EVENT_LOG`、`VA_RUNTIME_DIR`、`VA_LATENCY_LOG`

**テストの分け方**
- 区切りと話者判定のロジック: 判定値の列を作って与えます。モデル不要で、決まった結果になります。
- モデルを使う試験: `VA_LIVE_MODELS=1` のときだけ動かします。
- 本人の WAV は登録音声を使い、リポジトリには入れません。
- AI の声の WAV は、AivisSpeech と `say -o` でローカルに作ります。
- 他人の声の素材は、ユーザーに決めてもらいます（9 章）。

---

## 8. リスク

- **メモリ（8GB）**: Whisper のモデルが 573MB【事実】。実行時の Whisper 全体、AivisSpeech、Claude Code、sherpa、ブラウザを合わせると、3〜4.5GB 程度と見ています【推測】。スワップに追い出されて AivisSpeech が遅くなる今の症状と合っています。
- **AI の声の回り込み**: 音量が上下にちらつく、誤って割り込む。対策はエコー判定、エコー除去、ヘッドホン。
- **AirPods のマイク**: 使うと Bluetooth の通話用プロファイルに切り替わり、再生の音質が落ちます【推測】。内蔵マイクを推奨します。
- **話者判定の限界**: 登録が少なく言語も違うため、点数の分布が重なるおそれがあります。セキュリティ機能ではありません（動画の本人の声、録音の再生、BGM、1 秒未満の発話に弱い）。
- **マイクが常に動く**: macOS の録音表示（オレンジの点）が常時点きます。音声はメモリにだけ置き、登録時以外は保存しません。
- **ビルド時のダウンロード**: sherpa-onnx のビルドがアーカイブを取りに行きます。
- **未確認の前提**: TTY の無いところから terminal-browser を操作できるか。Claude Code のツール名（TaskStop、SendMessage）が将来変わること。
- **VPIO**: ほかのアプリの音も下げてしまう。信号が変わるので声紋の録り直しが要る。

---

## 9. 要承認（実行していません）

- **承認済みのもの**: sherpa-onnx `=2.0.1` と、そのビルド時のアーカイブ取得。CAM++、wespeaker、Silero VAD のモデル。
- **未承認のもの**: エコー除去の crate（sonora、aec3、webrtc-audio-processing、speexdsp）、VPIO 用の crate（coreaudio-rs か objc2 系）、SenseVoice などの別の音声認識モデル、他人の声の公開コーパス、GitHub リポジトリの作成と push。
- 既存の crate がローカルのキャッシュに無い場合の取得【推測: たぶんある】。

---

## 要約

- `~/edex-rs` は Ghostty の split に ANSI フレームを送る TUI です。声と Claude まわり（stt、tts、aivis、claude、stream、agents、usage、mcp、policy、bridge、mascot、canvas）は 6〜7 千行が流用できます。
- 画面は Ghostty の専用ウィンドウ 1 枚の TUI を推奨します。描画は差し替えられる形にし、後から Web 表示も足せます。
- 作業は、pty + zsh + OSC 633 の裏のシェルと、ジョブのログで行います。見せるときは terminal-browser を split で開くか、専用プロファイルの Chrome を CDP で操作します。
- 声は、環状バッファ → Silero → 550ms で先行実行 / 1000ms で確定 → CAM++ で判定、の順に流します。割り込みは、音量を下げてから約 1 秒で判定します。エコー除去は測ってから sonora、次に VPIO を試します。
- 高速化は、計測 → 優先度付きの温め → 17 文のキャッシュ → 先行実行 → 最初の区切り、の順に P1〜P2 で入れます。
- 段階は P0〜P7 で、WAV をマイク代わりに流す入口と、擬似エコーを混ぜる入口でテストします。
- edex-rs の TaskStop/SendMessage の変更はまだコミットされていません。今の承認処理は、はっきりしない返事を「拒否」として扱うので、「無視」に変えます。

## 決めていただきたいこと

1. UI は Ghostty の TUI で良いか
2. 「マイクを切って」の意味: デバイスを止める（戻すのは m キー）か、ウェイク語だけを待つか
3. 割り込んだとき、読み上げだけ止めるか、返答の生成も止めるか
4. Whisper は large-v3-turbo のままか、軽いモデルや SenseVoice と比べるか（ダウンロードは要承認）
5. 他人の声のテスト素材: 家族の録音、macOS のほかの声、公開コーパス（要承認、ライセンスの確認が要る）
6. エコー除去を試す順番と、その依存追加の承認
7. Chrome は専用プロファイルで良いか（ログイン状態は使えない）。ブラウザ操作の承認の範囲
8. 終了時に裏のジョブを止めるか、残すか
9. ログの場所（`~/Library/Logs` か、edex と同じ `~/Library/Caches` か）
10. 救済した発話を声紋に追加して学習させるか。「他の声」の回数をどの単位でリセットするか
11. クレートの分け方（単一クレートを推奨）
12. Claude Code の作業ディレクトリの外で作業させるとき、`--add-dir` で再起動するか
13. GitHub リポジトリを作るか（外向きの操作なので事前確認）

出典:
- [sherpa-onnx (docs.rs)](https://docs.rs/sherpa-onnx)
- [sonora](https://docs.rs/crate/sonora/0.1.0) / [sonora-aec3](https://docs.rs/sonora-aec3) / [aec3](https://docs.rs/crate/aec3/0.4.0)
- [speexdsp (lib.rs)](https://lib.rs/crates/speexdsp)
- [wz-phone（webrtc-audio-processing のビルド報告、VPIO の Rust 実装例）](https://git.manko.yoga/manawenuz/wz-phone/commit/d1c96cd71f4e7e5175602f85d2630f7cfa85f3fd)
- [Apple Developer Forums: VoiceProcessingIO](https://developer.apple.com/forums/thread/66953) / [音量を下げてしまう問題](https://developer.apple.com/forums/thread/733733)
- [Mozilla gecko-dev（macOS 12 の VPIO の注意）](https://source-hub.etos-pro.ru/mozilla/gecko-dev/commit/35824cf6dd72327bddd6b6b8873e9e68add3d65a)
