# notetui

ターミナル（TUI）で動く Misskey クライアント。コアロジックは
[`notecli`](https://github.com/notedeck-dev/notecli) ライブラリに委譲し、本リポジトリは
TUI フロントエンドのみを持つ。NoteDeck（GUI）と同列の「notecli を消費する別フロント
エンド」という位置づけで、アカウント DB を共有する。

## 原則

- **責務分離**: Misskey API・DB・認証・モデル・ストリーミングはすべて `notecli` 側。
  notetui は端末状態と描画・入力だけを所有する。
- **共有**: アカウント DB は notecli / NoteDeck と共有（`~/.local/share/notecli/notecli.db`
  + OS keychain）。どれでログインしても同じアカウントを使える。
- **再現性**: `notecli` は git rev で pin（NoteDeck と同方針）。隣に notecli のソースが
  無くてもビルドできる。

## 目標

最終的に投稿・リプライ・リアクション・リノート・フォロー等まで扱える
**フルクライアント**を目指す。リアルタイム更新はストリーミング（WebSocket）で行い、
UI は単一カラム + ポップアップ構成。アーキテクチャの詳細は [`DESIGN.md`](./DESIGN.md) を参照。

現状は最小実装（タイムライン閲覧・切替・スクロール）。ロードマップは DESIGN.md の §12。

## インストール / ビルド

`notecli` を git から取得するため、追加のローカル準備は不要。

```sh
cargo run            # TUI を起動（未ログインならその場でログイン誘導）
```

## ログイン

アプリ単体でログインが完結する（別途 notecli を実行する必要はない）。

```sh
notetui login misskey.io   # 明示的にアカウントを追加
```

未ログインの状態で `notetui`（`cargo run`）すると、ホスト入力 → MiAuth 認証 URL の表示
→ ブラウザ承認 → Enter、の順で初回ログインに誘導される。トークンは OS keychain に
保存され、notecli / NoteDeck と共有される。

## キーバインド

| キー | 動作 |
|------|------|
| `q` / `Esc` | 終了 |
| `j` / `k`（↓/↑） | カーソル移動 |
| `g` / `G` | 先頭 / 末尾 |
| `r` | 再取得 |
| `Tab` / `h` / `l` | タイムライン切替（Home / Social / Local / Global） |

> 投稿・リアクション等の書き込み操作と、より多くのキーバインドは順次追加予定（DESIGN.md §6/§7）。

## ライセンス

MIT
