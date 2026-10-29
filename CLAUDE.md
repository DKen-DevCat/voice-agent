# voice-agent

開発運用ルールはグローバル `~/.claude/CLAUDE.md` を正本とする。ここにはこのリポジトリ固有のことだけを書く。

- 声だけでやり取りする話し相手のエージェント。使い方・設定・構成・テストの入口は `README.md`、設計と段階（P0〜P7）は `docs/design.md`
- `~/edex-rs` から声と Claude まわりを切り出したもの。edex-rs と edex-tui は参照するだけで、このリポジトリの作業では変更しない
- 画面に出すのは会話、マスコット、CPU、Claude の使用量だけ。見えるシェル、ファイル一覧、キーボード表示、ワールドビュー、ネットワーク表示は足さない。描画は `ui::view::View` の裏に置き、`UiState` だけから描く
- コマンドは Claude Code の Bash ではなく、`src/agent/tools.rs` の裏のシェル（`mcp__va__*`）で動かす。実行するツールは、承認ツールを通ったかどうかに関係なく、実行の直前に `src/agent/policy.rs` の判定を通す。git 履歴の改変は確認ではなく固定で禁止
- 承認待ちに「はい / いいえ」と判定できない返事が来たら、拒否ではなく無視して待ち続ける
- `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` を通してからコミットする
- 画面と通しの確認は tmux の別ソケット（`tmux -L vatest`）で行う。`VA_RUNTIME_DIR` と `VA_CONFIG` を別の場所にして、動いている voice-agent と重ねない（同じ場所では 2 つ目が起動しない）
- 開発機は M1 / 8GB。確認では Whisper と AivisSpeech を起動しない設定（`"tts": "say"`、存在しない `whisperModel`）を使う
- 新しい依存の追加は事前に確認する（承認済みは P2/P3 の sherpa-onnx `=2.0.1` とそのモデルだけ）
