//! Rust MCP application; protocol, presentation and deployment have separate boundaries.
mod notify;
mod response;
mod service;
mod tools;
// The contract modules keep a few API surfaces no current call site constructs
// (unused `NotImplemented` codes, `Probe::Incomplete`); they belong to those
// modules, so the lint is allowed here until their owners trim them.
#[allow(dead_code, reason = "Unused contract surface in git/store/worktree")]
mod git;
#[allow(dead_code, reason = "Unused contract surface in git/store/worktree")]
mod store;
#[allow(dead_code, reason = "Unused contract surface in git/store/worktree")]
mod worktree;
use clap::{Parser, Subcommand};
use mcp_presentation::Renderer;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt, model::*, service::RequestContext,
};
use std::{io, io::Write as _, path::PathBuf, process::ExitCode, sync::Arc};

#[derive(Parser)]
#[command(
    version,
    about = "MCP server that keeps Git worktrees tidy for an orchestrating agent"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}
#[derive(Subcommand)]
enum Commands {
    /// Run the protocol-only stdio endpoint.
    Mcp,
    /// Check local resources, without credentials, network or mutation.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Export the actual registry or inspect unimplemented tool stubs.
    Contract {
        #[command(subcommand)]
        command: ContractCommand,
    },
    /// Install a verified single-binary bundle. Does not restart services.
    SelfInstall {
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        home: PathBuf,
        #[arg(long)]
        bin_dir: PathBuf,
    },
    /// Select a retained compatible installation.
    Releases {
        #[command(subcommand)]
        command: ReleaseCommand,
    },
    /// Host hook surface: context injection for `UserPromptSubmit`.
    Hook {
        #[command(subcommand)]
        command: HookCommand,
    },
}

/// Host hook subcommands.
#[derive(Subcommand)]
enum HookCommand {
    /// Print idle-worktree context for a `UserPromptSubmit` hook.
    ///
    /// Always exits 0 and prints nothing on any internal error; emits one
    /// bounded `<agent-worktree>` block per new 24 h idle episode,
    /// rate-limited to one scan per 10 minutes.
    Context {
        /// Host harness the hook runs under. Both hosts consume the same
        /// `additionalContext` JSON envelope `agent-run hook context` emits.
        #[arg(long, default_value = "claude")]
        host: Host,
    },
}

/// Host harness selection for `hook context`.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum Host {
    /// Claude Code.
    Claude,
    /// Codex.
    Codex,
}

impl Host {
    /// Stable lowercase name used in hook diagnostics.
    fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}
#[derive(Subcommand)]
enum ContractCommand {
    Export,
    Readiness,
}
#[derive(Subcommand)]
enum ReleaseCommand {
    Use {
        version: String,
        #[arg(long)]
        home: PathBuf,
        #[arg(long)]
        bin_dir: PathBuf,
    },
}
#[derive(Clone)]
struct Handler {
    identity: Arc<Renderer>,
    templates: Arc<response::Templates>,
    service: Arc<service::Service>,
    catalog: Vec<Tool>,
}
impl Handler {
    fn new() -> Result<Self, &'static str> {
        let identity = Renderer::new().map_err(|_| "presentation_setup_failed")?;
        let templates = response::Templates::new(&tools::templates())?;
        let service = Arc::new(service::Service::new().map_err(|_| "policy_invalid")?);
        let catalog = serde_json::from_value(serde_json::Value::Array(tools::definitions()))
            .map_err(|_| "catalog_invalid")?;
        Ok(Self {
            identity: Arc::new(identity),
            templates: Arc::new(templates),
            service,
            catalog,
        })
    }
}
/// Private catalog cache lifetime in milliseconds for modern MCP requests.
const TOOLS_LIST_TTL_MS: u64 = 60_000;
impl ServerHandler for Handler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")))
            .with_instructions("Tools: get_status, create_worktree, list_worktrees, inspect_worktree, remove_worktree, prune_worktrees; tool descriptions define effects. Removal is always preview → fingerprint → apply and refuses anything not provably clean. Git is never forced and branches are never deleted.")
    }
    /// Return the static catalog; modern requests cache it privately for 60 seconds.
    /// Pagination is unused; legacy sessions retain their original wire fields.
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let mut result = ListToolsResult::with_all_items(self.catalog.clone());
        if context
            .protocol_version()
            .is_some_and(|version| version.as_str() >= ProtocolVersion::V_2026_07_28.as_str())
        {
            result = result
                .with_ttl_ms(TOOLS_LIST_TTL_MS)
                .with_cache_scope(CacheScope::Private);
        }
        Ok(result)
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let args = serde_json::Value::Object(request.arguments.unwrap_or_default());
        let reply = tools::call(
            &request.name,
            args,
            &self.identity,
            &self.templates,
            &self.service,
        )
        .await
        .ok_or_else(|| McpError::new(ErrorCode::METHOD_NOT_FOUND, "Unknown tool", None))?;
        Ok(reply.into())
    }
}
#[allow(clippy::print_stdout, reason = "Explicit CLI branch, never MCP output")]
fn print_json(value: impl serde::Serialize) -> ExitCode {
    match serde_json::to_string_pretty(&value) {
        Ok(text) => {
            // A closed pipe (EPIPE) is ignored, never a panic: CLI output is
            // best-effort.
            let _ = writeln!(io::stdout().lock(), "{text}");
            ExitCode::SUCCESS
        }
        Err(_) => {
            eprintln!("CLI serialization failed");
            ExitCode::FAILURE
        }
    }
}

