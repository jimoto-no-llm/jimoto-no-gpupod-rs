//! gpupod-control の CLI エントリポイント。

use std::io::{BufRead, Write};

use clap::{Parser, Subcommand};

use gpupod_control::{relay, web, App, Config};

#[derive(Parser)]
#[command(name = "gpupod-control", version, about = "Jimoto GPU Pod 中央サーバー")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// データベースのマイグレーションを適用する
    Migrate,
    /// 中継 SSH のホスト鍵を作成する（既存は作り替えない）
    InitControl,
    /// 承認済みテンプレートカタログを取り込む
    SyncTemplates {
        /// カタログファイル（既定 examples/templates.json）
        catalog: Option<String>,
    },
    /// 管理者アカウントを作成する
    CreateSuperuser,
    /// 中継 SSH サーバーだけを起動する
    Runrelay {
        /// リスン先ホスト（既定 0.0.0.0）
        #[arg(long)]
        host: Option<String>,
        /// リスン先ポート（既定 GPUPOD_RELAY_PORT）
        #[arg(long)]
        port: Option<u16>,
    },
    /// Web と中継を 1 プロセスで起動する
    Serve {
        /// HTTP のバインド先（既定 127.0.0.1:8000）
        #[arg(long)]
        bind: Option<String>,
    },
}

fn prompt(label: &str) -> anyhow::Result<String> {
    print!("{label}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(anyhow::Error::from)
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match cli.command {
        Command::Migrate => {
            let config = Config::from_env();
            config.ensure_state_dir()?;
            let db = gpupod_control::Db::open(&config.db_path())?;
            db.migrate()?;
            println!("マイグレーションを適用しました: {}", config.db_path().display());
        }
        Command::InitControl => {
            let config = Config::from_env();
            config.ensure_state_dir()?;
            let fingerprint = relay::init_host_key(&config.host_key_path())?;
            println!("Relay host key: {fingerprint}");
        }
        Command::SyncTemplates { catalog } => {
            let catalog = catalog.unwrap_or_else(|| "examples/templates.json".to_string());
            let templates = gpupod_common::load_templates(&catalog)?;
            let config = Config::from_env();
            config.ensure_state_dir()?;
            let db = gpupod_control::Db::open(&config.db_path())?;
            db.migrate()?;
            {
                let conn = db.lock();
                let ids: Vec<String> = templates.keys().cloned().collect();
                for (id, template) in &templates {
                    gpupod_control::models::upsert_template(&conn, id, &template.to_value())?;
                }
                gpupod_control::models::disable_templates_except(&conn, &ids)?;
            }
            println!("Imported {} templates", templates.len());
        }
        Command::CreateSuperuser => {
            let username = prompt("Username: ")?;
            if username.is_empty() {
                anyhow::bail!("Error: ユーザー名を入力してください");
            }
            let password = prompt("Password: ")?;
            let repeat = prompt("Password (again): ")?;
            if password != repeat {
                anyhow::bail!("Error: パスワードが一致しません");
            }
            if password.is_empty() {
                anyhow::bail!("Error: パスワードを入力してください");
            }
            let config = Config::from_env();
            config.ensure_state_dir()?;
            let db = gpupod_control::Db::open(&config.db_path())?;
            db.migrate()?;
            let encoded = gpupod_control::auth::hash_password(&password);
            let created = {
                let conn = db.lock();
                if gpupod_control::models::find_user_by_username(&conn, &username)?.is_some() {
                    None
                } else {
                    Some(gpupod_control::models::insert_user(
                        &conn,
                        &username,
                        &encoded,
                        true,
                        true,
                        true,
                        "",
                    )?)
                }
            };
            match created {
                Some(_) => println!("Superuser {username} を作成しました"),
                None => anyhow::bail!("Error: That username is already taken."),
            }
        }
        Command::Runrelay { host, port } => {
            let app = App::new(Config::from_env())?;
            let host = host.unwrap_or_else(|| "0.0.0.0".to_string());
            let port = port.unwrap_or(app.cfg.relay_port);
            runtime()?.block_on(relay::run_relay(app, host, port))?;
        }
        Command::Serve { bind } => {
            let app = App::new(Config::from_env())?;
            let bind = bind.unwrap_or_else(|| "127.0.0.1:8000".to_string());
            runtime()?.block_on(web::serve(app, &bind))?;
        }
    }
    Ok(())
}
