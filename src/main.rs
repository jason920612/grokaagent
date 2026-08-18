use std::io::{self, Read};
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use grokaagent::auth;
use grokaagent::config::{ProviderConfig, ProviderKind};
use grokaagent::events::JsonlSink;
use grokaagent::kit::{self, KernelSpec};
use grokaagent::provider::{AnyProvider, ReasoningEffort};
use grokaagent::tui::{self, TuiOptions};
use grokaagent::worker::{self, WorkerConfig, WorkerMode};

#[derive(Parser)]
#[command(name = "grokaagent", version, about = "Event-loop agent with xAI Grok OAuth or an OpenAI-compatible API")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Args, Clone, Debug, Default)]
struct ApiArgs {
    /// OpenAI-compatible base URL, e.g. http://host:port/v1
    #[arg(long)]
    base_url: Option<String>,
    /// Bearer token for a custom endpoint (optional for some local servers).
    #[arg(long)]
    api_key: Option<String>,
    /// Context window, e.g. 262K or 262144
    #[arg(long)]
    context: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Copy this binary to ~/.cargo/bin so `grokaagent` works from any directory.
    Install,
    /// Download the latest GitHub Release and replace this binary.
    Update,
    /// Sign in to xAI with the device-code OAuth flow.
    Login,
    /// Delete the saved xAI OAuth tokens.
    Logout,
    /// Run one agent turn-loop (headless).
    Run {
        prompt: Option<String>,
        #[arg(long)]
        model: Option<String>,
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
        #[command(flatten)]
        api: ApiArgs,
    },
    /// Interactive terminal UI.
    Tui {
        #[arg(long)]
        model: Option<String>,
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
        #[command(flatten)]
        api: ApiArgs,
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
        #[arg(long)]
        model: Option<String>,
        #[arg(long, default_value_t = 8)]
        max_turns: u32,
        #[arg(long, default_value = "high", value_parser = parse_reasoning)]
        reasoning: ReasoningEffort,
        #[command(flatten)]
        api: ApiArgs,
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

fn wants_auto_update(cmd: &Option<Command>) -> bool {
    matches!(cmd, None | Some(Command::Tui { .. }))
}

fn boot_provider(
    api: ApiArgs,
    model: Option<String>,
    persist: bool,
) -> grokaagent::Result<(ProviderConfig, String)> {
    let mut cfg = ProviderConfig::load();
    let override_set = api.base_url.is_some()
        || api.api_key.is_some()
        || api.context.is_some()
        || model.is_some();
    cfg.apply_cli(
        api.base_url,
        api.api_key,
        api.context.as_deref(),
        model.as_deref(),
    );
    if persist && override_set {
        let _ = cfg.save();
    }
    let model = cfg.effective_model().to_string();
    if cfg.route().is_openai() && model.is_empty() {
        return Err(grokaagent::Error::Provider(
            "custom API needs --model (or GROKA_MODEL / saved provider.json)".into(),
        ));
    }
    Ok((cfg, model))
}

async fn real_main() -> grokaagent::Result<()> {
    grokaagent::install::cleanup_old_exe();
    let cli = Cli::parse();
    if wants_auto_update(&cli.command) {
        match grokaagent::update::auto_update().await {
            grokaagent::update::Outcome::Updated { from, to } => {
                eprintln!("updated grokaagent {from} -> {to}; relaunching");
                grokaagent::update::reexec()?;
            }
            grokaagent::update::Outcome::UpToDate { .. }
            | grokaagent::update::Outcome::Skipped { .. } => {}
        }
    }
    let auth_path = auth::default_auth_path()?;
    match cli.command {
        None => {
            let (cfg, model) = boot_provider(ApiArgs::default(), None, false)?;
            tui::run_tui(TuiOptions {
                model,
                events: PathBuf::from("groka-events.jsonl"),
                workspace: std::env::current_dir()?,
                max_turns: 0,
                web_search: cfg.kind != ProviderKind::Openai,
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
        Some(Command::Update) => {
            match grokaagent::update::update_now().await? {
                grokaagent::update::Outcome::Updated { from, to } => {
                    println!("updated {from} -> {to}");
                }
                grokaagent::update::Outcome::UpToDate { version } => {
                    println!("already up to date ({version})");
                }
                grokaagent::update::Outcome::Skipped { reason } => {
                    println!("update skipped: {reason}");
                }
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
            api,
        }) => {
            let (cfg, model) = boot_provider(api, model, true)?;
            tui::run_tui(TuiOptions {
                model,
                events,
                workspace: match workspace {
                    Some(p) => p,
                    None => std::env::current_dir()?,
                },
                max_turns,
                web_search: !no_web_search && cfg.kind != ProviderKind::Openai,
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
            api,
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
            let (cfg, model) = boot_provider(api, model, false)?;
            let sink: Arc<dyn grokaagent::events::EventSink> = Arc::new(JsonlSink::create(&events)?);
            let provider = AnyProvider::connect(&cfg, Some(model.clone()))?;
            let server_tools = if cfg.route().is_openai() {
                vec![]
            } else {
                kit::search_tools(web_search)
            };
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
                    context_window: cfg.window_tokens(),
                    cancel: None,
                    skills: None,
                    task: None,
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
            api,
        }) => {
            let (_, model) = boot_provider(api, model, false)?;
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
