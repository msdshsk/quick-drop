use anyhow::{Context, Result};
use chrono::Local;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};

// Windowsの\\?\プレフィックスを削除するヘルパー関数
fn normalize_path(path: &Path) -> PathBuf {
    let path_str = path.to_string_lossy();
    if cfg!(windows) && path_str.starts_with(r"\\?\") {
        PathBuf::from(&path_str[4..])
    } else {
        path.to_path_buf()
    }
}

pub struct FtpConfig {
    pub port: u16,
    pub username: String,
    pub password: String,
    pub root_dir: String,
}

struct Session {
    authenticated: bool,
    username: Option<String>,
    current_dir: PathBuf,
    root_dir: PathBuf,
    pasv_listener: Option<TcpListener>,
    config: FtpConfig,
}

impl Session {
    fn new(config: FtpConfig, root_dir: PathBuf) -> Self {
        Self {
            authenticated: false,
            username: None,
            current_dir: PathBuf::from("/"),
            root_dir,
            pasv_listener: None,
            config,
        }
    }

    fn get_real_path(&self, virtual_path: &Path) -> Result<PathBuf> {
        eprintln!("[DEBUG] get_real_path: virtual_path={:?}, root_dir={:?}", virtual_path, self.root_dir);

        // 仮想パスを文字列として処理
        let virtual_path_str = virtual_path.to_string_lossy();

        // FTPの"/"から先頭の"/"を削除
        let cleaned_str = if virtual_path_str.starts_with('/') {
            &virtual_path_str[1..]
        } else {
            virtual_path_str.as_ref()
        };

        eprintln!("[DEBUG] get_real_path: cleaned_str={:?}", cleaned_str);

        // 空のパスの場合はルートディレクトリをそのまま返す（既に正規化済み）
        if cleaned_str.is_empty() {
            eprintln!("[DEBUG] get_real_path: empty path, returning={:?}", self.root_dir);
            return Ok(self.root_dir.clone());
        }

        let cleaned_path = Path::new(cleaned_str);
        eprintln!("[DEBUG] get_real_path: cleaned_path={:?}", cleaned_path);

        // ルートディレクトリと結合
        let full_path = self.root_dir.join(cleaned_path);
        eprintln!("[DEBUG] get_real_path: full_path={:?}", full_path);

        // セキュリティチェック: パストラバーサルを防ぐ
        // 文字列に".."が含まれていないか確認
        if cleaned_str.contains("..") {
            eprintln!("[ERROR] get_real_path: parent directory access attempted");
            return Err(anyhow::anyhow!("Access denied: parent directory access not allowed"));
        }

        // full_pathがルートディレクトリ配下にあることを確認
        // 両方を正規化して比較
        let canonical_full = if full_path.exists() {
            eprintln!("[DEBUG] get_real_path: full_path exists, canonicalizing");
            let canonical = full_path.canonicalize()?;
            normalize_path(&canonical)
        } else {
            eprintln!("[DEBUG] get_real_path: full_path does not exist");
            // 存在しない場合は親ディレクトリを正規化してファイル名を追加
            if let Some(parent) = full_path.parent() {
                eprintln!("[DEBUG] get_real_path: parent={:?}", parent);
                if parent.exists() {
                    let canonical_parent = normalize_path(&parent.canonicalize()?);
                    eprintln!("[DEBUG] get_real_path: canonical_parent={:?}", canonical_parent);
                    if let Some(file_name) = full_path.file_name() {
                        canonical_parent.join(file_name)
                    } else {
                        canonical_parent
                    }
                } else {
                    // 親が存在しない場合はエラー
                    eprintln!("[ERROR] get_real_path: parent directory does not exist");
                    return Err(anyhow::anyhow!("Parent directory does not exist"));
                }
            } else {
                full_path
            }
        };
        eprintln!("[DEBUG] get_real_path: canonical_full={:?}", canonical_full);

        // ルートディレクトリと比較（root_dirは既に正規化済み）
        eprintln!("[DEBUG] get_real_path: root_dir={:?}", self.root_dir);

        // Windowsでは大文字小文字を無視して比較する必要がある
        let canonical_full_str = canonical_full.to_string_lossy().to_lowercase();
        let root_dir_str = self.root_dir.to_string_lossy().to_lowercase();

        eprintln!("[DEBUG] get_real_path: canonical_full_str={}", canonical_full_str);
        eprintln!("[DEBUG] get_real_path: root_dir_str={}", root_dir_str);

        if !canonical_full_str.starts_with(&root_dir_str) {
            eprintln!("[ERROR] get_real_path: path outside root directory");
            eprintln!("[ERROR]   canonical_full={:?} (lowercase: {})", canonical_full, canonical_full_str);
            eprintln!("[ERROR]   root_dir={:?} (lowercase: {})", self.root_dir, root_dir_str);
            return Err(anyhow::anyhow!("Access denied: path outside root directory"));
        }

        eprintln!("[DEBUG] get_real_path: success, returning={:?}", canonical_full);
        Ok(canonical_full)
    }
}

