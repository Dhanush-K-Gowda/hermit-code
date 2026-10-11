//! Configuration: ~/.config/hermit/config.toml, then ./.hermit.toml, then env vars, then CLI flags.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub const DEFAULT_MODEL: &str = "poolside/laguna-s-2.1:free";
pub const DEFAULT_FALLBACKS: &[&str] = &[
    "nvidia/nemotron-3-super-120b-a12b:free",
    "google/gemma-4-31b-it:free",
];

#[derive(Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct FileConfig {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    /// Name of the env var holding the key (default OPENROUTER_API_KEY).
    pub api_key_env: Option<String>,
    pub model: Option<String>,
    pub fallback_models: Option<Vec<String>>,
    pub verify_cmd: Option<String>,
    pub verify: Option<bool>,
    pub allow_commands: Vec<String>,
    pub max_steps: Option<usize>,
    pub max_fix_attempts: Option<usize>,
    pub context_budget_tokens: Option<usize>,
    pub command_timeout_secs: Option<u64>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

impl FileConfig {
    fn merge(mut self, o: FileConfig) -> Self {
        macro_rules! take {
            ($($f:ident),*) => { $( if o.$f.is_some() { self.$f = o.$f; } )* };
        }
        take!(base_url, api_key, api_key_env, model, fallback_models, verify_cmd, verify, max_steps,
              max_fix_attempts, context_budget_tokens, command_timeout_secs, temperature, max_tokens);
        self.allow_commands.extend(o.allow_commands);
        self
    }
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub root: PathBuf,
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub fallback_models: Vec<String>,
    pub verify_cmd: Option<String>,
    pub verify: bool,
    pub allow_commands: Vec<String>,
    pub max_steps: usize,
    pub max_fix_attempts: usize,
    pub context_budget_tokens: usize,
    pub command_timeout_secs: u64,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub yolo: bool,
}

pub fn config_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .map(|d| d.join("hermit"))
}

fn load_file(p: &Path) -> Result<FileConfig> {
    match std::fs::read_to_string(p) {
        Ok(s) => toml::from_str(&s).with_context(|| format!("invalid config {}", p.display())),
        Err(_) => Ok(FileConfig::default()),
    }
}

impl Settings {
    pub fn load(root: &Path) -> Result<Self> {
        let mut fc = FileConfig::default();
        if let Some(d) = config_dir() {
            fc = fc.merge(load_file(&d.join("config.toml"))?);
        }
        fc = fc.merge(load_file(&root.join(".hermit.toml"))?);

        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let key_env = fc.api_key_env.clone().unwrap_or_else(|| "OPENROUTER_API_KEY".into());
        let api_key = env("HERMIT_API_KEY").or_else(|| env(&key_env)).or(fc.api_key);

        Ok(Settings {
            root: root.to_path_buf(),
            base_url: env("HERMIT_BASE_URL").or(fc.base_url).unwrap_or_else(|| DEFAULT_BASE_URL.into()),
            api_key,
            model: env("HERMIT_MODEL").or(fc.model).unwrap_or_else(|| DEFAULT_MODEL.into()),
            fallback_models: fc
                .fallback_models
                .unwrap_or_else(|| DEFAULT_FALLBACKS.iter().map(|s| s.to_string()).collect()),
            verify_cmd: fc.verify_cmd,
            verify: fc.verify.unwrap_or(true),
            allow_commands: fc.allow_commands,
            max_steps: fc.max_steps.unwrap_or(40),
            max_fix_attempts: fc.max_fix_attempts.unwrap_or(3),
            context_budget_tokens: fc.context_budget_tokens.unwrap_or(60_000),
            command_timeout_secs: fc.command_timeout_secs.unwrap_or(120),
            temperature: fc.temperature,
            max_tokens: fc.max_tokens,
            yolo: false,
        })
    }
}
