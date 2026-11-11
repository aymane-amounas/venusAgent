use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use repository::{inspect, normalize_workspace};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;
use venus_agent::{
    capability::CapabilityRequest,
    config::{AppConfig, AppPaths},
    database::Database,
    orchestrator::{CapabilityOrchestrator, CapabilityRunResult, register_workspace_capabilities},
    permissions::{PermissionGrantDraft, PermissionProfile},
    provider::{self, ChatMessage, ChatRequest, ChatRole},
    repository,
};
mod setup;
mod tui;

#[derive(Debug, Parser)]
#[command(
    name = env!("CARGO_BIN_NAME"),
    version,
    about = "Venus Agent, a user-controlled Rust AI agent runtime for terminal chat and permission-aware capabilities"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Register a workspace and initialize local application state.
    Init {
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
    },
    /// Configure a model provider and model with an interactive first-run wizard.
    Setup,
    /// Inspect repository files and report the current Git status.
    Inspect {
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
    },
    /// Manage a persistent task list for a registered workspace.
    Task {
        #[command(subcommand)]
        command: TaskCommands,
    },
    /// Check a provider or list the models it exposes.
    Provider {
        #[command(subcommand)]
        command: ProviderCommands,
    },
    /// Inspect and change capability permission policy.
    Permission {
        #[command(subcommand)]
        command: PermissionCommands,
    },
    /// Discover and run bounded workspace capabilities.
    Capability {
        #[command(subcommand)]
        command: CapabilityCommands,
    },
    /// Send a prompt to one configured provider and stream its response.
    Ask {
        #[arg(long)]
        provider: String,
        #[arg(long)]
        model: String,
        prompt: String,
    },
}

