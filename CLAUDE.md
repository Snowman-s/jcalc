# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 概要

JDWP (Java Debug Wire Protocol) 経由で JVM に四則演算を委譲するジョーク電卓。計算はすべて対象 JVM 上の `java.math.BigInteger` のメソッドを JDWP の `InvokeMethod` / `NewInstance` で呼び出して行う。

## リポジトリ構成

Cargo workspace (`jcalc`, `ore-jdwp`)。

- `jcalc/` — 電卓本体 (バイナリ)
- `ore-jdwp/` — 自作 JDWP 実装ライブラリ。**git submodule** (https://github.com/Snowman-s/ore-jdwp)。ここへの変更は submodule 側でのコミットが必要
- `java/Main.java` — 接続先 JVM 用の空の Main クラス

インデントは 2 スペース (`rustfmt.toml`)。

## コマンド

```sh
# ビルド / テスト / フォーマット (ルートから workspace 全体)
cargo build
cargo test
cargo fmt

# 単一テスト
cargo test -p jcalc parse::tests::test_parse_expression
cargo test -p ore_jdwp <テスト名>
```

実行には JDWP 待受状態の JVM が必要:

```sh
cd java && javac Main.java
java -agentlib:jdwp=transport=dt_socket,server=y,suspend=y,address=5005 Main
# 別ターミナルで
cargo run -p jcalc -- -v               # 対話モード (exit で終了)
cargo run -p jcalc -- -e "(1+2)*3"     # 単発計算
```

CLI オプション: `-H` ホスト (127.0.0.1), `-p` ポート (5005), `-v` 詳細ログ (stderr), `-e` 式。接続先 JVM のメインクラスは何でもよい。`-e` と stdin 非 TTY のモードでは 1 式だけ計算して終了し、失敗時は終了コード 1。stdin が TTY なら対話モード (`exit` か EOF で終了)。JVM は `suspend=y` で起動し、計算ごとに起動し直す必要はないが、jcalc を起動するたびに JVM の再起動が必要。

## アーキテクチャ

### jcalc (`jcalc/src/main.rs`)

1. TCP 接続して `JDWP-Handshake` を交換後、ストリームを read/write に分割し、受信タスク (`handle_receive`) と送信タスク (`handle_send`) を tokio で並行実行する。受信タスクはデコードしたパケットを `Result` として mpsc で送信側に流す (エラー応答は `Err`、デコードできないイベントは破棄)。
2. 受信パケットのデコードには「どのコマンドへの応答か」が必要なため、送信済みペイロードを `payloads: Arc<Mutex<Vec<...>>>` に保存し、パケット ID (= `cmd_id` = Vec のインデックス) で引く。ID サイズ (`JDWPContext`、受信タスクのローカル) は受信タスクが `VirtualMachineIDSizes` 応答をデコードした時点で設定する。それ以前にデコードできなかったイベント (ハンドシェイク直後の VMStart) は生バイトで保持し、設定後にデコードし直して応答の後に流す。
3. `SendHandler::send_and_receive` は 1 リクエスト送って次の応答を待つ同期的な往復。途中に来た `EventComposite` は `pending_events` に積み (VMDeath ならエラー)、`wait_for_vm_start` / `wait_for_class_prepare` が後から拾う。
4. 初期化: 自動で届く `VMStart` から初期スレッド (main を実行するスレッド) を得る (suspend policy が NONE ならエラー)。VMStart で止まったスレッドでは InvokeMethod が `INVALID_THREAD` になるため、そのスレッドに限定した `ClassPrepare` (ThreadOnly + Count 1) を設定して VM を resume し、そのイベントで停止したスレッドを計算用スレッドとして使う。初期スレッドでは必ずメインクラスが prepare されるので、メインクラス名に依存しない。例外表示用に `Throwable.toString` を先に解決し、`Class.forName` で BigInteger をロードさせてから、`ReferenceType.Methods` 1 回で `<init>(String)`, `add`, `subtract`, `multiply`, `divide`, `negate`, `toString` の method ID を得る。
5. `calc_expression`: `parse.rs` が式を **逆ポーランド記法 (Vec<Expression>)** に変換し (単項マイナス対応、ネストは `MAX_DEPTH` = 256 まで)、スタックマシンで評価。数値は 10 進文字列のまま `CreateString` + `ClassType.NewInstance` で `new BigInteger(String)`、演算は `ObjectReference.InvokeMethod` で BigInteger のメソッドを直接呼ぶ。InvokeMethod 中は全スレッドが動き GC されうるため、生成した BigInteger は `DisableCollection` し、計算後に `EnableCollection` する。結果は `toString()` → `StringReference.Value` で取得。JVM 側の例外は `Throwable.toString()` で文字列化して `Err` にする。

### ore-jdwp

- `src/defs.rs` と `src/packets.rs` 内の `jdwp_command_map!(...)` 呼び出し部分は **自動生成**。`tool/read_spec.py` (要 BeautifulSoup) が `tool/jdwp-protocol.html` (JDWP 仕様 HTML、リポジトリには含まれない) から `tool/defs.rs` と `tool/jdwp_command_map.txt` を出力するので、それを手でコピーする。defs.rs を直接編集した場合は生成器側との乖離に注意。
- `jdwp_command_map!` マクロが `JDWPPacketDataFromDebugger` / `JDWPPacketDataFromDebuggee` enum と送受信の dispatch を生成する。各コマンドの Send/Receive 構造体名は `<CommandSet><Command>Send/Receive`、引数なしのコマンドは `()`。
- `PacketData` トレイト (write_to / read_from) がバイナリ直列化、`ConvPrettyIOValue` (`packets/conv_input.rs`) が `PrettyIOKind` からの変換 (ore-jdwp 単体の対話 CLI `src/main.rs` 用) を担う。
- ID 型は `JDWPIDLengthEq{Object,ReferenceType,Method,Field,Frame}`。長さは `JDWPContext.id_sizes` に依存するため、read_from には context が必要。
