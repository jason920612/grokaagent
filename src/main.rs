use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use grokaagent::auth;
use grokaagent::events::JsonlSink;
use grokaagent::kit::{self, KernelSpec};
use grokaagent::provider::{ReasoningEffort, XaiOauthProvider};
use grokaagent::tui::{self, TuiOptions};
use grokaagent::worker::{self, WorkerConfig, WorkerMode};

#[derive(Parser)]
#[command(name = "grokaagent", version, about = "Event-loop agent with xAI Grok OAuth and A2A workers")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Copy this binary to ~/.cargo/bin so `grokaagent` works from any directory.
    Install,
    /// Sign in to xAI with the device-code OAuth flow.
    Login,
    /// Delete the saved xAI OAuth tokens.
    Logout,
    /// Run one agent turn-loop against Grok (headless).
    Run {
        prompt: Option<String>,
        #[arg(long, default_value = "grok-4.6")]
        model: String,
        #[arg(long, default_value = "groka-events.jsonl")]
        events: PathBuf,
        #[arg(long, default_value_t = 0, help = "0 = unlimited (default)")]
        max_turns: u32,
        #[arg(long)]
        web_search: bool,
        #[arg(long)]
        workspace: Option<PathBuf>,
        #[arg(long, default_value = "high", value_parser = parse_reasoning)]
        reasoning: ReasoningEffort,
    },
    /// Interactive terminal UI.
    Tui {
        #[arg(long, default_value = "grok-4.6")]
        model: String,
        #[arg(long, default_value = "groka-events.jsonl")]
        events: PathBuf,
        #[arg(long, default_value_t = 0, help = "0 = unlimited (default)")]
        max_turns: u32,
        /// Disable server-side web_search and x_search (on by default).
        #[arg(long = "no-web-search", default_value_t = false)]
        no_web_search: bool,
        #[arg(long)]
        workspace: Option<PathBuf>,
        #[arg(long, default_value = "high", value_parser = parse_reasoning)]
        reasoning: ReasoningEffort,
    },
    /// A2A worker (spawned by a parent; handshake JSON on the first stdout line).
    Worker {
        #[arg(long)]
        name: String,
        #[arg(long, default_value_t = 1)]
        depth: u32,
        #[arg(long)]
        parent_run: Option<String>,
        #[arg(long, default_value = "127.0.0.1:0")]
        listen: String,
        #[arg(long, default_value = "groka-events.jsonl")]
        events: PathBuf,
        #[arg(long, default_value = "grok")]
        mode: String,
        #[arg(long)]
        workspace: Option<PathBuf>,
        #[arg(long, default_value = "grok-4.6")]
        model: String,
        #[arg(long, default_value_t = 8)]
        max_turns: u32,
        #[arg(long, default_value = "high", value_parser = parse_reasoning)]
        reasoning: ReasoningEffort,
    },
}

fn parse_reasoning(s: &str) -> Result<ReasoningEffort, String> {
    ReasoningEffort::parse(s).ok_or_else(|| {
        format!("unknown --reasoning `{s}` (low|medium|high|xhigh)")
    })
}

#[tokio::main]
async fn main() {
    if let Err(e) = real_main().await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn real_main() -> grokaagent::Result<()> {
    let cli = Cli::parse();
    let auth_path = auth::default_auth_path()?;
    match cli.command {
        None => {
            tui::run_tui(TuiOptions {
                model: "grok-4.6".into(),
                events: PathBuf::from("groka-events.jsonl"),
                workspace: std::env::current_dir()?,
                max_turns: 0,
                web_search: true,
                reasoning_effort: ReasoningEffort::High,
            })
            .await?;
        }
        Some(Command::Install) => {
            let dest = grokaagent::install::install_current_exe()?;
            let bin_dir = dest.parent().unwrap_or(dest.as_path());
            println!("installed {}", dest.display());
            if grokaagent::install::path_contains(bin_dir) {
                println!("open a new terminal if this shell still cannot find grokaagent");
            } else {
                println!(
                    "add this directory to PATH, then open a new terminal:\n  {}",
                    bin_dir.display()
                );
            }
        }
        Some(Command::Login) => {
            auth::login(&auth_path).await?;
        }
        Some(Command::Logout) => {
            auth::delete_auth_file(&auth_path)?;
            println!("logged out");
        }
        Some(Command::Tui {
            model,
            events,
            max_turns,
            no_web_search,
            workspace,
            reasoning,
        }) => {
            tui::run_tui(TuiOptions {
                model,
                events,
                workspace: match workspace {
                    Some(p) => p,
                    None => std::env::current_dir()?,
                },
                max_turns,
                web_search: !no_web_search,
                reasoning_effort: reasoning,
            })
            .await?;
        }
        Some(Command::Run {
            prompt,
            model,
            events,
            max_turns,
            web_search,
            workspace,
            reasoning,
        }) => {
            let prompt = match prompt {
                Some(p) => p,
                None => {
                    let mut buf = String::new();
                    io::stdin().read_to_string(&mut buf)?;
                    buf
                }
            };
            let prompt = prompt.trim().to_string();
            if prompt.is_empty() {
                return Err(grokaagent::Error::EmptyPrompt);
            }
            let workspace = match workspace {
                Some(p) => p,
                None => std::env::current_dir()?,
            };
            let sink: Arc<dyn grokaagent::events::EventSink> = Arc::new(JsonlSink::create(&events)?);
            let provider = XaiOauthProvider::new(auth_path, Some(model.clone()))?;
            let server_tools = kit::search_tools(web_search);
            let outcome = kit::run_with_nursery(
                &provider,
                sink,
                KernelSpec {
                    agent_name: "root".into(),
                    prompt,
                    model,
                    max_turns,
                    workspace,
                    events_file: events.clone(),
                    events_dir: kit::events_dir(&events),
                    server_tools,
                    depth: 0,
                    parent_run_id: None,
                    run_id: String::new(),
                    child_mode: std::env::var("GROKA_CHILD_MODE")
                        .unwrap_or_else(|_| "grok".into()),
                    reasoning_effort: reasoning,
                    knobs: None,
                    inbox: None,
                    images: Vec::new(),
                    ask: None,
                    cancel: None,
                },
            )
            .await?;
            println!("{}", outcome.text);
            for (i, u) in outcome.cache_turns.iter().enumerate() {
                eprintln!(
                    "turn {} cache {}/{} ({:.0}%)",
                    i + 1,
                    u.cached_tokens,
                    u.input_tokens,
                    u.rate() * 100.0
                );
            }
            if outcome.compacted > 0 {
                eprintln!("compacted {} time(s)", outcome.compacted);
            }
            eprintln!(
                "run {} finished in {} turn(s); events -> {}",
                outcome.run_id,
                outcome.turns,
                events.display()
            );
        }
        Some(Command::Worker {
            name,
            depth,
            parent_run,
            listen,
            events,
            mode,
            workspace,
            model,
            max_turns,
            reasoning,
        }) => {
            worker::run_worker(WorkerConfig {
                name,
                depth,
                parent_run_id: parent_run,
                listen,
                events,
                mode: WorkerMode::parse(&mode),
                workspace: match workspace {
                    Some(p) => p,
                    None => std::env::current_dir()?,
                },
                model,
                max_turns,
                reasoning_effort: reasoning,
            })
            .await?;
        }
    }
    Ok(())
}