/// Release qualification recorded in the embedded family manifest, so
/// `doctor` and `get_status` report the same value the release tooling reads.
/// An unreadable manifest fails closed to `not_verified`.
fn qualification() -> &'static str {
    static QUALIFICATION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    QUALIFICATION.get_or_init(|| {
        toml::from_str::<toml::Value>(include_str!("../family.toml"))
            .ok()
            .and_then(|family| {
                family
                    .get("qualification")
                    .and_then(toml::Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "not_verified".to_owned())
    })
}

/// Prints one `UserPromptSubmit` context envelope, mirroring the shape
/// `agent-run hook context` emits for both Claude Code and Codex; an empty
/// `text` prints nothing at all. Write failures (EPIPE) are ignored — the
/// hook must always exit 0 quietly.
#[allow(
    clippy::print_stdout,
    reason = "Explicit hook branch, never MCP output"
)]
fn print_context(text: &str) {
    let envelope = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": text
        }
    });
    let _ = writeln!(io::stdout().lock(), "{envelope}");
}
#[tokio::main]
async fn main() -> ExitCode {
    match Cli::parse().command {
        Commands::SelfInstall {
            bundle,
            home,
            bin_dir,
        } => match family_delivery::install(&bundle, &home, &bin_dir) {
            Ok(m) => print_json(m),
            Err(e) => {
                eprintln!("install: {e}");
                ExitCode::FAILURE
            }
        },
        Commands::Releases {
            command:
                ReleaseCommand::Use {
                    version,
                    home,
                    bin_dir,
                },
        } => match family_delivery::use_version(&home, &bin_dir, &version) {
            Ok(m) => print_json(m),
            Err(e) => {
                eprintln!("activate: {e}");
                ExitCode::FAILURE
            }
        },
        Commands::Doctor { json: _ } => {
            let ready = Handler::new().is_ok();
            // The layout facts doctor reports: home, config path, the
            // configured worktree root (there is no default) and the
            // discovery roots, all after `~/` expansion.
            let env_home = std::env::var_os("AGENT_WORKTREE_HOME").map(PathBuf::from);
            let platform_home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/"));
            let home = env_home
                .clone()
                .unwrap_or_else(|| platform_home.join(".agent-worktree"));
            let layout = store::resolve_layout(env_home, platform_home);
            let (root, discovery_roots) = match &layout {
                Ok(layout) => (
                    layout
                        .root
                        .as_ref()
                        .map_or("not configured".to_owned(), |root| {
                            root.display().to_string()
                        }),
                    layout
                        .config
                        .discovery_roots
                        .iter()
                        .map(|root| root.display().to_string())
                        .collect::<Vec<_>>(),
                ),
                Err(error) => (format!("invalid_config: {}", error.detail), Vec::new()),
            };
            let output = print_json(
                serde_json::json!({"product":env!("CARGO_PKG_NAME"),"version":env!("CARGO_PKG_VERSION"),
                "home":home.display().to_string(),"config":home.join("config.toml").display().to_string(),
                  "worktree_root":root,"discovery_roots":discovery_roots,
                "local_ready":ready,"release_qualification":qualification(),"incomplete_tools":tools::incomplete()}),
            );
            if ready { output } else { ExitCode::from(2) }
        }
        Commands::Contract {
            command: ContractCommand::Export,
        } => match Handler::new() {
            Ok(h) => print_json(h.catalog),
            Err(e) => {
                eprintln!("contract: {e}");
                ExitCode::FAILURE
            }
        },
        Commands::Contract {
            command: ContractCommand::Readiness,
        } => print_json(tools::incomplete()),
        Commands::Hook {
            command: HookCommand::Context { host },
        } => {
            // The hook must never break the host: every outcome is a silent exit 0.
            let text = notify::hook_context(host.as_str()).await;
            if !text.is_empty() {
                print_context(&text);
            }
            ExitCode::SUCCESS
        }
        Commands::Mcp => {
            let h = match Handler::new() {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("startup: {e}");
                    return ExitCode::FAILURE;
                }
            };
            match h.serve(rmcp::transport::stdio()).await {
                Ok(service) => match service.waiting().await {
                    Ok(_) => ExitCode::SUCCESS,
                    Err(_) => ExitCode::FAILURE,
                },
                Err(_) => {
                    eprintln!("MCP transport failed");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
