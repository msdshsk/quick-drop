// デバッグ用に一時的にコメントアウト
// #![windows_subsystem = "windows"]

mod ftp_server;

use eframe::egui;
use ftp_server::FtpConfig;
use serde::{Deserialize, Serialize};
use std::fs;
use std::sync::Arc;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

#[derive(Serialize, Deserialize, Clone)]
struct Config {
    port: u16,
    username: String,
    password: String,
    root_dir: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 2121,
            username: "admin".to_string(),
            password: "password".to_string(),
            root_dir: "./ftp_root".to_string(),
        }
    }
}

impl Config {
    fn load() -> Self {
        let config_path = "config.toml";
        if let Ok(content) = fs::read_to_string(config_path) {
            if let Ok(config) = toml::from_str(&content) {
                return config;
            }
        }
        Config::default()
    }

    fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let content = toml::to_string_pretty(self)?;
        fs::write("config.toml", content)?;
        Ok(())
    }
}

struct FtpServerApp {
    config: Config,
    server_running: bool,
    logs: Vec<String>,
    log_receiver: Option<mpsc::UnboundedReceiver<String>>,
    runtime: Arc<Runtime>,
    server_handle: Option<tokio::task::JoinHandle<()>>,
    show_about: bool,
}

impl FtpServerApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // 日本語フォントの設定
        let mut fonts = egui::FontDefinitions::default();

        // Windowsのメイリオフォントを動的に読み込み
        if let Ok(font_data) = std::fs::read("C:\\Windows\\Fonts\\meiryo.ttc") {
            fonts.font_data.insert(
                "meiryo".to_owned(),
                egui::FontData::from_owned(font_data),
            );

            // フォールバックフォントとして日本語フォントを設定
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "meiryo".to_owned());

            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .insert(0, "meiryo".to_owned());

            cc.egui_ctx.set_fonts(fonts);
        }

        let config = Config::load();
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap(),
        );

        Self {
            config,
            server_running: false,
            logs: Vec::new(),
            log_receiver: None,
            runtime,
            server_handle: None,
            show_about: false,
        }
    }

    fn start_server(&mut self) {
        if self.server_running {
            return;
        }

        // ログチャネルの作成
        let (log_tx, log_rx) = mpsc::unbounded_channel();
        self.log_receiver = Some(log_rx);

        // 設定を保存
        let _ = self.config.save();

        // FTPサーバ設定
        let ftp_config = FtpConfig {
            port: self.config.port,
            username: self.config.username.clone(),
            password: self.config.password.clone(),
            root_dir: self.config.root_dir.clone(),
        };

        self.logs.push("========================================".to_string());
        self.logs.push("  QuickDrop を起動しています...".to_string());
        self.logs.push("========================================".to_string());
        self.logs
            .push(format!("ポート: {}", self.config.port));
        self.logs
            .push(format!("ユーザー名: {}", self.config.username));
        self.logs
            .push(format!("ルートディレクトリ: {}", self.config.root_dir));
        self.logs.push("========================================".to_string());

        // サーバを起動
        let runtime = self.runtime.clone();
        let handle = runtime.spawn(async move {
            if let Err(e) = ftp_server::run_server(ftp_config, log_tx).await {
                eprintln!("サーバエラー: {}", e);
            }
        });

        self.server_handle = Some(handle);
        self.server_running = true;
    }

    fn stop_server(&mut self) {
        if !self.server_running {
            return;
        }

        if let Some(handle) = self.server_handle.take() {
            handle.abort();
        }

        self.logs.push("========================================".to_string());
        self.logs.push("  QuickDrop を停止しました".to_string());
        self.logs.push("========================================".to_string());

        self.server_running = false;
        self.log_receiver = None;
    }

    fn select_directory(&mut self) {
        if let Some(path) = rfd::FileDialog::new().pick_folder() {
            self.config.root_dir = path.to_string_lossy().to_string();
        }
    }
}

impl eframe::App for FtpServerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ログの受信
        if let Some(ref mut rx) = self.log_receiver {
            while let Ok(log) = rx.try_recv() {
                self.logs.push(log);
            }
        }

        // UIの更新をリクエスト
        ctx.request_repaint();

        // メニューバー
        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("ヘルプ", |ui| {
                    if ui.button("バージョン情報").clicked() {
                        self.show_about = true;
                        ui.close_menu();
                    }
                });
            });
        });

        // バージョン情報ダイアログ
        if self.show_about {
            egui::Window::new("バージョン情報")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.heading("QuickDrop");
                        ui.add_space(8.0);
                        ui.label(format!("バージョン: {}", env!("CARGO_PKG_VERSION")));
                        ui.label("簡易FTPサーバ");
                        ui.add_space(12.0);
                        if ui.button("閉じる").clicked() {
                            self.show_about = false;
                        }
                    });
                });
        }

        // トップパネル: 設定
        egui::TopBottomPanel::top("settings_panel").show(ctx, |ui| {
            ui.heading("QuickDrop - FTP Server");
            ui.separator();

            ui.horizontal(|ui| {
                ui.label("ポート:");
                ui.add_enabled(
                    !self.server_running,
                    egui::DragValue::new(&mut self.config.port)
                        .speed(1)
                        .range(1024..=65535),
                );
            });

            ui.horizontal(|ui| {
                ui.label("ユーザー名:");
                ui.add_enabled(
                    !self.server_running,
                    egui::TextEdit::singleline(&mut self.config.username).desired_width(200.0),
                );
            });

            ui.horizontal(|ui| {
                ui.label("パスワード:");
                ui.add_enabled(
                    !self.server_running,
                    egui::TextEdit::singleline(&mut self.config.password)
                        .password(true)
                        .desired_width(200.0),
                );
            });

            ui.horizontal(|ui| {
                ui.label("ルートディレクトリ:");
                ui.add_enabled(
                    !self.server_running,
                    egui::TextEdit::singleline(&mut self.config.root_dir).desired_width(300.0),
                );
                if ui
                    .add_enabled(!self.server_running, egui::Button::new("参照..."))
                    .clicked()
                {
                    self.select_directory();
                }
            });

            ui.separator();

            ui.horizontal(|ui| {
                if self.server_running {
                    if ui.button("■ 停止").clicked() {
                        self.stop_server();
                    }
                    ui.label("[稼働中]");
                } else {
                    if ui.button("▶ 起動").clicked() {
                        self.start_server();
                    }
                    ui.label("[停止中]");
                }
            });
        });

        // 中央パネル: ログ表示
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("ログ");
            ui.separator();

            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for log in &self.logs {
                        ui.label(log);
                    }
                });
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // アプリケーション終了時にサーバを停止
        self.stop_server();
    }
}

fn main() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([800.0, 600.0])
            .with_min_inner_size([600.0, 400.0]),
        ..Default::default()
    };

    eframe::run_native(
        "QuickDrop",
        options,
        Box::new(|cc| Ok(Box::new(FtpServerApp::new(cc)))),
    )
}
