# voice-agent

声だけでやり取りする話し相手のエージェントです。Space を押しながら話すと、Claude Code が考えて声で返事をします。開発の作業も頼めます。コマンドは、画面に出ない裏のシェルで動きます。危ない操作は、実行する前に「はい / いいえ」で確認します。

画面に出すのは、会話、マスコット、CPU 使用率、Claude の使用量だけです。

```
┌─ VOICE-AGENT ─────────────────────────────────────── CPU ▂▃▅▂▁▃▄▅ 23% ──┐
│ あなた  テストを走らせて                                    (マスコット) │
│ AI      テストを実行します。                                             │
│         [exec] cargo test                                                │
│ AI      全部通りました。                                                 │
├──────────────────────────────────────────────────────────────────────────┤
│ ◇ 待機                       $0.42 · 5H 73% · 7D 24% · CTX 23% · 1.4M tok │
└─ Space長押し 話す  t 文字  y/n 承認  m マイク  Esc 止める  q 終了 ────────┘
```

## 必要なもの

- macOS（Apple Silicon）と Rust（edition 2024）
- [Claude Code](https://claude.com/claude-code)（`claude` コマンド）。ログイン済みのものをそのまま使います
- 音声認識: `whisper-server`（`brew install whisper-cpp`）と、モデル `~/.local/share/whisper/ggml-large-v3-turbo-q5_0.bin`
- 読み上げ: AivisSpeech.app（無ければ macOS の `say`）
- ターミナル: Ghostty を推奨します。キーを離したことを知らせてくれるターミナル（kitty keyboard protocol）なら、Space の長押しで話せます。そうでないターミナルでは、Space で話し始めて、もう一度 Space で送ります
- 使うターミナルに、マイクの使用を許可してください（システム設定 > プライバシーとセキュリティ > マイク）

## 使い方

```sh
cargo build --release
./target/release/voice-agent      # 専用の Ghostty ウィンドウで動かすのがおすすめ
```

作業を頼む場所で起動するか、設定の `dir` で場所を決めます。Claude と裏のシェルは、そこで始まります。

| キー | 動き |
|---|---|
| Space 長押し | 話す（離すと送信、Esc で取り消し） |
| t / Enter | 文字で送る（Enter 送信、Shift+Enter 改行、Esc 取り消し、↑↓ 履歴） |
| y / n | 承認待ちに答える |
| m | マイクのオン・オフ（オフのときはデバイスごと止めます） |
| Esc / Ctrl-C | 全部止める（読み上げ、承認待ち、シェルのコマンド、Claude の返答） |
| ↑ ↓ PgUp PgDn End | 会話をさかのぼる |
| q | 終了（Claude Code、裏のシェル、ジョブも止めます） |

声のコマンドは、言ったことの全体が次のどれかのときだけ働きます。「サーバーを止めて」のような頼みごとは、そのまま Claude に届きます。

| 言うこと | 動き |
|---|---|
| 「止めて」「ストップ」 | 読み上げを止め、シェルで動いているコマンドに Ctrl-C を送る。Claude の返答は続き、画面には出るが読み上げない |
| 「生成もストップ」「全部止めて」 | 上に加えて、Claude の返答も止める（Esc と同じ） |
| 「マイクを切って」 | マイクを止める（戻すのは m キー） |

### 承認

Claude は Bash を使わず、voice-agent の裏のシェルのツール（`mcp__va__shell_run` など）でコマンドを動かします。コマンドは、実行の直前に次の順で判定します。

1. git の履歴の書き換え（`reset --hard`、`branch -D`、force push、`rebase`、`commit --amend`、`filter-branch`）は、確認なしで必ず断ります
2. `~/.claude/settings.json` の `permissions.deny` にある `Bash(...)` のルールに合うものは断ります
3. PR の作成、ブランチの削除、push、外部サービスへの反映、ネットワークからの依存の追加、ファイルの再帰的な削除、`sudo` などは、声か y / n で確認します。当てはまった理由は全部読み上げます
4. それ以外は、そのまま動かします

確認への声の返事は、言ったことの全体が「はい」「お願いします」「進めてください」のような返事のときだけ許可になります。「許可しません」「お願い、やめて」のように否定の言葉が入っていれば拒否です。どちらでもない返事（「ええと」「はい、次は README も」など）は無視して、確認を待ち続けます。確認が出る前に話し始めた発話は、答えにしません。確認が重なったときは、古いものから 1 つずつ表示し、読み上げ、答えを受けます。

承認待ちの間に「止めて」と言うと拒否になります。「静かにして」は読み上げを止めるだけで、確認は待ち続けます。

この判定は正規表現による安全柵です。サンドボックスではありません。一覧に無いコマンドはそのまま動きます。言い換え（スクリプトを書いて実行するなど）で避けることもできます。Edit と Write は、作業ディレクトリの外でも、`~/.claude/settings.json` の allow に Edit や Write があれば、Claude Code が先に許可します。

## 設定

`~/.config/voice-agent/config.json` に書きます。どのキーも省略できます。

| キー | 既定値 | 内容 |
|---|---|---|
| `shell` | `/bin/zsh` | 裏のシェル（zsh のみ） |
| `dir` | 起動したディレクトリ | Claude と裏のシェルが始まる場所 |
| `model` | `claude-sonnet-5-5` | 声の窓口になる Claude のモデル。重い作業は opus-worker と fable-worker に任せます |
| `effort` | Claude Code の既定 | `claude --effort` に渡す値 |
| `tts` | `aivis` | `aivis`（エンジンが無ければ `say`）か `say` |
| `aivisURL` | `http://127.0.0.1:10101` | AivisSpeech Engine の場所。答えが無ければ AivisSpeech.app のエンジンを起動します |
| `aivisSpeaker` | `888753760` | 声のスタイル ID（まお） |
| `aivisSpeed` | `1.3` | 話す速さ |
| `voice` | `Kyoko` | `say` の声 |
| `voiceRate` | `260` | `say` の速さ |
| `speak` | `true` | 読み上げるか |
| `whisperModel` | `~/.local/share/whisper/ggml-large-v3-turbo-q5_0.bin` | Whisper のモデル |
| `language` | `ja` | 音声認識の言語 |
| `holdMs` | `300` | Space をこの時間（ミリ秒）押し続けると録音が始まる |
| `mascot` | `true` | マスコットを出すか |

人格は、設定と同じ場所の `persona.md` に書きます。中身はシステムプロンプトの最後に足されます。今は空で構いません。

## ファイル

`~/Library/Caches/voice-agent/`（`VA_RUNTIME_DIR` で変えられます）に置きます。

- `voice-agent.log`: 動作のログ
- `shells/<名前>.log`: 裏のシェルの出力の全部。ツールが返すのは最後の 400 行・30KB までで、このファイルの場所を添えます
- `jobs/<番号>.log`: 長い処理（`job_start`）の出力
- `zsh/`: 裏のシェルに読ませる zsh の設定（OSC 633 でコマンドの終わりと終了コードを知らせます）
- `voice-agent.lock`: 同じ場所で 2 つ起動しないための印
- `mcp.json`: Claude Code に渡す MCP の設定（接続用のトークン入り。本人だけが読める 0600 で、終了時に消します）

## 構成

```
src/
  main.rs       起動、端末の入出力、描画のループ
  app.rs        状態機械（待機/聞き取り中/文字起こし中/考え中/実行中/読み上げ中/承認待ち/マイクオフ）
  config.rs  log.rs  net.rs  util.rs  sysmon.rs（CPU）
  agent/   claude.rs（常駐する stream-json の claude）  stream.rs  agents.rs（サブエージェントの追跡）
           usage.rs（使用量）  mcp.rs（MCP サーバー va）  tools.rs（シェルとジョブのツール）
           policy.rs（承認の判定）  prompt.rs（システムプロンプトと人格の差し込み口）
  shell/   pty.rs  bridge.rs（OSC 633）  zsh/  registry.rs（名前付きの裏のシェル）  jobs.rs
  speech/  stt.rs（whisper-server）  aivis.rs  tts.rs（文の区切りと読み上げ）  arbiter.rs
  audio/   capture.rs（マイク）  playback.rs（途切れない再生）  wav.rs
  voice/   commands.rs（声のコマンド）
  ui/      mod.rs（画面の状態とキー）  view.rs（View トレイトと描画）  canvas.rs  mascot.rs
           usage.rs  input.rs  keys.rs  term.rs
```

設計と段階（P0〜P7）は [docs/design.md](docs/design.md) にあります。今あるのは P0（土台）です。常時待ち受け（P2）、話者認証（P3）、声での割り込み（P4）はこれからです。

## テスト

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

本物のマイクを使うテストは `VA_LIVE_AUDIO=1 cargo test live_ -- --nocapture` で動きます。

通しの確認には、次の環境変数を使います。

| 変数 | 内容 |
|---|---|
| `VA_CONFIG` | 設定ファイルの場所（`persona.md` もその隣を見ます） |
| `VA_RUNTIME_DIR` | ログとシェルの場所。動いている voice-agent と分けるときに使います |
| `VA_TEST_STEPS` | 決まった時刻に文字を送るか、WAV を文字起こしに回す: `[{"at":3000,"text":"…"},{"at":9000,"wav":"/x.wav"}]` |
| `VA_TTS_OUT` | 読み上げを、スピーカーではなくこのディレクトリの音声ファイルにする（隣に読んだ文の `.txt`） |
| `VA_EVENT_LOG` | 出来事を 1 行 1 件の JSON で追記する |
| `VA_FORCE_KITTY` | `1` にすると、キーを離したことを知らせるターミナルとして扱う（tmux で Space の長押しを試すとき） |

tmux の別ソケットで動かすと、画面とキー操作を確かめられます。

```sh
tmux -L vatest new-session -d -s va -x 120 -y 30 \
  "env VA_RUNTIME_DIR=/tmp/va-rt VA_CONFIG=/tmp/va/config.json VA_EVENT_LOG=/tmp/va/events.jsonl VA_TTS_OUT=/tmp/va/tts ./target/release/voice-agent"
tmux -L vatest send-keys -t va t; tmux -L vatest send-keys -t va -l "裏のシェルで echo ok を実行して"; tmux -L vatest send-keys -t va Enter
tmux -L vatest capture-pane -p -t va
# Space の長押し（VA_FORCE_KITTY=1 のとき）: 押す / 離す
tmux -L vatest send-keys -t va -H 1b 5b 33 32 75
tmux -L vatest send-keys -t va -H 1b 5b 33 32 3b 31 3a 33 75
tmux -L vatest send-keys -t va q; tmux -L vatest kill-server
```

確認用の設定を `{"tts": "say", "whisperModel": "/nonexistent"}` にすると、重い Whisper と AivisSpeech を起動せずに済みます。