#[derive(Debug, Subcommand)]
enum TaskCommands {
    /// Add a task to the workspace.
    Add {
        description: String,
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
    },
    /// List workspace tasks.
    List {
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
    },
    /// Mark a pending task complete.
    Complete {
        id: i64,
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum ProviderCommands {
    /// List configured providers and their credential status.
    List,
    /// Add a provider and select a model interactively.
    Add,
    /// Check provider reachability and discover models.
    Check {
        name: String,
    },
    Models {
        name: String,
    },
    /// Test actual inference with a configured model.
    Test {
        name: String,
        #[arg(long)]
        model: String,
    },
    /// Make a configured model the default for new chats.
    Switch {
        name: String,
        model: String,
    },
    /// Remove a provider and its model entries.
    Remove {
        name: String,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
enum PermissionCommands {
    /// Show or change the execution profile.
    Profile { profile: Option<String> },
    /// Add a capability and operation-scoped user grant.
    Grant {
        capability_id: String,
        capability_version: String,
        operation: String,
        #[arg(long = "permission")]
        permissions: Vec<String>,
        #[arg(long)]
        requires_approval: bool,
        #[arg(long)]
        expires_in_seconds: Option<i64>,
    },
    /// List existing and revoked permission grants.
    Grants,
    /// Revoke a permission grant by ID.
    Revoke { grant_id: i64 },
    /// List request-scoped approvals.
    Approvals,
    /// Approve one pending request for its exact arguments.
    Approve { approval_id: i64 },
    /// Deny one pending request.
    Deny { approval_id: i64 },
    /// Revoke one pending or previously approved request.
    RevokeApproval { approval_id: i64 },
    /// Inspect capability audit events (metadata only).
    Audit {
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
}

#[derive(Debug, Subcommand)]
enum CapabilityCommands {
    /// List available bounded workspace capabilities.
    List {
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
    },
    /// Propose and, if authorized, run a capability.
    Run {
        capability_id: String,
        operation: String,
        #[arg(long, default_value = ".")]
        workspace: PathBuf,
        /// JSON input for file-read and search operations.
        #[arg(long, value_name = "JSON")]
        input: Option<String>,
        #[arg(long)]
        approval_id: Option<i64>,
    },
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    let paths = AppPaths::new()?;

    let Some(command) = cli.command else {
        let mut config = AppConfig::load(&paths)?;
        initialize_logging(&config)?;
        if config.providers.is_empty() || config.models.is_empty() {
            config = setup::run(&paths, config).await?;
        }
        if config.providers.is_empty() || config.models.is_empty() {
            bail!("No provider and model are configured. Run `venus setup` when ready.");
        }
        let workspace = normalize_workspace(&PathBuf::from("."))?;
        let database = open_database(&config)?;
        database.register_workspace(&workspace)?;
        tui::run(config, database, workspace).await?;
        return Ok(());
    };

    match command {
        Commands::Init { workspace } => {
            paths.initialize_config()?;
            let config = AppConfig::load(&paths)?;
            initialize_logging(&config)?;
            let root = normalize_workspace(&workspace)?;
            let database = open_database(&config)?;
            database.register_workspace(&root)?;
            let profile = database.permission_profile()?;
            println!("Initialized workspace: {}", root.display());
            println!("Configuration: {}", paths.config_path.display());
            println!("Database: {}", config.database_path.display());
            println!("Permission profile: {}", profile.as_str());
            println!("{}", profile_explanation(profile));
            println!(
                "Bounded workspace summary/read/search and exact text replacement capabilities are available."
            );
            println!("Run `venus-agent setup` to connect a model, then `venus-agent` to chat.");
        }
        Commands::Setup => {
            let config = AppConfig::load(&paths)?;
            initialize_logging(&config)?;
            setup::run(&paths, config).await?;
        }
        Commands::Inspect { workspace } => {
            let config = AppConfig::load(&paths)?;
            initialize_logging(&config)?;
            let root = normalize_workspace(&workspace)?;
            inspect(&root)?;
        }
        Commands::Task { command } => {
            let config = AppConfig::load(&paths)?;
            initialize_logging(&config)?;
            let database = open_database(&config)?;
            match command {
                TaskCommands::Add {
                    description,
                    workspace,
                } => {
                    let root = normalize_workspace(&workspace)?;
                    let id = database.add_task(&root, &description)?;
                    println!("Added task #{id}: {description}");
                }
                TaskCommands::List { workspace } => {
                    let root = normalize_workspace(&workspace)?;
                    let tasks = database.list_tasks(&root)?;
                    if tasks.is_empty() {
                        println!("No tasks for {}", root.display());
                    } else {
                        for task in tasks {
                            println!(
                                "#{} [{}] {} (created {})",
                                task.id, task.status, task.description, task.created_at
                            );
                        }
                    }
                }
                TaskCommands::Complete { id, workspace } => {
                    let root = normalize_workspace(&workspace)?;
                    if !database.complete_task(&root, id)? {
                        bail!("Task #{id} does not exist or is not pending.");
                    }
                    println!("Completed task #{id}.");
                }
            }
        }
        Commands::Provider { command } => {
            let mut config = AppConfig::load(&paths)?;
            initialize_logging(&config)?;
            match command {
                ProviderCommands::List => {
                    if config.providers.is_empty() {
                        println!(
                            "No providers configured. Run `venus provider add` to set one up."
                        );
                    }
                    for configured in &config.providers {
                        let model_count = config
                            .models
                            .iter()
                            .filter(|model| model.provider == configured.name)
                            .count();
                        let credential = configured
                            .api_key_env
                            .as_deref()
                            .map(|variable| {
                                if std::env::var(variable).is_ok_and(|value| !value.is_empty()) {
                                    format!("credential={variable}:set")
                                } else {
                                    format!("credential={variable}:missing")
                                }
                            })
                            .unwrap_or_else(|| "credential=not-required".to_owned());
                        let default = config
                            .models
                            .first()
                            .is_some_and(|model| model.provider == configured.name);
                        println!(
                            "{}{} kind={:?} models={} {}",
                            if default { "* " } else { "" },
                            configured.name,
                            configured.kind,
                            model_count,
                            credential
                        );
                    }
                }
                ProviderCommands::Add => {
                    let old_providers = config.providers.len();
                    let old_models = config.models.len();
                    config = setup::run(&paths, config).await?;
                    if config.providers.len() == old_providers && config.models.len() == old_models
                    {
                        bail!("No provider or model was added.");
                    }
                }
                ProviderCommands::Remove { name, yes } => {
                    if !yes {
                        bail!(
                            "Removing a provider also removes its model entries. Review `venus provider list`, then repeat with `venus provider remove {name} --yes`."
                        );
                    }
                    let before = config.providers.len();
                    config.providers.retain(|provider| provider.name != name);
                    if config.providers.len() == before {
                        bail!("Provider '{name}' is not configured.");
                    }
                    let previous_models = config.models.len();
                    config.models.retain(|model| model.provider != name);
                    let removed_models = previous_models - config.models.len();
                    config.save()?;
                    println!("Removed provider '{name}' and {removed_models} associated model(s).");
                    if let Some(first) = config.models.first() {
                        println!(
                            "The next chat starts with the first remaining model: {}/{}.",
                            first.provider, first.name
                        );
                    }
                }
                ProviderCommands::Check { name } => {
                    let selected = provider::create_provider(&config, &name)?;
                    selected.check().await?;
                    println!("Provider '{}' is reachable.", selected.identity());
                }
                ProviderCommands::Models { name } => {
                    let selected = provider::create_provider(&config, &name)?;
                    for model in selected.discover_models().await? {
                        println!(
                            "{}{} (streaming={}, tools={:?}, structured_output={:?})",
                            model.id,
                            model
                                .display_name
                                .as_deref()
                                .filter(|name| !name.trim().is_empty())
                                .map(|name| format!(" — {name}"))
                                .unwrap_or_default(),
                            model.capabilities.streaming,
                            model.capabilities.tools,
                            model.capabilities.structured_output
                        );
                    }
                }
                ProviderCommands::Test { name, model } => {
                    let selected = provider::create_provider(&config, &name)?;
                    let model_id = config
                        .models
                        .iter()
                        .find(|configured| {
                            configured.provider == name
                                && (configured.name == model || configured.model_id == model)
                        })
                        .map(|configured| configured.model_id.clone())
                        .unwrap_or(model);
                    let mut on_delta = |_delta: &str| {};
                    let response = selected
                        .generate(
                            ChatRequest {
                                model: model_id.clone(),
                                prompt: "Reply with OK.".to_owned(),
                                messages: vec![ChatMessage {
                                    role: ChatRole::User,
                                    content: "Reply with OK.".to_owned(),
                                }],
                                json_schema: None,
                                tools: Vec::new(),
                            },
                            CancellationToken::new(),
                            &mut on_delta,
                        )
                        .await
                        .with_context(|| {
                            format!(
                                "Inference test failed for provider '{name}' and model '{model_id}'"
                            )
                        })?;
                    if response.content.trim().is_empty() {
                        bail!(
                            "Provider '{name}' returned no text for the inference test with model '{model_id}'"
                        );
                    }
                    println!(
                        "Inference succeeded: provider '{}' responded using model '{}'.",
                        response.provider, response.model
                    );
                }
                ProviderCommands::Switch { name, model } => {
                    switch_default_model(&mut config, &name, &model)?;
                    config.save()?;
                    println!(
                        "Default model for new chats is now {}/{}.",
                        config.models[0].provider, config.models[0].name
                    );
                    println!("No automatic fallback is configured.");
                }
            }
        }
        Commands::Permission { command } => {
            let config = AppConfig::load(&paths)?;
            initialize_logging(&config)?;
            let database = open_database(&config)?;
            match command {
                PermissionCommands::Profile { profile: None } => {
                    let profile = database.permission_profile()?;
                    println!("Profile: {}", profile.as_str());
                    println!("{}", profile_explanation(profile));
                }
                PermissionCommands::Profile {
                    profile: Some(profile),
                } => {
                    let profile = PermissionProfile::parse(&profile)?;
                    let previous = database.permission_profile()?;
                    if profile == previous {
                        println!("Permission profile is already {}.", profile.as_str());
                        println!("{}", profile_explanation(profile));
                        return Ok(());
                    }
                    println!(
                        "Changing permission profile from {} to {}.",
                        previous.as_str(),
                        profile.as_str()
                    );
                    println!("{}", profile_explanation(profile));
                    println!(
                        "This changes no grants or OS permissions; pending and approved requests bound to another profile will expire."
                    );
                    database.set_permission_profile(profile, unix_timestamp()?)?;
                    println!(
                        "Permission profile changed from {} to {}.",
                        previous.as_str(),
                        profile.as_str()
                    );
                }
                PermissionCommands::Grant {
                    capability_id,
                    capability_version,
                    operation,
                    permissions,
                    requires_approval,
                    expires_in_seconds,
                } => {
                    let now = unix_timestamp()?;
                    let expires_at = expires_in_seconds
                        .map(|seconds| {
                            if seconds <= 0 {
                                bail!("--expires-in-seconds must be positive");
                            }
                            now.checked_add(seconds)
                                .context("Grant expiration overflowed")
                        })
                        .transpose()?;
                    let draft = PermissionGrantDraft {
                        capability_id,
                        capability_version,
                        operation,
                        permissions,
                        requires_approval,
                        expires_at,
                    };
                    let id = database.create_permission_grant(&draft, now)?;
                    println!("Created grant #{id}.");
                    println!(
                        "This grant configures Venus policy; it does not install a capability."
                    );
                }
                PermissionCommands::Grants => {
                    let now = unix_timestamp()?;
                    let grants = database.list_permission_grants()?;
                    if grants.is_empty() {
                        println!("No capability grants are configured.");
                    }
                    for grant in grants {
                        let state = if grant.revoked_at.is_some() {
                            "revoked"
                        } else if grant.expires_at.is_some_and(|expires_at| expires_at <= now) {
                            "expired"
                        } else {
                            "active"
                        };
                        println!(
                            "#{} {}@{} operation={} permissions={:?} approval={} expires={:?} state={}",
                            grant.id,
                            grant.capability_id,
                            grant.capability_version,
                            grant.operation,
                            grant.permissions,
                            grant.requires_approval,
                            grant.expires_at,
                            state
                        );
                    }
                }
                PermissionCommands::Revoke { grant_id } => {
                    if !database.revoke_permission_grant(grant_id, unix_timestamp()?)? {
                        bail!("Permission grant #{grant_id} does not exist or is already revoked");
                    }
                    println!(
                        "Revoked grant #{grant_id} and invalidated its outstanding approvals."
                    );
                }
                PermissionCommands::Approvals => {
                    let approvals = database.list_approvals(unix_timestamp()?)?;
                    if approvals.is_empty() {
                        println!("No capability approval requests exist.");
                    }
                    for approval in approvals {
                        println!(
                            "#{} {:?} {}@{} operation={} scope={} expires={} args={} permissions={:?} consequences={:?}",
                            approval.id,
                            approval.state,
                            approval.capability_id,
                            approval.capability_version,
                            approval.operation,
                            approval.scope,
                            approval.expires_at,
                            approval.argument_summary,
                            approval.required_permissions,
                            approval.consequences
                        );
                    }
                }
                PermissionCommands::Approve { approval_id } => {
                    let approval =
                        database.resolve_approval(approval_id, true, unix_timestamp()?)?;
                    println!(
                        "Approved request #{} for {} operation {}; the approval is single-request and no operation was executed.",
                        approval.request_id, approval.capability_id, approval.operation
                    );
                }
                PermissionCommands::Deny { approval_id } => {
                    let approval =
                        database.resolve_approval(approval_id, false, unix_timestamp()?)?;
                    println!("Denied request #{}.", approval.request_id);
                }
                PermissionCommands::RevokeApproval { approval_id } => {
                    if !database.revoke_approval(approval_id, unix_timestamp()?)? {
                        bail!("Approval #{approval_id} does not exist or cannot be revoked");
                    }
                    println!("Revoked approval #{approval_id}.");
                }
                PermissionCommands::Audit { limit } => {
                    let events = database.list_capability_audit(limit)?;
                    if events.is_empty() {
                        println!("No capability audit events exist.");
                    }
                    for event in events {
                        println!(
                            "event={} request={} {}@{} operation={} profile={} decision={} reason={} status={} approval={:?} error={:?} args={}",
                            event.id,
                            event.request_id,
                            event.capability_id,
                            event.capability_version,
                            event.operation,
                            event.permission_profile.as_str(),
                            event.authorization_decision,
                            event.decision_reason,
                            event.status.as_str(),
                            event.approval_id,
                            event.error_classification,
                            event.argument_summary
                        );
                    }
                }
            }
        }
        Commands::Capability { command } => {
            let config = AppConfig::load(&paths)?;
            initialize_logging(&config)?;
            let database = open_database(&config)?;
            match command {
                CapabilityCommands::List { workspace } => {
                    let root = normalize_workspace(&workspace)?;
                    let mut orchestrator = CapabilityOrchestrator::default();
                    register_workspace_capabilities(&mut orchestrator, root)?;
                    for descriptor in orchestrator.discover() {
                        println!(
                            "{}@{} — {} [effect: {:?}]",
                            descriptor.id,
                            descriptor.version,
                            descriptor.description,
                            descriptor.effect
                        );
                    }
                }
                CapabilityCommands::Run {
                    capability_id,
                    operation,
                    workspace,
                    input,
                    approval_id,
                } => {
                    let root = normalize_workspace(&workspace)?;
                    let mut orchestrator = CapabilityOrchestrator::default();
                    register_workspace_capabilities(&mut orchestrator, root.clone())?;
                    let descriptor = orchestrator
                        .discover()
                        .find(|descriptor| descriptor.id == capability_id)
                        .cloned()
                        .with_context(|| {
                            format!("Capability '{capability_id}' is not registered")
                        })?;
                    let default_input = if capability_id == "workspace.summary" {
                        serde_json::json!({"workspace": root.to_string_lossy()})
                    } else {
                        serde_json::Value::Null
                    };
                    let mut input = match input {
                        Some(input) => {
                            let parsed: serde_json::Value =
                                serde_json::from_str(&input).context("Invalid JSON in --input")?;
                            if capability_id == "workspace.summary" && parsed != default_input {
                                bail!(
                                    "workspace.summary always uses the selected --workspace path"
                                );
                            }
                            parsed
                        }
                        None if !default_input.is_null() => default_input,
                        None => bail!("--input JSON is required for '{capability_id}'"),
                    };
                    if matches!(
                        capability_id.as_str(),
                        "workspace.file-read" | "workspace.file-replace"
                    ) {
                        let requested_path = input["path"]
                            .as_str()
                            .context("Workspace capability input must include a string path")?;
                        input["path"] = serde_json::json!(
                            repository::normalize_workspace_relative_path(requested_path)?
                        );
                    }
                    let request = CapabilityRequest {
                        capability_id,
                        capability_version: descriptor.version.clone(),
                        operation,
                        input,
                    };
                    print_capability_proposal(&descriptor, &request, &root);
                    let cancellation = CancellationToken::new();
                    let execution = orchestrator.run(
                        &database,
                        &request,
                        approval_id,
                        unix_timestamp()?,
                        cancellation.clone(),
                    );
                    tokio::pin!(execution);
                    let result = tokio::select! {
                        result = &mut execution => result?,
                        signal = tokio::signal::ctrl_c() => {
                            signal.context("Could not listen for Ctrl-C")?;
                            cancellation.cancel();
                            execution.await?
                        }
                    };
                    match result {
                        CapabilityRunResult::AwaitingApproval {
                            request_id,
                            approval_id,
                            reason,
                        } => {
                            println!("Approval required for request #{request_id}: {reason}");
                            println!(
                                "Review with `venus-agent permission approvals`, approve with `venus-agent permission approve {approval_id}`, then rerun the exact capability request with `--approval-id {approval_id}`."
                            );
                        }
                        CapabilityRunResult::Denied { reason, .. } => {
                            bail!("Capability request denied: {reason}");
                        }
                        CapabilityRunResult::Cancelled { request_id } => {
                            println!("Capability request #{request_id} was cancelled.");
                        }
                        CapabilityRunResult::Completed { request_id, output } => {
                            println!("Completed capability request #{request_id}:");
                            println!("{}", serde_json::to_string_pretty(&output)?);
                        }
                    }
                }
            }
        }
        Commands::Ask {
            provider: provider_name,
            model,
            prompt,
        } => {
            let config = AppConfig::load(&paths)?;
            initialize_logging(&config)?;
            let model_id = config
                .models
                .iter()
                .find(|configured| configured.provider == provider_name && configured.name == model)
                .map(|configured| configured.model_id.clone())
                .unwrap_or(model);
            let selected = provider::create_provider(&config, &provider_name)?;
            eprintln!("Provider: {} | Model: {}", selected.identity(), model_id);
            let mut streamed_content = String::new();
            let mut on_delta = |delta: &str| {
                print!("{delta}");
                let _ = std::io::Write::flush(&mut std::io::stdout());
                streamed_content.push_str(delta);
            };
            let request = provider::ChatRequest {
                model: model_id,
                prompt,
                messages: vec![],
                json_schema: None,
                tools: Vec::new(),
            };
            let response = tokio::select! {
                result = selected.generate(request, CancellationToken::new(), &mut on_delta) => result?,
                signal = tokio::signal::ctrl_c() => {
                    signal.context("Could not listen for Ctrl-C")?;
                    bail!("Request cancelled by user");
                }
            };
            debug_assert_eq!(streamed_content, response.content);
            if !response.tool_calls.is_empty() {
                bail!(
                    "The selected provider returned tool calls, but tool execution is not implemented."
                );
            }
            println!();
            println!(
                "[provider: {} | model: {} | input tokens: {:?} | output tokens: {:?}]",
                response.provider,
                response.model,
                response.usage.input_tokens,
                response.usage.output_tokens
            );
            open_database(&config)?.record_model_interaction(
                &response.provider,
                &response.model,
                response.usage.input_tokens,
                response.usage.output_tokens,
            )?;
        }
    }

    Ok(())
}

fn switch_default_model(config: &mut AppConfig, provider: &str, model: &str) -> Result<()> {
    let position = config
        .models
        .iter()
        .position(|configured| {
            configured.provider == provider
                && (configured.name == model || configured.model_id == model)
        })
        .with_context(|| {
            format!(
                "Model '{model}' is not configured for provider '{provider}'. Use `venus provider models {provider}` to inspect available models."
            )
        })?;
    let selected = config.models.remove(position);
    config.models.insert(0, selected);
    Ok(())
}

fn profile_explanation(profile: PermissionProfile) -> &'static str {
    match profile {
        PermissionProfile::Open => {
            "Open adds no Venus Agent approval or permission gates for reads. File replacement still requires a workspace.write grant and fresh exact-request approval. The process uses only its existing OS permissions; this is not a sandbox and does not elevate privileges."
        }
        PermissionProfile::Controlled => {
            "Controlled denies requests without a matching user grant. File replacement always requires per-request approval; read grants can also require approval."
        }
        PermissionProfile::Restricted => {
            "Restricted denies requests without a matching user grant and requires a fresh, request-specific approval for each authorized request."
        }
    }
}

fn print_capability_proposal(
    descriptor: &venus_agent::capability::CapabilityDescriptor,
    request: &CapabilityRequest,
    workspace: &std::path::Path,
) {
    println!("Proposed action: {} ({})", descriptor.name, descriptor.id);
    println!("Operation: {}", request.operation);
    println!("Version: {}", descriptor.version);
    println!("Workspace: {:?}", workspace.display().to_string());
    println!("Effect class: {:?}", descriptor.effect);
    println!("Effects:");
    for effect in &descriptor.effects {
        println!("  - {effect}");
    }
    println!(
        "Arguments: {}",
        venus_agent::permissions::argument_summary(&request.input)
    );
    if descriptor.id == "workspace.file-replace"
        && let (Some(old_text), Some(new_text)) = (
            request
                .input
                .get("old_text")
                .and_then(serde_json::Value::as_str),
            request
                .input
                .get("new_text")
                .and_then(serde_json::Value::as_str),
        )
    {
        println!("Proposed replacement:");
        println!("- {old_text}");
        println!("+ {new_text}");
    }
    println!(
        "Permission requirements: {}",
        descriptor
            .requirements
            .iter()
            .map(|requirement| format!("{} ({})", requirement.permission, requirement.description))
            .collect::<Vec<_>>()
            .join(", ")
    );
}

fn unix_timestamp() -> Result<i64> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("System clock is before the Unix epoch")?
        .as_secs();
    i64::try_from(seconds).context("System timestamp exceeds the supported range")
}

fn initialize_logging(config: &AppConfig) -> Result<()> {
    let filter = EnvFilter::try_new(&config.log_level)
        .with_context(|| format!("Invalid log level in {}", config.config_path.display()))?;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init()
        .map_err(|error| anyhow::anyhow!("Could not initialize application logging: {error}"))?;
    Ok(())
}

fn open_database(config: &AppConfig) -> Result<Database> {
    let database = Database::open(&config.database_path)?;
    if let Some(retention_days) = config.task_retention_days {
        let removed = database.prune_completed_tasks(retention_days)?;
        if removed > 0 {
            eprintln!(
                "Task retention: removed {removed} completed task(s) older than {retention_days} days."
            );
        }
    }
    if let Some(retention_days) = config.model_metadata_retention_days {
        let removed = database.prune_model_interactions(retention_days)?;
        if removed > 0 {
            eprintln!(
                "Model metadata retention: removed {removed} record(s) older than {retention_days} days."
            );
        }
    }
    Ok(database)
}

#[cfg(test)]
mod tests {
    use super::{Cli, switch_default_model};
    use clap::CommandFactory;
    use venus_agent::config::{AppConfig, ModelConfig};

    #[test]
    fn cli_name_matches_the_selected_binary() {
        assert_eq!(Cli::command().get_name(), env!("CARGO_BIN_NAME"));
    }

    #[test]
    fn provider_switch_persists_the_selected_model_as_default_order() {
        let mut config = AppConfig {
            database_path: std::env::temp_dir().join("venus-switch-test.sqlite3"),
            log_level: "off".into(),
            task_retention_days: None,
            model_metadata_retention_days: None,
            providers: Vec::new(),
            models: vec![
                ModelConfig {
                    name: "local-default".into(),
                    provider: "ollama-local".into(),
                    model_id: "qwen3.5:2b".into(),
                    task: None,
                },
                ModelConfig {
                    name: "cloud-default".into(),
                    provider: "openai".into(),
                    model_id: "gpt-test".into(),
                    task: None,
                },
            ],
            config_path: std::env::temp_dir().join("venus-switch-test.toml"),
        };
        switch_default_model(&mut config, "openai", "gpt-test")
            .expect("configured model should switch");
        assert_eq!(config.models[0].name, "cloud-default");
        assert!(switch_default_model(&mut config, "openai", "not-configured").is_err());
    }
}