async fn handle_client(
    stream: TcpStream,
    config: FtpConfig,
    log_tx: mpsc::UnboundedSender<String>,
) -> Result<()> {
    let peer_addr = stream.peer_addr()?;
    let server_addr = stream.local_addr()?;
    let _ = log_tx.send(format!("[{}] 新しい接続", peer_addr));

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // ルートディレクトリは既に正規化されている
    let root_dir = PathBuf::from(&config.root_dir);

    let session = Arc::new(Mutex::new(Session::new(config, root_dir)));

    // ウェルカムメッセージ
    writer
        .write_all(b"220 Local FTP Server Ready\r\n")
        .await?;

    let mut line = String::new();

    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line).await?;

        if bytes_read == 0 {
            let _ = log_tx.send(format!("[{}] 接続が閉じられました", peer_addr));
            break;
        }

        let command = line.trim();
        if command.is_empty() {
            continue;
        }

        let _ = log_tx.send(format!("[{}] コマンド: {}", peer_addr, command));

        let parts: Vec<&str> = command.splitn(2, ' ').collect();
        let cmd = parts[0].to_uppercase();
        let arg = parts.get(1).map(|s| s.trim());

        let response = handle_command(&cmd, arg, &session, &peer_addr, &server_addr, &log_tx).await?;

        writer.write_all(response.as_bytes()).await?;

        if cmd == "QUIT" {
            break;
        }
    }

    let _ = log_tx.send(format!("[{}] セッション終了", peer_addr));
    Ok(())
}

