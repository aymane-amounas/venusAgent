use anyhow::{Context, Result, bail};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    OpenaiCompatible,
    Ollama,
    Anthropic,
    Gemini,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderConfig {
    pub name: String,
    pub kind: ProviderKind,
    pub base_url: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelConfig {
    pub name: String,
    pub provider: String,
    pub model_id: String,
    #[serde(default)]
    pub task: Option<String>,
}

#[derive(Debug)]
pub struct AppPaths {
    pub config_path: PathBuf,
    pub default_database_path: PathBuf,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct AppConfig {
    pub database_path: PathBuf,
    pub log_level: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_retention_days: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_metadata_retention_days: Option<u32>,
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    #[serde(default)]
    pub models: Vec<ModelConfig>,
    #[serde(skip)]
    pub config_path: PathBuf,
}

impl AppPaths {
    pub fn new() -> Result<Self> {
        let project_dirs = ProjectDirs::from("org", "venusAgent", "venusAgent")
            .context("Could not determine the application data directory")?;
        let legacy_project_dirs = ProjectDirs::from("dev", "Forge", "Forge Agent")
            .context("Could not determine the legacy application data directory")?;
        // Keep existing installs on their configured state path instead of silently creating a new database.
        let use_legacy_paths = prefer_legacy_paths(
            &project_dirs.config_dir().join("config.toml"),
            &project_dirs.data_local_dir().join("state.sqlite3"),
            &legacy_project_dirs.config_dir().join("config.toml"),
            &legacy_project_dirs.data_local_dir().join("state.sqlite3"),
        );
        let selected = if use_legacy_paths {
            legacy_project_dirs
        } else {
            project_dirs
        };
        Ok(Self {
            config_path: selected.config_dir().join("config.toml"),
            default_database_path: selected.data_local_dir().join("state.sqlite3"),
        })
    }

    pub fn initialize_config(&self) -> Result<()> {
        if self.config_path.exists() {
            return Ok(());
        }
        let parent = self
            .config_path
            .parent()
            .context("Configuration path has no parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("Could not create {}", parent.display()))?;

        let config = AppConfig {
            database_path: self.default_database_path.clone(),
            log_level: "info".to_owned(),
            task_retention_days: None,
            model_metadata_retention_days: None,
            providers: Vec::new(),
            models: Vec::new(),
            config_path: self.config_path.clone(),
        };
        let contents =
            toml::to_string_pretty(&config).context("Could not serialize default configuration")?;
        fs::write(&self.config_path, contents)
            .with_context(|| format!("Could not write {}", self.config_path.display()))?;
        Ok(())
    }
}

impl AppConfig {
    pub fn save(&self) -> Result<()> {
        let parent = self
            .config_path
            .parent()
            .context("Configuration path has no parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("Could not create {}", parent.display()))?;
        let contents = toml::to_string_pretty(self).context("Could not serialize configuration")?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("System clock is before the Unix epoch")?
            .as_nanos();
        let temporary_path =
            self.config_path
                .with_extension(format!("toml.{}.{}.tmp", std::process::id(), nonce));
        let write_result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary_path)
                .with_context(|| format!("Could not create {}", temporary_path.display()))?;
            file.write_all(contents.as_bytes())
                .with_context(|| format!("Could not write {}", temporary_path.display()))?;
            file.sync_all()
                .with_context(|| format!("Could not sync {}", temporary_path.display()))?;
            Ok::<_, anyhow::Error>(())
        })();
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temporary_path);
            return Err(error);
        }
        if let Err(error) = fs::rename(&temporary_path, &self.config_path) {
            let _ = fs::remove_file(&temporary_path);
            return Err(error).with_context(|| {
                format!(
                    "Could not replace configuration at {}",
                    self.config_path.display()
                )
            });
        }
        Ok(())
    }

    pub fn load(paths: &AppPaths) -> Result<Self> {
        let mut config = if paths.config_path.exists() {
            let contents = fs::read_to_string(&paths.config_path)
                .with_context(|| format!("Could not read {}", paths.config_path.display()))?;
            toml::from_str::<Self>(&contents).with_context(|| {
                format!("Invalid configuration in {}", paths.config_path.display())
            })?
        } else {
            Self {
                database_path: paths.default_database_path.clone(),
                log_level: "info".to_owned(),
                task_retention_days: None,
                model_metadata_retention_days: None,
                providers: Vec::new(),
                models: Vec::new(),
                config_path: paths.config_path.clone(),
            }
        };
        config.config_path = paths.config_path.clone();
        if !config.database_path.is_absolute() {
            bail!(
                "database_path must be absolute in {}",
                config.config_path.display()
            );
        }
        if config.task_retention_days == Some(0) {
            bail!(
                "task_retention_days must be a positive number of days in {}",
                config.config_path.display()
            );
        }
        if config.model_metadata_retention_days == Some(0) {
            bail!(
                "model_metadata_retention_days must be a positive number of days in {}",
                config.config_path.display()
            );
        }
        let mut provider_names = HashSet::new();
        for provider in &config.providers {
            if provider.name.trim().is_empty() {
                bail!("Provider names must not be empty");
            }
            if !provider_names.insert(provider.name.as_str()) {
                bail!("Duplicate provider name '{}'", provider.name);
            }
            if provider.timeout_secs == 0 {
                bail!(
                    "Provider '{}' timeout must be greater than zero",
                    provider.name
                );
            }
            let endpoint = Url::parse(&provider.base_url)
                .with_context(|| format!("Invalid endpoint for provider '{}'", provider.name))?;
            if !matches!(endpoint.scheme(), "http" | "https")
                || endpoint.host_str().is_none()
                || !endpoint.username().is_empty()
                || endpoint.password().is_some()
                || endpoint.query().is_some()
                || endpoint.fragment().is_some()
            {
                bail!(
                    "Provider '{}' endpoint must be HTTP(S), have a host, and must not embed credentials",
                    provider.name
                );
            }
            if provider.api_key_env.is_some()
                && endpoint.scheme() == "http"
                && !endpoint.host_str().is_some_and(is_loopback_host)
            {
                bail!(
                    "Provider '{}' uses an API key with a non-loopback HTTP endpoint; use HTTPS to protect credentials",
                    provider.name
                );
            }
            if provider
                .api_key_env
                .as_deref()
                .is_some_and(|name| name.trim().is_empty())
            {
                bail!("Provider '{}' api_key_env cannot be empty", provider.name);
            }
            if provider
                .api_key_env
                .as_deref()
                .is_some_and(|name| !valid_environment_variable(name))
            {
                bail!(
                    "Provider '{}' api_key_env must be a valid environment variable name",
                    provider.name
                );
            }
        }
        let mut model_aliases = HashSet::new();
        for model in &config.models {
            if model.name.trim().is_empty() || model.model_id.trim().is_empty() {
                bail!("Model names and IDs must not be empty");
            }
            if !model_aliases.insert((model.provider.as_str(), model.name.as_str())) {
                bail!(
                    "Duplicate model alias '{}' for provider '{}'",
                    model.name,
                    model.provider
                );
            }
            if !config
                .providers
                .iter()
                .any(|provider| provider.name == model.provider)
            {
                bail!(
                    "Model '{}' references unknown provider '{}'",
                    model.name,
                    model.provider
                );
            }
        }
        Ok(config)
    }
}

