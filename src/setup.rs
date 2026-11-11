use anyhow::{Context, Result, bail};
use std::{
    ffi::OsStr,
    fs::{self, OpenOptions},
    io::{self, IsTerminal, Write},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;
use venus_agent::{
    config::{AppConfig, AppPaths, ModelConfig, ProviderConfig, ProviderKind},
    provider::{self, ChatMessage, ChatRequest, ChatRole, ModelInfo},
};

const OLLAMA_NAME: &str = "ollama-local";
const OLLAMA_URL: &str = "http://127.0.0.1:11434";

struct ProviderPreset {
    name: &'static str,
    kind: ProviderKind,
    base_url: &'static str,
    api_key_env: Option<&'static str>,
}

const CLOUD_PRESETS: [ProviderPreset; 4] = [
    ProviderPreset {
        name: "openai",
        kind: ProviderKind::OpenaiCompatible,
        base_url: "https://api.openai.com/v1",
        api_key_env: Some("OPENAI_API_KEY"),
    },
    ProviderPreset {
        name: "anthropic",
        kind: ProviderKind::Anthropic,
        base_url: "https://api.anthropic.com",
        api_key_env: Some("ANTHROPIC_API_KEY"),
    },
    ProviderPreset {
        name: "gemini",
        kind: ProviderKind::Gemini,
        base_url: "https://generativelanguage.googleapis.com/v1beta",
        api_key_env: Some("GEMINI_API_KEY"),
    },
    ProviderPreset {
        name: "openrouter",
        kind: ProviderKind::OpenaiCompatible,
        base_url: "https://openrouter.ai/api/v1",
        api_key_env: Some("OPENROUTER_API_KEY"),
    },
];

pub async fn run(paths: &AppPaths, mut config: AppConfig) -> Result<AppConfig> {
    if !io::stdin().is_terminal() {
        bail!(
            "Interactive setup needs a terminal. Run `venus-agent setup` from a terminal, or configure {} manually.",
            paths.config_path.display()
        );
    }

    println!("Welcome to Venus Agent.");
    repair_legacy_ollama_names(&mut config)?;
    if config.providers.is_empty() || config.models.is_empty() {
        first_run_setup(paths, &mut config).await?;
    } else {
        manage_setup(paths, &mut config).await?;
    }
    if !config.providers.is_empty() && !config.models.is_empty() {
        println!("Configuration saved to {}.", paths.config_path.display());
    }
    Ok(config)
}

async fn first_run_setup(paths: &AppPaths, config: &mut AppConfig) -> Result<()> {
    let ollama_on_path = ollama_cli_on_path();
    println!(
        "Ollama CLI on PATH: {}. Checking the local service for installed models...",
        if ollama_on_path { "found" } else { "not found" }
    );
    let local = ollama_config();
    match discover_for_setup(config, local.clone()).await {
        Ok(models) if !models.is_empty() => {
            let recommended = recommend_model(&models);
            let selected = &models[recommended];
            println!(
                "Using installed model '{}' with provider '{}'. No model will be downloaded.",
                selected.id, OLLAMA_NAME
            );
            println!("Checking that the selected model can generate a response...");
            test_inference(config, &local, selected).await?;
            save_model(config, local, selected)?;
            config.save()?;
            println!("Setup complete. Start chatting with `venus`.");
            Ok(())
        }
        Ok(_) => {
            println!("Ollama is reachable, but no models are installed.");
            offer_cloud_or_later(paths, config).await
        }
        Err(error) => {
            if ollama_on_path {
                println!(
                    "Ollama CLI is installed, but its local service is not reachable: {error:#}"
                );
                println!("Start Ollama and run `venus` again to use local models.");
            } else {
                println!(
                    "Ollama CLI was not found on PATH and its local service is not reachable: {error:#}"
                );
                println!("Install and start Ollama to use local models.");
            }
            offer_cloud_or_later(paths, config).await
        }
    }
}

fn ollama_cli_on_path() -> bool {
    ollama_cli_available(std::env::var_os("PATH").as_deref())
}

fn ollama_cli_available(path: Option<&OsStr>) -> bool {
    let Some(path) = path else {
        return false;
    };
    let executable = if cfg!(windows) {
        "ollama.exe"
    } else {
        "ollama"
    };
    std::env::split_paths(&path).any(|directory| executable_file(&directory.join(executable)))
}

fn executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

async fn offer_cloud_or_later(paths: &AppPaths, config: &mut AppConfig) -> Result<()> {
    println!("You can configure a cloud provider now, or finish and do this later.");
    println!("1) Configure a cloud provider");
    println!("2) Do this later");
    match prompt("Choose an option", Some("2"))?.as_str() {
        "1" => add_cloud_provider(paths, config).await,
        "2" => {
            println!(
                "No provider was configured. Run `venus setup` after installing Ollama or when you are ready to configure a cloud provider."
            );
            Ok(())
        }
        _ => bail!("Choose 1 to configure a cloud provider or 2 to finish later."),
    }
}

async fn manage_setup(paths: &AppPaths, config: &mut AppConfig) -> Result<()> {
    loop {
        println!("Provider setup");
        println!("1) Add an Ollama model");
        println!("2) Add a cloud provider");
        println!("3) Finish");
        match prompt("Choose an option", Some("3"))?.as_str() {
            "1" => add_ollama_model(paths, config).await?,
            "2" => add_cloud_provider(paths, config).await?,
            "3" => return Ok(()),
            _ => println!("Choose 1, 2, or 3."),
        }
    }
}

async fn add_ollama_model(paths: &AppPaths, config: &mut AppConfig) -> Result<()> {
    let provider_config = config
        .providers
        .iter()
        .find(|provider| provider.name == OLLAMA_NAME)
        .cloned()
        .unwrap_or_else(ollama_config);
    let models = discover_for_setup(config, provider_config.clone())
        .await
        .context("Could not validate Ollama or discover its installed models")?;
    let model = choose_model(&models, "Ollama")?;
    test_inference(config, &provider_config, model).await?;
    save_model(config, provider_config, model)?;
    config.save()?;
    println!("Added {} from Ollama.", model.id);
    println!("Updated {}.", paths.config_path.display());
    Ok(())
}

async fn add_cloud_provider(paths: &AppPaths, config: &mut AppConfig) -> Result<()> {
    println!("Choose a cloud provider:");
    for (index, preset) in CLOUD_PRESETS.iter().enumerate() {
        println!("{}) {}", index + 1, preset.name);
    }
    println!("5) Finish without adding a provider");
    let selection = prompt("Provider", Some("5"))?;
    let index = selection
        .parse::<usize>()
        .context("Choose one of the listed provider numbers")?;
    let Some(preset) = CLOUD_PRESETS.get(index.wrapping_sub(1)) else {
        if index == 5 {
            return Ok(());
        }
        bail!("Choose a provider number from the list");
    };
    configure_cloud(paths, config, preset).await
}

async fn configure_cloud(
    paths: &AppPaths,
    config: &mut AppConfig,
    preset: &ProviderPreset,
) -> Result<()> {
    let existing = config
        .providers
        .iter()
        .find(|provider| provider.name == preset.name)
        .cloned();
    let provider_config = existing.unwrap_or_else(|| ProviderConfig {
        name: preset.name.to_owned(),
        kind: preset.kind.clone(),
        base_url: preset.base_url.to_owned(),
        api_key_env: preset.api_key_env.map(str::to_owned),
        timeout_secs: 120,
    });
    let secret_name = provider_config.api_key_env.clone().unwrap_or_default();
    if secret_name.is_empty() {
        bail!(
            "Provider '{}' has no configured API key environment name",
            preset.name
        );
    }
    if !std::env::var(&secret_name).is_ok_and(|value| !value.is_empty()) {
        bail!(
            "Set {} in your shell, then run `venus setup` again. Venus never asks for or stores API key values.",
            secret_name
        );
    }

    println!(
        "Checking {} authentication and discovering available models...",
        preset.name
    );
    let models = discover_for_setup(config, provider_config.clone())
        .await
        .with_context(|| {
            format!(
                "Could not validate {} credentials or discover models",
                preset.name
            )
        })?;
    let model = choose_model(&models, preset.name)?;
    println!(
        "Venus will send one short test request to '{}'. Provider usage or charges may apply.",
        model.id
    );
    if !confirm("Run a minimal inference test before saving? [Y/n]", true)? {
        bail!("Provider was not saved because model inference was not verified.");
    }
    test_inference(config, &provider_config, model).await?;
    save_model(config, provider_config, model)?;
    config.save()?;
    println!(
        "Added {}. Credentials are read from {} and are not saved to the config.",
        preset.name, secret_name
    );
    println!("Updated {}.", paths.config_path.display());
    Ok(())
}

async fn discover_for_setup(
    config: &AppConfig,
    provider_config: ProviderConfig,
) -> Result<Vec<ModelInfo>> {
    let temporary = config_with_provider(config, provider_config.clone());
    let selected = provider::create_provider(&temporary, &provider_config.name)?;
    selected
        .discover_models()
        .await
        .with_context(|| format!("Could not reach provider '{}'", provider_config.name))
}

async fn test_inference(
    config: &AppConfig,
    provider_config: &ProviderConfig,
    model: &ModelInfo,
) -> Result<()> {
    let temporary = config_with_provider(config, provider_config.clone());
    let selected = provider::create_provider(&temporary, &provider_config.name)?;
    let mut on_delta = |_delta: &str| {};
    let response = selected
        .generate(
            ChatRequest {
                model: model.id.clone(),
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
        .context("The minimal inference test failed; configuration was not saved")?;
    if response.content.trim().is_empty() {
        bail!("The provider returned no text for the minimal inference test");
    }
    println!("Inference test succeeded.");
    Ok(())
}

fn save_model(
    config: &mut AppConfig,
    provider_config: ProviderConfig,
    model: &ModelInfo,
) -> Result<()> {
    if !config
        .providers
        .iter()
        .any(|provider| provider.name == provider_config.name)
    {
        config.providers.push(provider_config.clone());
    }
    let alias = unique_alias(config, &provider_config.name, &model.id);
    config.models.push(ModelConfig {
        name: alias,
        provider: provider_config.name,
        model_id: model.id.clone(),
        task: Some("general".to_owned()),
    });
    Ok(())
}

fn choose_model<'a>(models: &'a [ModelInfo], provider: &str) -> Result<&'a ModelInfo> {
    if models.is_empty() {
        bail!(
            "Provider '{}' is reachable but exposes no models Venus can use. Select a model in your provider account, then try again.",
            provider
        );
    }
    let recommended = recommend_model(models);
    println!("Available models from {provider}:");
    for (index, model) in models.iter().enumerate() {
        let recommendation = if index == recommended {
            " (recommended)"
        } else {
            ""
        };
        println!("  {}) {}{recommendation}", index + 1, model.id);
    }
    let default = (recommended + 1).to_string();
    let selection = prompt("Select a model", Some(&default))?;
    let index = selection
        .parse::<usize>()
        .context("Model selection must be one of the listed numbers")?;
    models
        .get(
            index
                .checked_sub(1)
                .context("Choose a model number from the list")?,
        )
        .with_context(|| format!("No model is numbered {index}"))
}

fn recommend_model(models: &[ModelInfo]) -> usize {
    models
        .iter()
        .position(|model| {
            let id = model.id.to_ascii_lowercase();
            id.contains("coder") || id.contains("code")
        })
        .unwrap_or(0)
}

fn ollama_config() -> ProviderConfig {
    ProviderConfig {
        name: OLLAMA_NAME.to_owned(),
        kind: ProviderKind::Ollama,
        base_url: OLLAMA_URL.to_owned(),
        api_key_env: None,
        timeout_secs: 120,
    }
}

fn config_with_provider(config: &AppConfig, provider: ProviderConfig) -> AppConfig {
    let mut temporary = AppConfig {
        database_path: config.database_path.clone(),
        log_level: config.log_level.clone(),
        task_retention_days: config.task_retention_days,
        model_metadata_retention_days: config.model_metadata_retention_days,
        providers: config
            .providers
            .iter()
            .filter(|configured| configured.name != provider.name)
            .cloned()
            .collect(),
        models: config.models.clone(),
        config_path: config.config_path.clone(),
    };
    temporary.providers.push(provider);
    temporary
}

fn unique_alias(config: &AppConfig, provider: &str, model_id: &str) -> String {
    let base = format!("{provider}-{}", slug(model_id));
    if !config
        .models
        .iter()
        .any(|model| model.provider == provider && model.name == base)
    {
        return base;
    }
    for suffix in 2.. {
        let candidate = format!("{base}-{suffix}");
        if !config
            .models
            .iter()
            .any(|model| model.provider == provider && model.name == candidate)
        {
            return candidate;
        }
    }
    unreachable!("unbounded suffix sequence always contains a free alias")
}

fn slug(value: &str) -> String {
    let mut slug = String::new();
    let mut separator = false;
    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            if separator && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(character);
            separator = false;
        } else {
            separator = true;
        }
    }
    if slug.len() > 48 {
        slug.truncate(48);
    }
    slug.trim_matches('-').to_owned()
}

