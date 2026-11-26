//! The command line, and the commands a running peer takes on stdin.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::events::emit;
use crate::peer::{Peer, Settings};
use crate::state::StateDir;

/// A scripted Paykit v0.2 wallet for local tests: one identity with an in-process signer, the
/// local Dispatch, the fake Homeserver and an on-chain rail on the local regtest bitcoind.
///
/// Every address must be loopback. Events are JSON lines on stdout; logs go to stderr.
#[derive(Debug, Parser)]
#[command(name = "paykit-peer", version)]
pub struct Cli {
    /// State directory (identity secret in plain text, stores, rail history). Without it the
    /// peer lives in memory, and only `run` is useful.
    #[arg(long, global = true)]
    pub state: Option<PathBuf>,
    /// The Dispatch's plain listener on loopback (the CORS proxy or the API port).
    #[arg(long, global = true, default_value = "http://127.0.0.1:18580")]
    pub dispatch_url: String,
    /// The Dispatch's https origin, which requests are signed for.
    #[arg(long, global = true, default_value = "https://localhost:18580")]
    pub origin: String,
    /// The fake Homeserver.
    #[arg(long, global = true, default_value = "http://127.0.0.1:18590")]
    pub homeserver: String,
    /// bitcoind's JSON-RPC (regtest).
    #[arg(long, global = true, default_value = "http://127.0.0.1:18443")]
    pub bitcoind_url: String,
    #[arg(long, global = true, default_value = "admin1")]
    pub rpc_user: String,
    #[arg(long, global = true, default_value = "123")]
    pub rpc_password: String,
    /// The bitcoind wallet the peer pays from and receives to; default
    /// `paykit-peer-<first 12 characters of the Pubky ID>`, chosen at `init`.
    #[arg(long, global = true)]
    pub wallet: Option<String>,
    /// The bitcoind wallet `fund` sends from (mining to it when it is short).
    #[arg(long, global = true, default_value = "default")]
    pub funder_wallet: String,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create the identity, enrol it at the Dispatch, approve and join the Paykit app.
    Init {
        /// Also fund the peer's bitcoind wallet with this many satoshis.
        #[arg(long)]
        fund: Option<u64>,
    },
    /// Print the identity's Pubky ID (z-base-32, no prefix), alone on one line.
    Id,
    /// Fund the peer's bitcoind wallet up to at least this many satoshis.
    Fund { sats: u64 },
    /// Allow this identity to start a conversation (admission).
    Allow { id: String },
    /// Request `sats` from identity `id`, to a new address, or with `--any` to any of this
    /// identity's Private Payment Lists.
    Request {
        id: String,
        sats: u64,
        #[arg(long)]
        any: bool,
        /// The payment reference; random by default.
        #[arg(long)]
        reference: Option<String>,
    },
    /// Set this app's Private Payment List to a new address and publish it to `id`.
    PublishList { id: String },
    /// Run: receive, print events, and take commands on stdin (`allow <id>`, `request <id>
    /// <sats> [--any]`, `publish-list <id>`, `fund <sats>`, `id`, `quit`).
    Run {
        /// Pay every open one-time request that arrives.
        #[arg(long)]
        auto_pay: bool,
        /// Verify every proof that arrives and send its receipt.
        #[arg(long)]
        auto_confirm: bool,
        /// Fund the wallet with this many satoshis first.
        #[arg(long)]
        fund: Option<u64>,
        /// How long to wait between rounds, in ms.
        #[arg(long, default_value_t = 1000)]
        poll_ms: u64,
    },
}

/// A command line read from a running peer's stdin.
#[derive(Debug, Parser)]
#[command(name = "", no_binary_name = true)]
pub struct Line {
    #[command(subcommand)]
    pub command: Command,
}

/// The stack of the thread the peer runs on: Paykit's futures are large.
const STACK_BYTES: usize = 32 << 20;

pub fn main() -> ExitCode {
    let cli = Cli::parse();
    let thread = std::thread::Builder::new()
        .name("paykit-peer".into())
        .stack_size(STACK_BYTES)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| anyhow::anyhow!("could not start the runtime: {error}"))?;
            runtime.block_on(Box::pin(run(cli)))
        });
    let result = match thread {
        Ok(thread) => thread
            .join()
            .unwrap_or_else(|_| Err(anyhow::anyhow!("the peer panicked"))),
        Err(error) => Err(anyhow::anyhow!(
            "could not start the peer's thread: {error}"
        )),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("paykit-peer: {error:#}");
            emit(
                "fatal",
                serde_json::json!({ "error": format!("{error:#}") }),
            );
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let settings = Settings {
        dispatch_url: cli.dispatch_url,
        origin: cli.origin,
        homeserver: cli.homeserver,
        bitcoind_url: cli.bitcoind_url,
        rpc_user: cli.rpc_user,
        rpc_password: cli.rpc_password,
        wallet: cli.wallet,
        funder_wallet: cli.funder_wallet,
    };
    let dir = match &cli.state {
        Some(path) => Some(StateDir::open(path)?),
        None => {
            if !matches!(cli.command, Command::Run { .. }) {
                anyhow::bail!(
                    "without --state every start is a new identity: use --state <dir>, or `run` \
                     and send commands on its stdin"
                );
            }
            None
        }
    };
    if let (Command::Id, Some(dir)) = (&cli.command, &dir) {
        // Read from the state directory alone: nothing is contacted.
        let config = dir
            .load_config()?
            .ok_or_else(|| anyhow::anyhow!("no identity in this state directory: run init"))?;
        println!("{}", config.identity);
        return Ok(());
    }
    let mut peer = Box::pin(Peer::open(settings, dir)).await?;
    let result = match cli.command {
        Command::Run {
            auto_pay,
            auto_confirm,
            fund,
            poll_ms,
        } => {
            if let Some(sats) = fund {
                Box::pin(peer.fund(sats)).await?;
            }
            Box::pin(peer.run(auto_pay, auto_confirm, poll_ms)).await
        }
        other => Box::pin(peer.command(other)).await,
    };
    let closed = Box::pin(peer.close()).await;
    result.and(closed)
}