fn valid_environment_variable(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

pub(crate) fn is_loopback_host(host: &str) -> bool {
    let address = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || address
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn prefer_legacy_paths(
    current_config: &std::path::Path,
    current_database: &std::path::Path,
    legacy_config: &std::path::Path,
    legacy_database: &std::path::Path,
) -> bool {
    !(current_config.exists() || current_database.exists())
        && (legacy_config.exists() || legacy_database.exists())
}

fn default_timeout() -> u64 {
    120
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initializes_a_default_config_once() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let paths = AppPaths {
            config_path: temp.path().join("config/config.toml"),
            default_database_path: temp.path().join("data/state.sqlite3"),
        };

        paths.initialize_config()?;
        let original = fs::read_to_string(&paths.config_path)?;
        paths.initialize_config()?;

        assert_eq!(fs::read_to_string(&paths.config_path)?, original);
        let config = AppConfig::load(&paths)?;
        assert_eq!(config.database_path, paths.default_database_path);
        assert_eq!(config.log_level, "info");
        assert_eq!(config.task_retention_days, None);
        assert_eq!(config.model_metadata_retention_days, None);
        Ok(())
    }

    #[test]
    fn saves_configuration_atomically_and_can_reload_it() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let paths = AppPaths {
            config_path: temp.path().join("config/config.toml"),
            default_database_path: temp.path().join("data/state.sqlite3"),
        };
        let mut config = AppConfig {
            database_path: paths.default_database_path.clone(),
            log_level: "info".to_owned(),
            task_retention_days: None,
            model_metadata_retention_days: None,
            providers: vec![ProviderConfig {
                name: "local".to_owned(),
                kind: ProviderKind::Ollama,
                base_url: "http://localhost:11434".to_owned(),
                api_key_env: None,
                timeout_secs: 120,
            }],
            models: vec![ModelConfig {
                name: "default".to_owned(),
                provider: "local".to_owned(),
                model_id: "small-model".to_owned(),
                task: None,
            }],
            config_path: paths.config_path.clone(),
        };

        config.save()?;
        config.log_level = "warn".to_owned();
        config.save()?;
        let loaded = AppConfig::load(&paths)?;
        assert_eq!(loaded.providers.len(), 1);
        assert_eq!(loaded.models[0].model_id, "small-model");
        assert_eq!(loaded.log_level, "warn");
        assert_eq!(
            fs::read_dir(paths.config_path.parent().context("config parent")?)?.count(),
            1
        );
        Ok(())
    }

    #[test]
    fn loads_configuration_created_by_the_previous_setup_wizard() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let config_path = temp.path().join("config.toml");
        let previous_setup_config = AppConfig {
            database_path: temp.path().join("state.sqlite3"),
            log_level: "info".to_owned(),
            task_retention_days: None,
            model_metadata_retention_days: None,
            providers: vec![ProviderConfig {
                name: "qwen3.5:2b".into(),
                kind: ProviderKind::Ollama,
                base_url: "http://localhost:11434".into(),
                api_key_env: None,
                timeout_secs: 120,
            }],
            models: vec![ModelConfig {
                name: "5".into(),
                provider: "qwen3.5:2b".into(),
                model_id: "qwen3.5:2b".into(),
                task: Some("general".into()),
            }],
            config_path: config_path.clone(),
        };
        fs::write(
            &config_path,
            toml::to_string_pretty(&previous_setup_config)?,
        )?;
        let paths = AppPaths {
            config_path: config_path.clone(),
            default_database_path: temp.path().join("default.sqlite3"),
        };
        let config = AppConfig::load(&paths)?;
        assert_eq!(config.database_path, previous_setup_config.database_path);
        assert!(config.database_path.is_absolute());
        assert_eq!(config.providers[0].name, "qwen3.5:2b");
        assert_eq!(config.models[0].name, "5");
        assert_eq!(config.models[0].model_id, "qwen3.5:2b");
        Ok(())
    }

    #[test]
    fn existing_configuration_keeps_its_data_location_after_product_rename() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let current = temp.path().join("venus-agent");
        let legacy = temp.path().join("legacy");
        fs::create_dir_all(&legacy)?;
        let current_config = current.join("config.toml");
        let current_database = current.join("state.sqlite3");
        let legacy_config = legacy.join("config.toml");
        let legacy_database = legacy.join("state.sqlite3");

        fs::write(&legacy_config, "existing config")?;
        assert!(prefer_legacy_paths(
            &current_config,
            &current_database,
            &legacy_config,
            &legacy_database
        ));
        fs::create_dir_all(&current)?;
        fs::write(&current_config, "new config")?;
        assert!(!prefer_legacy_paths(
            &current_config,
            &current_database,
            &legacy_config,
            &legacy_database
        ));
        Ok(())
    }

    #[test]
    fn rejects_relative_database_paths() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let config_path = temp.path().join("config.toml");
        fs::write(
            &config_path,
            "database_path = \"state.sqlite3\"\nlog_level = \"info\"\n",
        )?;
        let paths = AppPaths {
            config_path,
            default_database_path: temp.path().join("default.sqlite3"),
        };

        assert!(AppConfig::load(&paths).is_err());
        Ok(())
    }

    #[test]
    fn rejects_api_keys_with_remote_plaintext_endpoints() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let config_path = temp.path().join("config.toml");
        let config = AppConfig {
            database_path: temp.path().join("state.sqlite3"),
            log_level: "info".to_owned(),
            task_retention_days: None,
            model_metadata_retention_days: None,
            providers: vec![ProviderConfig {
                name: "remote".into(),
                kind: ProviderKind::OpenaiCompatible,
                base_url: "http://example.com/v1".into(),
                api_key_env: Some("OPENAI_API_KEY".into()),
                timeout_secs: 120,
            }],
            models: Vec::new(),
            config_path: config_path.clone(),
        };
        fs::write(&config_path, toml::to_string_pretty(&config)?)?;
        let paths = AppPaths {
            config_path,
            default_database_path: temp.path().join("default.sqlite3"),
        };

        let error = AppConfig::load(&paths).expect_err("plaintext API-key endpoint must fail");
        assert!(format!("{error:#}").contains("use HTTPS"));
        Ok(())
    }

    #[test]
    fn permits_loopback_http_endpoints_with_api_keys() -> Result<()> {
        for endpoint in [
            "http://127.0.0.1:1234/v1",
            "http://localhost:1234/v1",
            "http://[::1]:1234/v1",
            "https://example.com/v1",
        ] {
            let temp = tempfile::tempdir()?;
            let config_path = temp.path().join("config.toml");
            let config = AppConfig {
                database_path: temp.path().join("state.sqlite3"),
                log_level: "info".to_owned(),
                task_retention_days: None,
                model_metadata_retention_days: None,
                providers: vec![ProviderConfig {
                    name: "local".into(),
                    kind: ProviderKind::OpenaiCompatible,
                    base_url: endpoint.into(),
                    api_key_env: Some("LOCAL_API_KEY".into()),
                    timeout_secs: 120,
                }],
                models: Vec::new(),
                config_path: config_path.clone(),
            };
            fs::write(&config_path, toml::to_string_pretty(&config)?)?;
            let paths = AppPaths {
                config_path,
                default_database_path: temp.path().join("default.sqlite3"),
            };

            AppConfig::load(&paths).with_context(|| format!("endpoint {endpoint}"))?;
        }
        Ok(())
    }

    #[test]
    fn rejects_zero_task_retention() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let config_path = temp.path().join("config.toml");
        let contents = toml::to_string(&AppConfig {
            database_path: temp.path().join("state.sqlite3"),
            log_level: "info".to_owned(),
            task_retention_days: Some(0),
            model_metadata_retention_days: None,
            providers: Vec::new(),
            models: Vec::new(),
            config_path: config_path.clone(),
        })?;
        fs::write(&config_path, contents)?;
        let paths = AppPaths {
            config_path,
            default_database_path: temp.path().join("default.sqlite3"),
        };

        assert!(AppConfig::load(&paths).is_err());
        Ok(())
    }

    #[test]
    fn reports_invalid_configuration_instead_of_using_defaults() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, "not valid toml = [")?;
        let paths = AppPaths {
            config_path,
            default_database_path: temp.path().join("default.sqlite3"),
        };

        assert!(AppConfig::load(&paths).is_err());
        Ok(())
    }

    #[test]
    fn rejects_provider_urls_that_embed_credentials() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let config_path = temp.path().join("config.toml");
        let config = AppConfig {
            database_path: temp.path().join("state.sqlite3"),
            log_level: "info".into(),
            task_retention_days: None,
            model_metadata_retention_days: None,
            providers: vec![ProviderConfig {
                name: "unsafe-url".into(),
                kind: ProviderKind::OpenaiCompatible,
                base_url: "https://user:secret@example.com/v1".into(),
                api_key_env: None,
                timeout_secs: 30,
            }],
            models: vec![],
            config_path: config_path.clone(),
        };
        fs::write(&config_path, toml::to_string(&config)?)?;
        let paths = AppPaths {
            config_path,
            default_database_path: temp.path().join("default.sqlite3"),
        };
        assert!(AppConfig::load(&paths).is_err());
        Ok(())
    }

    #[test]
    fn rejects_invalid_api_key_environment_variable_names() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let config_path = temp.path().join("config.toml");
        let config = AppConfig {
            database_path: temp.path().join("state.sqlite3"),
            log_level: "info".into(),
            task_retention_days: None,
            model_metadata_retention_days: None,
            providers: vec![ProviderConfig {
                name: "invalid-key-name".into(),
                kind: ProviderKind::OpenaiCompatible,
                base_url: "https://api.example.test/v1".into(),
                api_key_env: Some("NOT-AN-ENV-VAR".into()),
                timeout_secs: 30,
            }],
            models: vec![],
            config_path: config_path.clone(),
        };
        fs::write(&config_path, toml::to_string(&config)?)?;
        let paths = AppPaths {
            config_path,
            default_database_path: temp.path().join("default.sqlite3"),
        };
        let error = AppConfig::load(&paths).expect_err("invalid env name must be rejected");
        assert!(error.to_string().contains("environment variable name"));
        Ok(())
    }

    #[test]
    fn rejects_duplicate_provider_names() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let config_path = temp.path().join("config.toml");
        let provider = ProviderConfig {
            name: "same".into(),
            kind: ProviderKind::Ollama,
            base_url: "http://localhost:11434".into(),
            api_key_env: None,
            timeout_secs: 30,
        };
        let config = AppConfig {
            database_path: temp.path().join("state.sqlite3"),
            log_level: "info".into(),
            task_retention_days: None,
            model_metadata_retention_days: None,
            providers: vec![provider.clone(), provider],
            models: vec![],
            config_path: config_path.clone(),
        };
        fs::write(&config_path, toml::to_string(&config)?)?;
        let paths = AppPaths {
            config_path,
            default_database_path: temp.path().join("default.sqlite3"),
        };
        let error = AppConfig::load(&paths).expect_err("duplicate names must be rejected");
        assert!(error.to_string().contains("Duplicate provider"));
        Ok(())
    }
}
