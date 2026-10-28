use anyhow::{ensure, Result};
use clap::{Parser, Subcommand};
use simadmin_call_notify::{deliver, load_secret, observer, Config, Store};
use std::{path::PathBuf, time::Duration};
#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    CheckConfig {
        #[arg(long)]
        config: PathBuf,
    },
    Probe {
        #[arg(long)]
        config: PathBuf,
    },
    EnqueueTest {
        #[arg(long)]
        config: PathBuf,
    },
    Run {
        #[arg(long)]
        config: PathBuf,
    },
}
#[tokio::main]
async fn main() -> Result<()> {
    let a = Args::parse();
    let path = match &a.command {
        Command::CheckConfig { config }
        | Command::Probe { config }
        | Command::EnqueueTest { config }
        | Command::Run { config } => config,
    };
    let c = Config::load(path)?;
    match a.command {
        Command::CheckConfig { .. } => println!(
            "Configuration valid (offline); enabled={}; model={}; net={}",
            c.enabled, c.modem_model, c.network_device
        ),
        Command::Probe { .. } => {
            let conn = zbus::Connection::system().await?;
            let (_, calls) =
                tokio::time::timeout(Duration::from_secs(15), observer::snapshot(&conn, &c))
                    .await??;
            println!(
                "Read-only modem query succeeded; matching modem=1; incoming calls={}",
                calls.len()
            )
        }
        Command::EnqueueTest { .. } => {
            let mut s = Store::open(&c.data_dir)?;
            s.state.enqueue_test()?;
            s.save()?;
            println!("One clearly labelled test queued; start service to deliver")
        }
        Command::Run { .. } => {
            ensure!(c.enabled, "call notifier is disabled");
            run(c).await?
        }
    }
    Ok(())
}
async fn delivery_result(
    task: &mut Option<tokio::task::JoinHandle<bool>>,
) -> std::result::Result<bool, tokio::task::JoinError> {
    match task {
        Some(t) => t.await,
        None => std::future::pending().await,
    }
}
async fn run(c: Config) -> Result<()> {
    let secret = load_secret(&c.secret_file)?;
    let sender = simadmin_notify::Sender::new()?;
    let mut store = Store::open(&c.data_dir)?;
    store.save()?;
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let mut watcher = tokio::spawn(observer::watch(c, tx));
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut sending = None;
    let mut retry = tokio::time::Instant::now();
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    eprintln!("Incoming call notifier started; read-only ModemManager observer");
    let result: Result<()> = async {
        loop {
            tokio::select! {
                _ = terminate.recv() => break,
                _ = tokio::signal::ctrl_c() => break,
                result = &mut watcher => {
                    result??;
                    anyhow::bail!("call observer exited");
                },
                next = rx.recv() => {
                    let Some((scope, calls)) = next else { anyhow::bail!("call observation channel closed") };
                    let before = serde_json::to_vec(&store.state)?;
                    store.state.reconcile(&scope, &calls, chrono::Utc::now())?;
                    if before != serde_json::to_vec(&store.state)? { store.save()?; }
                },
                result = delivery_result(&mut sending), if sending.is_some() => {
                    sending = None;
                    if result? {
                        store.state.pending.pop_front();
                        store.save()?;
                        retry = tokio::time::Instant::now();
                        eprintln!("Call notification durably accepted by local bridge");
                    } else {
                        retry = tokio::time::Instant::now() + Duration::from_secs(5);
                        eprintln!("Call notification deferred; durable record retained");
                    }
                },
                _ = tick.tick() => {
                    if sending.is_none() && tokio::time::Instant::now() >= retry {
                        if let Some(n) = store.state.pending.front().cloned() {
                            let sender = sender.clone();
                            let secret = secret.clone();
                            sending = Some(tokio::spawn(async move {
                                deliver(&sender, &secret, "http://127.0.0.1:17866/rx/simadmin/json", &n).await.is_ok()
                            }));
                        }
                    }
                }
            }
        }
        Ok(())
    }.await;
    watcher.abort();
    if let Some(t) = sending {
        t.abort();
    }
    result
}
