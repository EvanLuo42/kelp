use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;

/// How many days of logs to keep.
const LOG_DAYS: usize = 7;

#[derive(Debug, Parser)]
#[command(version, about = "Kelp per-user daemon")]
struct Args {
    /// The Kelp home directory; defaults to `KELP_HOME` or the platform default.
    #[arg(long)]
    home: Option<PathBuf>,
    /// Exits after this many minutes without requests; 0 runs forever.
    #[arg(long, default_value_t = 30)]
    idle_minutes: u64,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let home = args
        .home
        .or_else(|| kelp_client::Home::default_location().map(|h| h.dir().to_owned()));
    let Some(home) = home else {
        eprintln!("kelpd: cannot find the Kelp home directory; pass --home");
        std::process::exit(1);
    };
    let flush = init_logging(&home);
    let idle_timeout = (args.idle_minutes > 0).then(|| Duration::from_secs(args.idle_minutes * 60));
    if let Err(e) = kelp_daemon::serve(kelp_daemon::Config { home, idle_timeout }).await {
        tracing::error!("{e}");
        // Exiting skips destructors, so the log must be flushed first.
        drop(flush);
        eprintln!("kelpd: {e}");
        std::process::exit(1);
    }
}

/// Logs to `logs/kelpd.log.<date>` in `home`, since the kelp command starts
/// kelpd without a terminal, or to standard error if that cannot be opened.
/// `KELP_LOG` or else `RUST_LOG` sets the filter, which defaults to `info`.
/// The returned guard flushes the log when dropped.
fn init_logging(home: &Path) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("KELP_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let dir = home.join("logs");
    let file = std::fs::create_dir_all(&dir)
        .map_err(|e| e.to_string())
        .and_then(|()| {
            tracing_appender::rolling::Builder::new()
                .rotation(tracing_appender::rolling::Rotation::DAILY)
                .filename_prefix("kelpd.log")
                .max_log_files(LOG_DAYS)
                .build(&dir)
                .map_err(|e| e.to_string())
        });
    match file {
        Ok(file) => {
            let (writer, guard) = tracing_appender::non_blocking(file);
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(writer)
                .init();
            Some(guard)
        }
        Err(e) => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .init();
            tracing::warn!("cannot write logs into {}: {e}", home.display());
            None
        }
    }
}
