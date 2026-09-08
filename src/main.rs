use std::io::Write;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use wardn::{Db, config, members::Member, org::Org, roles::Role, serve};

#[derive(Parser)]
#[command(name = "wardn", version, about = "Org & access layer for Mynd")]
struct Cli {
    /// Path to the local libSQL database. Defaults to
    /// `~/.local/share/wardn/org.db`, or `$WARDN_DB_PATH` if set.
    #[arg(long, global = true)]
    db: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// First-run setup: creates the local database and the org.
    Init {
        #[arg(long)]
        name: Option<String>,
    },
    /// Manage the org this instance holds.
    Org {
        #[command(subcommand)]
        command: OrgCommand,
    },
    /// Manage API keys used to authenticate against `wardn serve`.
    Keys {
        #[command(subcommand)]
        command: KeysCommand,
    },
    /// Org name, member count, storage path, and whether `wardn serve` is
    /// reachable.
    Status,
    /// Start the HTTP authorization service Mynd instances call.
    Serve {
        #[arg(long)]
        listen: Option<String>,
    },
}

#[derive(Subcommand)]
enum OrgCommand {
    /// Creates the org (fails if one already exists — see Decision 2).
    Create {
        #[arg(long)]
        name: String,
    },
    Rename {
        #[arg(long)]
        name: String,
    },
    /// Deletes the org and every member/key with it. Requires confirmation
    /// unless `--yes` is passed.
    Delete {
        #[arg(long)]
        yes: bool,
    },
    Invite {
        email: String,
        #[arg(long, default_value = "member")]
        role: String,
    },
    Members {
        #[command(subcommand)]
        command: MembersCommand,
    },
    Role {
        #[command(subcommand)]
        command: RoleCommand,
    },
}

#[derive(Subcommand)]
enum MembersCommand {
    List,
    Remove { member_id: String },
}

#[derive(Subcommand)]
enum RoleCommand {
    Set { member_id: String, role: String },
}

#[derive(Subcommand)]
enum KeysCommand {
    Create {
        #[arg(long)]
        member: String,
        #[arg(long)]
        name: Option<String>,
    },
    Revoke {
        key_id: String,
    },
    List,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let db_path = config::db_path(cli.db.as_deref());
    let is_serve = matches!(cli.command, Command::Serve { .. });
    init_tracing(is_serve)?;

    match cli.command {
        Command::Init { name } => cmd_init(&db_path, name).await,
        Command::Org { command } => cmd_org(&db_path, command).await,
        Command::Keys { command } => cmd_keys(&db_path, command).await,
        Command::Status => cmd_status(&db_path).await,
        Command::Serve { listen } => cmd_serve(&db_path, listen).await,
    }
}

fn default_env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

/// One-off CLI commands (`org invite`, `keys create`, ...) get plain,
/// human-readable logs — nobody wants JSON scrolling past a `wardn status`
/// call. Only `wardn serve`, the long-running process, gets the full
/// structured setup: JSON logs, plus OTel trace export and Loki log
/// shipping when the "observability" feature is compiled in *and* their
/// env vars are set (see `src/config.rs`) — both are independently
/// optional, so this degrades cleanly all the way down to stdout-only.
fn init_tracing(serve_mode: bool) -> Result<()> {
    if !serve_mode {
        tracing_subscriber::fmt()
            .with_env_filter(default_env_filter())
            .init();
        return Ok(());
    }

    #[cfg(feature = "observability")]
    {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;

        let otel_layer = config::observability::otel_exporter_otlp_endpoint()
            .as_deref()
            .map(wardn::telemetry::init_tracer)
            .transpose()?
            .map(|tracer| tracing_opentelemetry::layer().with_tracer(tracer));

        let mut loki_task = None;
        let loki_layer = match config::observability::loki_url() {
            Some(url) => {
                let (layer, task) = wardn::telemetry::init_loki_layer(&url)?;
                loki_task = Some(task);
                Some(layer)
            }
            None => None,
        };

        tracing_subscriber::registry()
            .with(default_env_filter())
            .with(tracing_subscriber::fmt::layer().json())
            .with(otel_layer)
            .with(loki_layer)
            .init();

        if let Some(task) = loki_task {
            tokio::spawn(task);
        }
    }

    #[cfg(not(feature = "observability"))]
    {
        tracing_subscriber::fmt()
            .with_env_filter(default_env_filter())
            .init();
    }

    Ok(())
}

fn prompt(message: &str) -> Result<String> {
    print!("{message}");
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_string())
}

fn print_org(org: &Org) {
    println!("org: {} ({})", org.name, org.id);
}

fn print_member(member: &Member) {
    let status = if member.removed_at.is_some() {
        " [removed]"
    } else {
        ""
    };
    println!("{}\t{}\t{}{}", member.id, member.email, member.role, status);
}

async fn cmd_init(db_path: &str, name: Option<String>) -> Result<()> {
    let db = Db::open(db_path).await?;
    if let Some(existing) = wardn::org::get(&db.conn).await? {
        println!("wardn is already initialized at {db_path}");
        print_org(&existing);
        return Ok(());
    }
    let name = match name {
        Some(name) => name,
        None => prompt("Org name: ")?,
    };
    if name.trim().is_empty() {
        bail!("org name must not be empty");
    }
    let org = wardn::org::create(&db.conn, name.trim()).await?;
    println!("Created database at {db_path}");
    print_org(&org);
    Ok(())
}