fn repair_legacy_ollama_names(config: &mut AppConfig) -> Result<()> {
    let Some((provider_index, old_name, numeric_alias, model_id)) = legacy_ollama_candidate(config)
    else {
        return Ok(());
    };
    let suggested_alias = format!("ollama-local-{}", slug(&model_id));
    println!(
        "Found a likely setup mistake: provider '{old_name}' and model alias '{numeric_alias}' appear to be selections rather than names."
    );
    if !confirm(
        &format!(
            "Rename the provider to '{OLLAMA_NAME}' and the model alias to '{suggested_alias}'? A backup will be created. [Y/n]"
        ),
        true,
    )? {
        return Ok(());
    }
    backup_config(&config.config_path)?;
    apply_legacy_ollama_names(
        config,
        provider_index,
        &old_name,
        &numeric_alias,
        &suggested_alias,
    );
    config.save()?;
    println!("Normalized the Ollama provider and model names.");
    Ok(())
}

fn apply_legacy_ollama_names(
    config: &mut AppConfig,
    provider_index: usize,
    old_name: &str,
    numeric_alias: &str,
    suggested_alias: &str,
) {
    config.providers[provider_index].name = OLLAMA_NAME.to_owned();
    for model in &mut config.models {
        if model.provider == old_name {
            model.provider = OLLAMA_NAME.to_owned();
            if model.name == numeric_alias {
                model.name = suggested_alias.to_owned();
            }
        }
    }
}

