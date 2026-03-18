# QuickDrop 開発で得た技術ナレッジ

このドキュメントは、Windows向けFTPサーバアプリケーション「QuickDrop」の開発で得た技術的な学びをまとめたものです。

## 目次

1. [Windowsでビルド可能なライブラリ選定](#windowsでビルド可能なライブラリ選定)
2. [ローカルサーバのPASVモード実装](#ローカルサーバのpasvモード実装)
3. [Windowsパス解決の落とし穴](#windowsパス解決の落とし穴)
4. [その他の学び](#その他の学び)

---

## Windowsでビルド可能なライブラリ選定

### 問題: libunftpの依存関係エラー

当初、FTPサーバ実装に `libunftp` クレートを使用しようとしたが、以下のビルドエラーが発生：

```
error: failed to run custom build command for `openssl-sys v0.9.104`
...
Could not find directory of OpenSSL installation
```

さらに調査すると、以下の外部依存が必要：
- **CMake**: C/C++プロジェクトのビルドツール
- **NASM**: アセンブラ
- **Perl**: ビルドスクリプト実行用
- **OpenSSL**: 暗号化ライブラリ

### 解決策: Pure Rust実装

外部依存を避けるため、`tokio` のみを使用した自作FTPサーバを実装：

```toml
[dependencies]
tokio = { version = "1", features = ["full"] }
anyhow = "1.0"
chrono = "0.4"
```

### 学び

- **Windows環境での開発**: 外部ツール（CMake, NASM等）への依存は避けるべき
- **Pure Rust の優位性**: クロスコンパイルや配布が容易
- **tokio の強力さ**: 非同期I/O、TCP通信、ファイル操作など、必要な機能が全て揃っている

### 推奨アプローチ

Windows向けアプリケーションでは：
1. まず Pure Rust なクレートを探す
2. 外部依存が必要な場合は、`*-sys` クレートの依存関係を確認
3. プロトコル実装が必要な場合は、tokio で自作も検討

---

## ローカルサーバのPASVモード実装

### 問題1: ローカルホストのみでバインド

初期実装では以下のように実装していた：

```rust
// ❌ 問題のあるコード
"PASV" => {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    // ...
}
```

**症状**:
- 同一PC内（localhost）からは接続可能
- 別PCからの接続で「対象のコンピューターによって拒否されました」エラー

**原因**:
`127.0.0.1` にバインドすると、ローカルループバックインターフェースのみが待ち受け状態になり、外部からのアクセスを受け付けない。

### 問題2: 応答IPアドレスの不一致

さらに、PASV応答で返すIPアドレスもリスナーのアドレスを使用していた：

```rust
// ❌ 問題のあるコード
let ip = match addr.ip() {  // addrはlistener.local_addr()
    IpAddr::V4(ip) => ip,
    _ => Ipv4Addr::new(127, 0, 0, 1),
};
// クライアントに127.0.0.1が返される
```

**症状**:
別PCのFTPクライアントが `127.0.0.1` (自分自身のlocalhost) に接続しようとして失敗。

### 解決策

#### 1. 全インターフェースでバインド

```rust
// ✅ 正しいコード
let listener = TcpListener::bind("0.0.0.0:0").await?;
```

`0.0.0.0` は「全てのネットワークインターフェース」を意味する特殊なアドレス。これにより、以下のすべてのインターフェースで待ち受けが可能：
- `127.0.0.1` (localhost)
- `192.168.x.x` (プライベートネットワーク)
- その他のネットワークインターフェース

#### 2. サーバの実IPアドレスを返す

```rust
// ✅ 正しいコード
async fn handle_client(
    stream: TcpStream,
    config: FtpConfig,
    log_tx: mpsc::UnboundedSender<String>,
) -> Result<()> {
    let peer_addr = stream.peer_addr()?;      // クライアントのアドレス
    let server_addr = stream.local_addr()?;   // サーバのアドレス（重要）
    // ...
}

// PASV応答でserver_addrのIPを使用
"PASV" => {
    let listener = TcpListener::bind("0.0.0.0:0").await?;
    let addr = listener.local_addr()?;

    // サーバの実際のIPアドレスを使用
    let ip = match server_addr.ip() {
        IpAddr::V4(ip) => ip,
        _ => Ipv4Addr::new(127, 0, 0, 1),
    };
    let port = addr.port();

    // 227応答でクライアントに通知
    Ok(format!("227 Entering Passive Mode ({},{},{},{},{},{})\r\n",
        ip.octets()[0], ip.octets()[1], ip.octets()[2], ip.octets()[3],
        (port >> 8) as u8, (port & 0xff) as u8
    ))
}
```

### 学び

#### PASVモードの仕組み

1. **コントロール接続**: クライアント → サーバ（ポート2121等）
2. **PASV コマンド**: クライアントがサーバに送信
3. **227 応答**: サーバがデータ接続用の (IP, ポート) を返す
4. **データ接続**: クライアント → サーバの指定IP:ポート

#### 重要なポイント

- **バインドアドレス**: リスナーがどのインターフェースで待ち受けるか
  - `127.0.0.1:0` → localhostのみ
  - `0.0.0.0:0` → 全インターフェース

- **応答IPアドレス**: クライアントに通知するIP
  - リスナーのIP（`listener.local_addr().ip()`）ではなく
  - サーバ接続時のIP（`stream.local_addr().ip()`）を使用

- **ポート番号**: OSが自動割り当て（`:0` 指定）
  - `listener.local_addr().port()` で取得
  - 上位8ビット・下位8ビットに分割して送信

#### トラブルシューティング

| 症状 | 原因 | 解決策 |
|------|------|--------|
| 別PCから接続できない | `127.0.0.1` バインド | `0.0.0.0` に変更 |
| 接続後にデータ転送失敗 | 応答IPが`127.0.0.1` | `server_addr.ip()` を使用 |
| Windowsファイアウォール | ポートがブロック | ファイアウォール設定を確認 |

---

## Windowsパス解決の落とし穴

### 問題1: `\\?\` プレフィックス

Windowsの `canonicalize()` は正規化されたパスに `\\?\` プレフィックスを付加する：

```rust
let path = PathBuf::from("D:\\share");
let canonical = path.canonicalize()?;
println!("{:?}", canonical);
// 出力: "\\\\?\\D:\\share"
```

このプレフィックスは：
- **目的**: 長いパス名（260文字以上）のサポート、Win32ファイル名前空間のバイパス
- **問題**: 文字列比較時にマッチしない

```rust
// ❌ 問題のあるコード
let root = PathBuf::from("D:\\share");
let canonical = root.canonicalize()?;  // "\\\\?\\D:\\share"

if canonical.starts_with(&root) {  // false!
    // 実行されない
}
```

### 解決策1: normalize_path() ヘルパー関数

```rust
// ✅ 正しいコード
fn normalize_path(path: &Path) -> PathBuf {
    let path_str = path.to_string_lossy();
    if cfg!(windows) && path_str.starts_with(r"\\?\") {
        PathBuf::from(&path_str[4..])  // "\\\\?\\" の4文字を削除
    } else {
        path.to_path_buf()
    }
}

// 使用例
let canonical = path.canonicalize()?;
let normalized = normalize_path(&canonical);  // "D:\\share"
```

### 問題2: 大文字小文字の区別

Windowsのファイルシステム（NTFS）は大文字小文字を区別しないが、Rustの文字列比較は区別する：

```rust
// ❌ 問題のあるコード
let path1 = PathBuf::from("D:\\Share");
let path2 = PathBuf::from("d:\\share");

if path1 == path2 {  // false!
    // 実行されない（同じディレクトリなのに）
}
```

### 解決策2: 小文字化して比較

```rust
// ✅ 正しいコード
let canonical_full_str = canonical_full.to_string_lossy().to_lowercase();
let root_dir_str = self.root_dir.to_string_lossy().to_lowercase();

if canonical_full_str.starts_with(&root_dir_str) {
    // 大文字小文字を無視して正しく比較
}
```

### 問題3: FTPパス "/" の処理

FTPでは "/" がルートディレクトリを表すが、Windowsパスとして処理するとドライブルートになる：

```rust
// ❌ 問題のあるコード
let virtual_path = Path::new("/");
let full_path = root_dir.join(virtual_path);  // "D:/" になる！
```

**症状**:
- 設定: `root_dir = "D:\\share"`
- FTPコマンド: `LIST /`
- 期待: `D:\share` の内容
- 実際: `D:\` の内容（ドライブルート）

### 解決策3: 文字列ベースのパス処理

```rust
// ✅ 正しいコード
fn get_real_path(&self, virtual_path: &Path) -> Result<PathBuf> {
    let virtual_path_str = virtual_path.to_string_lossy();

    // 先頭の "/" を削除
    let cleaned_str = if virtual_path_str.starts_with('/') {
        &virtual_path_str[1..]  // "/" → ""
    } else {
        virtual_path_str.as_ref()
    };

    // 空文字列の場合はルートディレクトリ
    if cleaned_str.is_empty() {
        return Ok(self.root_dir.clone());
    }

    // 通常のパス結合
    let full_path = self.root_dir.join(Path::new(cleaned_str));
    // ...
}
```

### 問題4: パストラバーサル攻撃

FTPクライアントが `../../../etc/passwd` のような相対パスを送信してくる可能性：

```rust
// ❌ 危険なコード
let file_path = root_dir.join(user_input);  // 無検証
```

### 解決策4: セキュリティチェック

```rust
// ✅ 正しいコード（多層防御）

// 1. 文字列レベルのチェック
if cleaned_str.contains("..") {
    return Err(anyhow::anyhow!("Access denied: parent directory access not allowed"));
}

// 2. 正規化後のパスチェック
let canonical_full = full_path.canonicalize()?;
let normalized = normalize_path(&canonical_full);

let canonical_full_str = normalized.to_string_lossy().to_lowercase();
let root_dir_str = self.root_dir.to_string_lossy().to_lowercase();

if !canonical_full_str.starts_with(&root_dir_str) {
    return Err(anyhow::anyhow!("Access denied: path outside root directory"));
}
```

### 学び: Windowsパス処理のベストプラクティス

1. **常に正規化**: `canonicalize()` を使用して絶対パスを取得
2. **プレフィックス除去**: `normalize_path()` で `\\?\` を削除
3. **大文字小文字を無視**: `to_lowercase()` してから比較
4. **文字列ベース処理**: FTPパスは文字列として前処理
5. **セキュリティ第一**: 多層防御（文字列チェック + パスチェック）

#### 完全な実装例

```rust
fn normalize_path(path: &Path) -> PathBuf {
    let path_str = path.to_string_lossy();
    if cfg!(windows) && path_str.starts_with(r"\\?\") {
        PathBuf::from(&path_str[4..])
    } else {
        path.to_path_buf()
    }
}

fn get_real_path(&self, virtual_path: &Path) -> Result<PathBuf> {
    // 1. FTPパスを文字列として処理
    let virtual_path_str = virtual_path.to_string_lossy();
    let cleaned_str = if virtual_path_str.starts_with('/') {
        &virtual_path_str[1..]
    } else {
        virtual_path_str.as_ref()
    };

    // 2. 空パス = ルートディレクトリ
    if cleaned_str.is_empty() {
        return Ok(self.root_dir.clone());
    }

    // 3. セキュリティチェック
    if cleaned_str.contains("..") {
        return Err(anyhow::anyhow!("Access denied"));
    }

    // 4. パス結合と正規化
    let full_path = self.root_dir.join(Path::new(cleaned_str));
    let canonical_full = if full_path.exists() {
        normalize_path(&full_path.canonicalize()?)
    } else {
        // 非存在ファイルの処理...
        if let Some(parent) = full_path.parent() {
            if parent.exists() {
                let canonical_parent = normalize_path(&parent.canonicalize()?);
                if let Some(file_name) = full_path.file_name() {
                    canonical_parent.join(file_name)
                } else {
                    canonical_parent
                }
            } else {
                return Err(anyhow::anyhow!("Parent directory does not exist"));
            }
        } else {
            full_path
        }
    };

    // 5. ルートディレクトリ配下チェック（大文字小文字を無視）
    let canonical_full_str = canonical_full.to_string_lossy().to_lowercase();
    let root_dir_str = self.root_dir.to_string_lossy().to_lowercase();

    if !canonical_full_str.starts_with(&root_dir_str) {
        return Err(anyhow::anyhow!("Access denied: path outside root directory"));
    }

    Ok(canonical_full)
}
```

---

## その他の学び

### 1. eframe/egui での日本語フォント

Windowsの日本語フォント（メイリオ）を動的に読み込む：

```rust
let mut fonts = egui::FontDefinitions::default();

if let Ok(font_data) = std::fs::read("C:\\Windows\\Fonts\\meiryo.ttc") {
    fonts.font_data.insert(
        "meiryo".to_owned(),
        egui::FontData::from_owned(font_data),
    );

    // ProportionalとMonospaceの両方に設定
    fonts.families
        .entry(egui::FontFamily::Proportional)
        .or_default()
        .insert(0, "meiryo".to_owned());

    fonts.families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .insert(0, "meiryo".to_owned());

    cc.egui_ctx.set_fonts(fonts);
}
```

### 2. Windowsアプリケーションのコンソール非表示

```rust
// src/main.rs の先頭
#![windows_subsystem = "windows"]
```

**注意**: デバッグ時はコメントアウトしてコンソール出力を確認

### 3. アイコン埋め込み

```rust
// build.rs
fn main() {
    if cfg!(target_os = "windows") {
        let mut res = winres::WindowsResource::new();
        res.set_icon("quick_drop.ico");
        res.compile().unwrap();
    }
}
```

```toml
# Cargo.toml
[build-dependencies]
winres = "0.1"
```

### 4. tokio ランタイムの管理

GUI（egui）と非同期サーバ（tokio）を併用する場合：

```rust
struct FtpServerApp {
    runtime: Arc<Runtime>,  // Arc で共有
    server_handle: Option<tokio::task::JoinHandle<()>>,
    // ...
}

impl FtpServerApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap(),
        );
        // ...
    }

    fn start_server(&mut self) {
        let runtime = self.runtime.clone();
        let handle = runtime.spawn(async move {
            // サーバ処理
        });
        self.server_handle = Some(handle);
    }

    fn stop_server(&mut self) {
        if let Some(handle) = self.server_handle.take() {
            handle.abort();  // タスクを中止
        }
    }
}
```

### 5. リアルタイムログ表示

mpsc チャネルでサーバからGUIにログを送信：

```rust
// サーバスレッド
let (log_tx, log_rx) = mpsc::unbounded_channel();

// サーバ内
let _ = log_tx.send(format!("[{}] 接続成功", peer_addr));

// GUI更新ループ
if let Some(ref mut rx) = self.log_receiver {
    while let Ok(log) = rx.try_recv() {
        self.logs.push(log);
    }
}

// 自動スクロール
egui::ScrollArea::vertical()
    .stick_to_bottom(true)
    .show(ui, |ui| {
        for log in &self.logs {
            ui.label(log);
        }
    });
```

---

## まとめ

### Windows向けRustアプリケーション開発のポイント

1. **依存関係**: Pure Rustを優先、外部ツール依存を避ける
2. **ネットワーク**: ローカルサーバは `0.0.0.0` でバインド
3. **パス処理**:
   - `\\?\` プレフィックスを除去
   - 大文字小文字を無視して比較
   - セキュリティチェックを多層化
4. **GUI**: 日本語フォントの動的読み込み
5. **非同期**: tokio ランタイムを Arc で共有管理

### 参考リソース

- [tokio ドキュメント](https://docs.rs/tokio/)
- [egui ドキュメント](https://docs.rs/egui/)
- [Windows ファイルパスの名前空間](https://learn.microsoft.com/ja-jp/windows/win32/fileio/naming-a-file)
- [RFC 959 - FTP Protocol Specification](https://www.rfc-editor.org/rfc/rfc959)