async fn handle_command(
    cmd: &str,
    arg: Option<&str>,
    session: &Arc<Mutex<Session>>,
    peer_addr: &SocketAddr,
    server_addr: &SocketAddr,
    log_tx: &mpsc::UnboundedSender<String>,
) -> Result<String> {
    match cmd {
        "USER" => {
            let username = arg.unwrap_or("");
            let mut sess = session.lock().await;
            sess.username = Some(username.to_string());
            Ok(format!("331 User {} OK. Password required\r\n", username))
        }
        "PASS" => {
            let password = arg.unwrap_or("");
            let mut sess = session.lock().await;

            if let Some(username) = sess.username.clone() {
                if username == sess.config.username && password == sess.config.password {
                    sess.authenticated = true;
                    let _ = log_tx.send(format!("[{}] 認証成功: {}", peer_addr, username));
                    Ok("230 User logged in\r\n".to_string())
                } else {
                    let _ = log_tx.send(format!("[{}] 認証失敗: {}", peer_addr, username));
                    Ok("530 Login incorrect\r\n".to_string())
                }
            } else {
                Ok("503 Login with USER first\r\n".to_string())
            }
        }
        "SYST" => Ok("215 Windows_NT\r\n".to_string()),
        "PWD" => {
            let sess = session.lock().await;
            if !sess.authenticated {
                return Ok("530 Please login with USER and PASS\r\n".to_string());
            }
            Ok(format!("257 \"/\" is current directory\r\n"))
        }
        "TYPE" => {
            let sess = session.lock().await;
            if !sess.authenticated {
                return Ok("530 Please login with USER and PASS\r\n".to_string());
            }
            Ok("200 Type set to I\r\n".to_string())
        }
        "PASV" => {
            let mut sess = session.lock().await;
            if !sess.authenticated {
                return Ok("530 Please login with USER and PASS\r\n".to_string());
            }

            // パッシブモード用のリスナーを作成（0.0.0.0で全インターフェースを待ち受け）
            let listener = TcpListener::bind("0.0.0.0:0").await?;
            let addr = listener.local_addr()?;
            sess.pasv_listener = Some(listener);

            // サーバのIPアドレスを使用（クライアントが接続してきたサーバのアドレス）
            let ip = match server_addr.ip() {
                IpAddr::V4(ip) => ip,
                _ => Ipv4Addr::new(127, 0, 0, 1),
            };
            let port = addr.port();
            let p1 = (port >> 8) as u8;
            let p2 = (port & 0xff) as u8;

            Ok(format!(
                "227 Entering Passive Mode ({},{},{},{},{},{})\r\n",
                ip.octets()[0],
                ip.octets()[1],
                ip.octets()[2],
                ip.octets()[3],
                p1,
                p2
            ))
        }
        "LIST" => {
            let mut sess = session.lock().await;
            if !sess.authenticated {
                return Ok("530 Please login with USER and PASS\r\n".to_string());
            }

            if let Some(listener) = sess.pasv_listener.take() {
                let _ = log_tx.send(format!("[DEBUG] LIST: current_dir={:?}, root_dir={:?}",
                    sess.current_dir, sess.root_dir));

                let real_path = match sess.get_real_path(&sess.current_dir) {
                    Ok(path) => {
                        let _ = log_tx.send(format!("[DEBUG] LIST: real_path={:?}", path));
                        path
                    },
                    Err(e) => {
                        let _ = log_tx.send(format!("[ERROR] LIST: get_real_path failed: {}", e));
                        return Ok("550 Failed to access directory\r\n".to_string());
                    },
                };
                let log_tx = log_tx.clone();

                tokio::spawn(async move {
                    if let Ok((mut stream, _)) = listener.accept().await {
                        if let Ok(entries) = fs::read_dir(&real_path) {
                            for entry in entries.flatten() {
                                if let Ok(metadata) = entry.metadata() {
                                    let file_type = if metadata.is_dir() { "d" } else { "-" };
                                    let size = metadata.len();
                                    let modified = metadata.modified().ok();
                                    let time_str = modified
                                        .map(|t| {
                                            let datetime: chrono::DateTime<Local> = t.into();
                                            datetime.format("%b %d %H:%M").to_string()
                                        })
                                        .unwrap_or_else(|| "Jan 01 00:00".to_string());

                                    let line = format!(
                                        "{}rwxr-xr-x 1 owner group {:>10} {} {}\r\n",
                                        file_type,
                                        size,
                                        time_str,
                                        entry.file_name().to_string_lossy()
                                    );
                                    let _ = stream.write_all(line.as_bytes()).await;
                                }
                            }
                        }
                        let _ = log_tx.send("LIST コマンド完了".to_string());
                    }
                });

                Ok("150 Opening data connection for directory list\r\n226 Transfer complete\r\n"
                    .to_string())
            } else {
                Ok("425 Use PASV first\r\n".to_string())
            }
        }
        "RETR" => {
            let mut sess = session.lock().await;
            if !sess.authenticated {
                return Ok("530 Please login with USER and PASS\r\n".to_string());
            }

            let filename = arg.unwrap_or("");
            let file_path = match sess.get_real_path(Path::new(filename)) {
                Ok(path) => path,
                Err(_) => return Ok("550 File not accessible\r\n".to_string()),
            };
            let log_tx = log_tx.clone();
            let filename_str = filename.to_string();

            if let Some(listener) = sess.pasv_listener.take() {
                tokio::spawn(async move {
                    if let Ok((mut stream, _)) = listener.accept().await {
                        if let Ok(mut file) = tokio::fs::File::open(&file_path).await {
                            let _ = tokio::io::copy(&mut file, &mut stream).await;
                            let _ = log_tx.send(format!("ダウンロード完了: {}", filename_str));
                        }
                    }
                });

                Ok(format!(
                    "150 Opening data connection for {}\r\n226 Transfer complete\r\n",
                    filename
                ))
            } else {
                Ok("425 Use PASV first\r\n".to_string())
            }
        }
        "STOR" => {
            let mut sess = session.lock().await;
            if !sess.authenticated {
                return Ok("530 Please login with USER and PASS\r\n".to_string());
            }

            let filename = arg.unwrap_or("");
            let file_path = match sess.get_real_path(Path::new(filename)) {
                Ok(path) => path,
                Err(_) => return Ok("550 File not accessible\r\n".to_string()),
            };
            let log_tx = log_tx.clone();
            let filename_str = filename.to_string();

            if let Some(listener) = sess.pasv_listener.take() {
                tokio::spawn(async move {
                    if let Ok((mut stream, _)) = listener.accept().await {
                        if let Ok(mut file) = tokio::fs::File::create(&file_path).await {
                            let _ = tokio::io::copy(&mut stream, &mut file).await;
                            let _ = log_tx.send(format!("アップロード完了: {}", filename_str));
                        }
                    }
                });

                Ok(format!(
                    "150 Opening data connection for {}\r\n226 Transfer complete\r\n",
                    filename
                ))
            } else {
                Ok("425 Use PASV first\r\n".to_string())
            }
        }
        "CWD" => {
            let sess = session.lock().await;
            if !sess.authenticated {
                return Ok("530 Please login with USER and PASS\r\n".to_string());
            }
            Ok("250 Directory changed to /\r\n".to_string())
        }
        "MKD" | "XMKD" => {
            let sess = session.lock().await;
            if !sess.authenticated {
                return Ok("530 Please login with USER and PASS\r\n".to_string());
            }

            let dirname = match arg {
                Some(d) if !d.is_empty() => d,
                _ => return Ok("501 Missing directory name\r\n".to_string()),
            };

            // 仮想パスを構築（絶対パスか相対パスかを判定）
            let virtual_path = if dirname.starts_with('/') {
                PathBuf::from(dirname)
            } else {
                sess.current_dir.join(dirname)
            };

            let real_path = match sess.get_real_path(&virtual_path) {
                Ok(path) => path,
                Err(_) => return Ok("550 Failed to create directory\r\n".to_string()),
            };

            match fs::create_dir(&real_path) {
                Ok(_) => {
                    let _ = log_tx.send(format!("[{}] ディレクトリ作成: {}", peer_addr, dirname));
                    Ok(format!("257 \"{}\" directory created\r\n", dirname))
                }
                Err(e) => {
                    let _ = log_tx.send(format!("[{}] ディレクトリ作成失敗: {} - {}", peer_addr, dirname, e));
                    Ok(format!("550 Failed to create directory: {}\r\n", e))
                }
            }
        }
        "RMD" | "XRMD" => {
            let sess = session.lock().await;
            if !sess.authenticated {
                return Ok("530 Please login with USER and PASS\r\n".to_string());
            }

            let dirname = match arg {
                Some(d) if !d.is_empty() => d,
                _ => return Ok("501 Missing directory name\r\n".to_string()),
            };

            let virtual_path = if dirname.starts_with('/') {
                PathBuf::from(dirname)
            } else {
                sess.current_dir.join(dirname)
            };

            let real_path = match sess.get_real_path(&virtual_path) {
                Ok(path) => path,
                Err(_) => return Ok("550 Failed to remove directory\r\n".to_string()),
            };

            match fs::remove_dir(&real_path) {
                Ok(_) => {
                    let _ = log_tx.send(format!("[{}] ディレクトリ削除: {}", peer_addr, dirname));
                    Ok("250 Directory removed\r\n".to_string())
                }
                Err(e) => {
                    let _ = log_tx.send(format!("[{}] ディレクトリ削除失敗: {} - {}", peer_addr, dirname, e));
                    Ok(format!("550 Failed to remove directory: {}\r\n", e))
                }
            }
        }
        "DELE" => {
            let sess = session.lock().await;
            if !sess.authenticated {
                return Ok("530 Please login with USER and PASS\r\n".to_string());
            }

            let filename = match arg {
                Some(f) if !f.is_empty() => f,
                _ => return Ok("501 Missing file name\r\n".to_string()),
            };

            let virtual_path = if filename.starts_with('/') {
                PathBuf::from(filename)
            } else {
                sess.current_dir.join(filename)
            };

            let real_path = match sess.get_real_path(&virtual_path) {
                Ok(path) => path,
                Err(_) => return Ok("550 File not accessible\r\n".to_string()),
            };

            match fs::remove_file(&real_path) {
                Ok(_) => {
                    let _ = log_tx.send(format!("[{}] ファイル削除: {}", peer_addr, filename));
                    Ok("250 File deleted\r\n".to_string())
                }
                Err(e) => {
                    let _ = log_tx.send(format!("[{}] ファイル削除失敗: {} - {}", peer_addr, filename, e));
                    Ok(format!("550 Failed to delete file: {}\r\n", e))
                }
            }
        }
        "QUIT" => Ok("221 Goodbye\r\n".to_string()),
        "NOOP" => Ok("200 OK\r\n".to_string()),
        _ => Ok(format!("502 Command not implemented\r\n")),
    }
}