async fn cmd_org(db_path: &str, command: OrgCommand) -> Result<()> {
    let db = Db::open(db_path)
        .await
        .with_context(|| format!("opening {db_path}"))?;
    match command {
        OrgCommand::Create { name } => {
            let org = wardn::org::create(&db.conn, &name).await?;
            print_org(&org);
        }
        OrgCommand::Rename { name } => {
            let org = wardn::org::rename(&db.conn, &name).await?;
            print_org(&org);
        }
        OrgCommand::Delete { yes } => {
            let Some(org) = wardn::org::get(&db.conn).await? else {
                bail!("no org exists yet — run `wardn init` first");
            };
            if !yes {
                let answer = prompt(&format!(
                    "This permanently deletes org \"{}\" and all its members and API keys. Type the org name to confirm: ",
                    org.name
                ))?;
                if answer != org.name {
                    bail!("confirmation did not match — org not deleted");
                }
            }
            wardn::org::delete(&db.conn).await?;
            println!("Deleted org \"{}\"", org.name);
        }
        OrgCommand::Invite { email, role } => {
            let role: Role = role
                .parse()
                .map_err(|e: wardn::roles::InvalidRole| anyhow::anyhow!(e))?;
            let member = wardn::members::invite(&db.conn, &email, role, None).await?;
            print_member(&member);
        }
        OrgCommand::Members { command } => match command {
            MembersCommand::List => {
                for member in wardn::members::list(&db.conn).await? {
                    print_member(&member);
                }
            }
            MembersCommand::Remove { member_id } => {
                wardn::members::remove(&db.conn, &member_id).await?;
                println!("Removed member {member_id}");
            }
        },
        OrgCommand::Role { command } => match command {
            RoleCommand::Set { member_id, role } => {
                let role: Role = role
                    .parse()
                    .map_err(|e: wardn::roles::InvalidRole| anyhow::anyhow!(e))?;
                let member = wardn::members::set_role(&db.conn, &member_id, role).await?;
                print_member(&member);
            }
        },
    }
    Ok(())
}

async fn cmd_keys(db_path: &str, command: KeysCommand) -> Result<()> {
    let db = Db::open(db_path).await?;
    match command {
        KeysCommand::Create { member, name } => {
            let (key, full_key) =
                wardn::api_keys::create(&db.conn, &member, name.as_deref()).await?;
            println!("Created API key {} for member {}", key.id, key.member_id);
            println!();
            println!("{full_key}");
            println!();
            println!("This key is shown once and cannot be recovered — store it now.");
        }
        KeysCommand::Revoke { key_id } => {
            wardn::api_keys::revoke(&db.conn, &key_id).await?;
            println!("Revoked API key {key_id}");
        }
        KeysCommand::List => {
            for key in wardn::api_keys::list(&db.conn).await? {
                let label = key.label.as_deref().unwrap_or("-");
                let status = if key.revoked_at.is_some() {
                    "revoked"
                } else {
                    "active"
                };
                println!(
                    "{}\t{}\t{}\tmember={}\t{}",
                    key.id, key.key_prefix, label, key.member_id, status
                );
            }
        }
    }
    Ok(())
}

async fn cmd_status(db_path: &str) -> Result<()> {
    let db = Db::open(db_path).await?;
    let org = wardn::org::get(&db.conn).await?;
    let members = wardn::members::list(&db.conn).await?;

    println!("storage path: {db_path}");
    match &org {
        Some(org) => println!("org: {} ({})", org.name, org.id),
        None => println!("org: not initialized — run `wardn init`"),
    }
    println!("members: {}", members.len());

    let listen_addr = config::default_listen_addr();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()?;
    match client
        .get(format!("http://{listen_addr}/v1/status"))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            #[derive(serde::Deserialize)]
            struct Status {
                started_at: i64,
            }
            match resp.json::<Status>().await {
                Ok(status) => {
                    let uptime = wardn::db::now() - status.started_at;
                    println!("serve: running at {listen_addr} (uptime {uptime}s)");
                }
                Err(_) => println!("serve: running at {listen_addr}"),
            }
        }
        _ => println!("serve: not running (checked {listen_addr})"),
    }
    Ok(())
}

async fn cmd_serve(db_path: &str, listen: Option<String>) -> Result<()> {
    let db = Db::open(db_path).await?;
    if wardn::org::get(&db.conn).await?.is_none() {
        bail!("no org exists yet — run `wardn init` first");
    }
    let listen_addr = listen.unwrap_or_else(config::default_listen_addr);

    #[cfg_attr(not(feature = "observability"), allow(unused_mut))]
    let mut state = serve::AppState::new(db.conn, wardn::db::now());

    // Metrics stay off (no global recorder installed at all) unless
    // WARDN_METRICS_TOKEN is set — see src/config.rs and
    // src/observability.rs.
    #[cfg(feature = "observability")]
    if let Some(token) = config::observability::metrics_token() {
        let handle = metrics_exporter_prometheus::PrometheusBuilder::new()
            .install_recorder()
            .context("installing prometheus recorder")?;
        state = state.with_metrics(handle, token);
    }

    let listener = tokio::net::TcpListener::bind(&listen_addr)
        .await
        .with_context(|| format!("binding {listen_addr}"))?;
    tracing::info!("wardn serve listening on {listen_addr}");
    axum::serve(listener, wardn::app(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received, draining in-flight requests");
}