fn legacy_ollama_candidate(config: &AppConfig) -> Option<(usize, String, String, String)> {
    config
        .providers
        .iter()
        .enumerate()
        .find_map(|(index, provider)| {
            if provider.kind != ProviderKind::Ollama
                || provider.name == OLLAMA_NAME
                || config
                    .providers
                    .iter()
                    .any(|configured| configured.name == OLLAMA_NAME)
            {
                return None;
            }
            let model = config.models.iter().find(|model| {
                model.provider == provider.name
                    && model
                        .name
                        .chars()
                        .all(|character| character.is_ascii_digit())
                    && model.model_id == provider.name
            })?;
            Some((
                index,
                provider.name.clone(),
                model.name.clone(),
                model.model_id.clone(),
            ))
        })
}

fn backup_config(path: &Path) -> Result<()> {
    let contents = fs::read(path).with_context(|| format!("Could not read {}", path.display()))?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("System clock is before the Unix epoch")?
        .as_nanos();
    let backup = path.with_extension(format!("toml.backup-{timestamp}"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&backup)
        .with_context(|| format!("Could not create configuration backup {}", backup.display()))?;
    file.write_all(&contents)
        .with_context(|| format!("Could not write configuration backup {}", backup.display()))?;
    file.sync_all()
        .with_context(|| format!("Could not sync configuration backup {}", backup.display()))?;
    Ok(())
}

fn confirm(label: &str, default: bool) -> Result<bool> {
    let answer = prompt(label, None)?;
    Ok(match answer.trim().to_ascii_lowercase().as_str() {
        "" => default,
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => bail!("Answer yes or no"),
    })
}

fn prompt(label: &str, default: Option<&str>) -> Result<String> {
    match default {
        Some(default) => print!("{label} [{default}]: "),
        None => print!("{label}: "),
    }
    io::stdout()
        .flush()
        .context("Could not display setup prompt")?;
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("Could not read setup input")?;
    let input = input.trim();
    if input.is_empty() {
        return Ok(default.unwrap_or_default().to_owned());
    }
    Ok(input.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn test_config(path: &Path) -> AppConfig {
        AppConfig {
            database_path: path.join("state.sqlite3"),
            log_level: "off".to_owned(),
            task_retention_days: None,
            model_metadata_retention_days: None,
            providers: Vec::new(),
            models: Vec::new(),
            config_path: path.join("config.toml"),
        }
    }

    fn model(id: &str) -> ModelInfo {
        ModelInfo {
            id: id.to_owned(),
            display_name: None,
            capabilities: provider::ModelCapabilities {
                streaming: true,
                tools: None,
                structured_output: None,
            },
        }
    }

    #[test]
    fn generated_aliases_are_descriptive_and_unique() {
        let mut config = test_config(Path::new("/tmp"));
        assert_eq!(
            unique_alias(&config, OLLAMA_NAME, "qwen3.5:2b"),
            "ollama-local-qwen3-5-2b"
        );
        config.models.push(ModelConfig {
            name: "ollama-local-qwen3-5-2b".into(),
            provider: OLLAMA_NAME.into(),
            model_id: "qwen3.5:2b".into(),
            task: None,
        });
        assert_eq!(
            unique_alias(&config, OLLAMA_NAME, "qwen3.5:2b"),
            "ollama-local-qwen3-5-2b-2"
        );
    }

    #[test]
    fn recommendation_prefers_a_coding_model_only_when_installed() {
        let models = vec![model("general:latest"), model("qwen2.5-coder:3b")];
        assert_eq!(recommend_model(&models), 1);
        assert_eq!(recommend_model(&models[..1]), 0);
    }

    #[test]
    fn ollama_detection_checks_path_without_invoking_a_command() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let executable = if cfg!(windows) {
            "ollama.exe"
        } else {
            "ollama"
        };
        let binary = temp.path().join(executable);
        fs::write(&binary, b"")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))?;
        }
        let path = OsString::from(temp.path());
        assert!(ollama_cli_available(Some(&path)));
        assert!(!ollama_cli_available(None));
        Ok(())
    }

    #[test]
    fn legacy_accidental_names_are_detected_and_repaired_with_a_backup() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut config = test_config(temp.path());
        config.providers.push(ProviderConfig {
            name: "qwen3.5:2b".into(),
            kind: ProviderKind::Ollama,
            base_url: OLLAMA_URL.into(),
            api_key_env: None,
            timeout_secs: 120,
        });
        config.models.push(ModelConfig {
            name: "5".into(),
            provider: "qwen3.5:2b".into(),
            model_id: "qwen3.5:2b".into(),
            task: None,
        });
        assert_eq!(
            legacy_ollama_candidate(&config),
            Some((0, "qwen3.5:2b".into(), "5".into(), "qwen3.5:2b".into()))
        );
        config.save()?;
        let original = fs::read(&config.config_path)?;
        backup_config(&config.config_path)?;
        apply_legacy_ollama_names(&mut config, 0, "qwen3.5:2b", "5", "ollama-local-qwen3-5-2b");
        config.save()?;
        assert_eq!(config.providers[0].name, OLLAMA_NAME);
        assert_eq!(config.models[0].provider, OLLAMA_NAME);
        assert_eq!(config.models[0].name, "ollama-local-qwen3-5-2b");
        let backup = fs::read_dir(temp.path())?
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .find(|path| path.to_string_lossy().contains(".backup-"))
            .context("configuration backup should be created before normalization")?;
        assert_eq!(fs::read(backup)?, original);
        assert!(legacy_ollama_candidate(&config).is_none());
        let mut ordinary = test_config(temp.path());
        ordinary.providers.push(ProviderConfig {
            name: "named-provider".into(),
            kind: ProviderKind::Ollama,
            base_url: OLLAMA_URL.into(),
            api_key_env: None,
            timeout_secs: 120,
        });
        ordinary.models.push(ModelConfig {
            name: "default".into(),
            provider: "named-provider".into(),
            model_id: "regular-model".into(),
            task: None,
        });
        assert!(legacy_ollama_candidate(&ordinary).is_none());
        assert!(slug("qwen3.5:2b") == "qwen3-5-2b");
        Ok(())
    }
}