pub async fn run_server(
    config: FtpConfig,
    log_tx: mpsc::UnboundedSender<String>,
) -> Result<()> {
    let port = config.port;
    let root_dir = config.root_dir.clone();

    // ルートディレクトリの作成と正規化
    let root_path = PathBuf::from(&root_dir);
    if !root_path.exists() {
        fs::create_dir_all(&root_path).with_context(|| {
            format!("ルートディレクトリ '{}' の作成に失敗しました", root_dir)
        })?;
        let _ = log_tx.send(format!("ルートディレクトリを作成しました: {}", root_dir));
    }

    // ルートディレクトリを絶対パスに正規化
    let canonical_root = root_path.canonicalize().with_context(|| {
        format!("ルートディレクトリ '{}' の正規化に失敗しました", root_dir)
    })?;

    // Windowsの\\?\プレフィックスを削除
    let normalized_root = normalize_path(&canonical_root);
    let canonical_root_str = normalized_root.to_string_lossy().to_string();
    let _ = log_tx.send(format!("正規化されたルートディレクトリ: {}", canonical_root_str));

    // TCPリスナーの起動
    let addr = format!("0.0.0.0:{}", port);
    let listener = TcpListener::bind(&addr).await?;

    let _ = log_tx.send(format!("FTPサーバを起動しました: {}", addr));

    // クライアント接続の受け入れ
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let config_clone = FtpConfig {
                    port: config.port,
                    username: config.username.clone(),
                    password: config.password.clone(),
                    root_dir: canonical_root_str.clone(),
                };
                let log_tx = log_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, config_clone, log_tx.clone()).await {
                        let _ = log_tx.send(format!("クライアント処理エラー: {}", e));
                    }
                });
            }
            Err(e) => {
                let _ = log_tx.send(format!("接続受け入れエラー: {}", e));
            }
        }
    }
}
