//! Configuration: options > env vars (API keys) > config file > defaults (§10).

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::cli::{ProviderName, SettingArgs};
use crate::tools::DEFAULT_ALLOW;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    provider: Option<ProviderName>,
    sandbox: Option<bool>,
    openrouter: Option<KeyedSection>,
    openai: Option<KeyedSection>,
    bedrock: Option<BedrockSection>,
    context: Option<ContextSection>,
    preview: Option<PreviewSection>,
    commands: Option<CommandsSection>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyedSection {
    model: Option<String>,
    api_key: Option<String>,
    base_url: Option<String>,
    reasoning_effort: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct BedrockSection {
    model: Option<String>,
    profile: Option<String>,
    region: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextSection {
    head_kib: Option<i64>,
    head_max_kib: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewSection {
    timeout_ms: Option<i64>,
    sample_kib: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandsSection {
    allow: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub head_bytes: usize,
    pub head_max_bytes: usize,
    pub timeout: Duration,
    pub sample_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderConfig {
    OpenAi {
        model: String,
        api_key: String,
        base_url: Option<String>,
        reasoning_effort: Option<String>,
    },
    OpenRouter {
        model: String,
        api_key: String,
        base_url: Option<String>,
        reasoning_effort: Option<String>,
    },
    Bedrock {
        model: String,
        profile: Option<String>,
        region: Option<String>,
    },
}

impl ProviderConfig {
    pub fn name(&self) -> &'static str {
        match self {
            ProviderConfig::OpenAi { .. } => "openai",
            ProviderConfig::OpenRouter { .. } => "openrouter",
            ProviderConfig::Bedrock { .. } => "bedrock",
        }
    }

    pub fn model(&self) -> &str {
        match self {
            ProviderConfig::OpenAi { model, .. }
            | ProviderConfig::OpenRouter { model, .. }
            | ProviderConfig::Bedrock { model, .. } => model,
        }
    }

    pub fn reasoning_effort(&self) -> Option<&str> {
        match self {
            ProviderConfig::OpenAi {
                reasoning_effort, ..
            }
            | ProviderConfig::OpenRouter {
                reasoning_effort, ..
            } => reasoning_effort.as_deref(),
            ProviderConfig::Bedrock { .. } => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub provider: ProviderConfig,
    pub sandbox: bool,
    pub settings: Settings,
    pub allow: Vec<String>,
    /// Shown in the status line.
    pub warnings: Vec<String>,
}

/// What the command line overrides.
#[derive(Clone, Copy, Debug, Default)]
pub struct Overrides<'a> {
    pub provider: Option<ProviderName>,
    pub model: Option<&'a str>,
    pub reasoning_effort: Option<&'a str>,
    pub settings: Option<&'a SettingArgs>,
}

const EXAMPLE: &str = r#"provider = "openai"

[openai]
model = "<model id>"
# api_key = "sk-…"   # or set OPENAI_API_KEY"#;

/// `$XDG_CONFIG_HOME/fov/config.toml`, by default `~/.config/fov/config.toml`.
pub fn path(env: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    let base = env("XDG_CONFIG_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env("HOME").unwrap_or_default()).join(".config"));
    base.join("fov").join("config.toml")
}

/// Load the config file (if any) and merge it with the options and the environment.
pub fn load(overrides: Overrides, env: &dyn Fn(&str) -> Option<String>) -> Result<Config> {
    let path = path(env);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let file: FileConfig = match &text {
        Some(t) => toml::from_str(t).with_context(|| format!("in {}", path.display()))?,
        None => FileConfig::default(),
    };
    let mut warnings = Vec::new();
    if text.is_some() && has_api_key(&file) {
        let mode = std::fs::metadata(&path)
            .map(|m| m.permissions().mode())
            .unwrap_or(0);
        if mode & 0o077 != 0 {
            warnings.push(format!(
                "{} contains an API key and is readable by others; run chmod 600 on it",
                path.display()
            ));
        }
    }
    let provider = resolve_provider(&file, overrides, env, &path).map_err(|e| match text {
        Some(_) => e,
        None => e.context(format!(
            "no config file at {}. A minimal one:\n\n{EXAMPLE}\n",
            path.display()
        )),
    })?;
    Ok(Config {
        provider,
        sandbox: file.sandbox.unwrap_or(true),
        settings: resolve_settings(&file, overrides.settings, &path)?,
        allow: file
            .commands
            .and_then(|c| c.allow)
            .unwrap_or_else(|| DEFAULT_ALLOW.iter().map(|s| s.to_string()).collect()),
        warnings,
    })
}

fn has_api_key(f: &FileConfig) -> bool {
    [&f.openai, &f.openrouter]
        .iter()
        .any(|s| s.as_ref().is_some_and(|s| s.api_key.is_some()))
}

fn resolve_provider(
    file: &FileConfig,
    o: Overrides,
    env: &dyn Fn(&str) -> Option<String>,
    path: &Path,
) -> Result<ProviderConfig> {
    let Some(name) = o.provider.or(file.provider) else {
        bail!(
            "no provider configured: set `provider` in {} or pass --provider",
            path.display()
        );
    };
    let model = |section_model: Option<&String>| -> Result<String> {
        match o
            .model
            .map(str::to_string)
            .or_else(|| section_model.cloned())
        {
            Some(m) if !m.is_empty() => Ok(m),
            _ => bail!(
                "no model for {}: set `[{}].model` in {} or pass --model",
                name.as_str(),
                name.as_str(),
                path.display()
            ),
        }
    };
    let effort = |section: Option<&KeyedSection>| -> Option<String> {
        o.reasoning_effort
            .map(str::to_string)
            .or_else(|| section.and_then(|s| s.reasoning_effort.clone()))
            .filter(|e| !e.is_empty())
    };
    let key = |var: &str, section: Option<&KeyedSection>| -> Result<String> {
        match env(var)
            .filter(|k| !k.is_empty())
            .or_else(|| section.and_then(|s| s.api_key.clone()))
        {
            Some(k) => Ok(k),
            None => bail!(
                "no API key for {}: set {var} or `[{}].api_key` in {}",
                name.as_str(),
                name.as_str(),
                path.display()
            ),
        }
    };
    Ok(match name {
        ProviderName::Openai => {
            let s = file.openai.as_ref();
            ProviderConfig::OpenAi {
                model: model(s.and_then(|s| s.model.as_ref()))?,
                api_key: key("OPENAI_API_KEY", s)?,
                base_url: s.and_then(|s| s.base_url.clone()),
                reasoning_effort: effort(s),
            }
        }
        ProviderName::Openrouter => {
            let s = file.openrouter.as_ref();
            ProviderConfig::OpenRouter {
                model: model(s.and_then(|s| s.model.as_ref()))?,
                api_key: key("OPENROUTER_API_KEY", s)?,
                base_url: s.and_then(|s| s.base_url.clone()),
                reasoning_effort: effort(s),
            }
        }
        ProviderName::Bedrock => {
            if o.reasoning_effort.is_some() {
                bail!("--reasoning-effort applies to openai and openrouter, not bedrock");
            }
            let s = file.bedrock.as_ref();
            ProviderConfig::Bedrock {
                model: model(s.and_then(|s| s.model.as_ref()))?,
                profile: s.and_then(|s| s.profile.clone()),
                region: s.and_then(|s| s.region.clone()),
            }
        }
    })
}

/// One setting: the option, else the config key, else the default. Must be a positive integer.
fn setting(
    option: (&str, Option<u64>),
    key: (&str, Option<i64>),
    default: u64,
    path: &Path,
) -> Result<(u64, String)> {
    let (value, source) = match (option.1, key.1) {
        (Some(v), _) => (v as i64, option.0.to_string()),
        (None, Some(v)) => (v, format!("{} in {}", key.0, path.display())),
        (None, None) => return Ok((default, String::from("the default"))),
    };
    if value <= 0 {
        bail!("{source} must be a positive integer, got {value}");
    }
    Ok((value as u64, source))
}

fn resolve_settings(
    file: &FileConfig,
    args: Option<&SettingArgs>,
    path: &Path,
) -> Result<Settings> {
    let default_args = SettingArgs::default();
    let a = args.unwrap_or(&default_args);
    let ctx = file.context.as_ref();
    let pre = file.preview.as_ref();
    let (head, head_src) = setting(
        ("--context-head-kib", a.context_head_kib),
        ("[context].head_kib", ctx.and_then(|c| c.head_kib)),
        16,
        path,
    )?;
    let (head_max, max_src) = setting(
        ("--context-head-max-kib", a.context_head_max_kib),
        ("[context].head_max_kib", ctx.and_then(|c| c.head_max_kib)),
        64,
        path,
    )?;
    let (timeout, _) = setting(
        ("--preview-timeout-ms", a.preview_timeout_ms),
        ("[preview].timeout_ms", pre.and_then(|p| p.timeout_ms)),
        3000,
        path,
    )?;
    let (sample, _) = setting(
        ("--preview-sample-kib", a.preview_sample_kib),
        ("[preview].sample_kib", pre.and_then(|p| p.sample_kib)),
        1024,
        path,
    )?;
    if head_max < head {
        bail!(
            "head_max_kib ({head_max}, from {max_src}) must be at least head_kib ({head}, from {head_src})"
        );
    }
    Ok(Settings {
        head_bytes: (head as usize) << 10,
        head_max_bytes: (head_max as usize) << 10,
        timeout: Duration::from_millis(timeout),
        sample_bytes: sample << 10,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Env {
        dir: tempfile::TempDir,
        vars: HashMap<String, String>,
    }

    impl Env {
        fn new(config: Option<&str>) -> Env {
            let dir = tempfile::tempdir().unwrap();
            if let Some(c) = config {
                let p = dir.path().join("fov/config.toml");
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(&p, c).unwrap();
                std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
            let mut vars = HashMap::new();
            vars.insert(
                "XDG_CONFIG_HOME".to_string(),
                dir.path().to_string_lossy().into_owned(),
            );
            Env { dir, vars }
        }

        fn load(&self, o: Overrides) -> Result<Config> {
            load(o, &|k| self.vars.get(k).cloned())
        }
    }

    const BASE: &str =
        "provider = \"openai\"\n[openai]\nmodel = \"m-file\"\napi_key = \"k-file\"\n";

    #[test]
    fn precedence() {
        let mut env = Env::new(Some(BASE));
        let c = env.load(Overrides::default()).unwrap();
        assert_eq!(
            c.provider,
            ProviderConfig::OpenAi {
                model: "m-file".into(),
                api_key: "k-file".into(),
                base_url: None,
                reasoning_effort: None
            }
        );
        assert!(c.sandbox);
        assert_eq!(c.allow.len(), DEFAULT_ALLOW.len());
        env.vars.insert("OPENAI_API_KEY".into(), "k-env".into());
        let c = env
            .load(Overrides {
                model: Some("m-cli"),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            c.provider,
            ProviderConfig::OpenAi {
                model: "m-cli".into(),
                api_key: "k-env".into(),
                base_url: None,
                reasoning_effort: None
            }
        );
    }

    #[test]
    fn settings_precedence_and_validation() {
        let env = Env::new(Some(&format!(
            "{BASE}[context]\nhead_kib = 8\n[preview]\ntimeout_ms = 500\n"
        )));
        let c = env.load(Overrides::default()).unwrap();
        assert_eq!(c.settings.head_bytes, 8 << 10);
        assert_eq!(c.settings.head_max_bytes, 64 << 10);
        assert_eq!(c.settings.timeout, Duration::from_millis(500));
        assert_eq!(c.settings.sample_bytes, 1 << 20);

        let args = SettingArgs {
            context_head_kib: Some(32),
            ..Default::default()
        };
        let c = env
            .load(Overrides {
                settings: Some(&args),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(c.settings.head_bytes, 32 << 10);

        let args = SettingArgs {
            context_head_kib: Some(128),
            ..Default::default()
        };
        let e = env
            .load(Overrides {
                settings: Some(&args),
                ..Default::default()
            })
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("--context-head-kib") && e.contains("the default"),
            "{e}"
        );

        let args = SettingArgs {
            preview_timeout_ms: Some(0),
            ..Default::default()
        };
        let e = env
            .load(Overrides {
                settings: Some(&args),
                ..Default::default()
            })
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("--preview-timeout-ms must be a positive integer"),
            "{e}"
        );

        let env = Env::new(Some(&format!("{BASE}[preview]\nsample_kib = -1\n")));
        let e = env.load(Overrides::default()).unwrap_err().to_string();
        assert!(e.contains("[preview].sample_kib in"), "{e}");
    }

    #[test]
    fn openrouter_base_url() {
        let env = Env::new(Some(
            "provider = \"openrouter\"\n[openrouter]\nmodel = \"m\"\napi_key = \"k\"\nbase_url = \"http://127.0.0.1:8765/v1\"\n",
        ));
        let c = env.load(Overrides::default()).unwrap();
        assert_eq!(
            c.provider,
            ProviderConfig::OpenRouter {
                model: "m".into(),
                api_key: "k".into(),
                base_url: Some("http://127.0.0.1:8765/v1".into()),
                reasoning_effort: None
            }
        );
    }

    #[test]
    fn missing_pieces_name_the_keys() {
        let env = Env::new(Some("provider = \"openrouter\"\n"));
        let e = format!("{:#}", env.load(Overrides::default()).unwrap_err());
        assert!(e.contains("[openrouter].model"), "{e}");
        let env = Env::new(Some(
            "provider = \"openrouter\"\n[openrouter]\nmodel = \"x\"\n",
        ));
        let e = format!("{:#}", env.load(Overrides::default()).unwrap_err());
        assert!(
            e.contains("OPENROUTER_API_KEY") && e.contains("[openrouter].api_key"),
            "{e}"
        );
        let env = Env::new(Some("provder = \"openai\"\n"));
        assert!(format!("{:#}", env.load(Overrides::default()).unwrap_err()).contains("provder"));
    }

    #[test]
    fn no_config_file() {
        let mut env = Env::new(None);
        let e = format!("{:#}", env.load(Overrides::default()).unwrap_err());
        assert!(
            e.contains("no config file at") && e.contains("[openai]"),
            "{e}"
        );
        env.vars.insert("OPENAI_API_KEY".into(), "k".into());
        let c = env
            .load(Overrides {
                provider: Some(ProviderName::Openai),
                model: Some("m"),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(c.provider.model(), "m");
        let _ = env.dir;
    }

    #[test]
    fn reasoning_effort() {
        let env = Env::new(Some(&format!("{BASE}reasoning_effort = \"low\"\n")));
        assert_eq!(
            env.load(Overrides::default())
                .unwrap()
                .provider
                .reasoning_effort(),
            Some("low")
        );
        let o = Overrides {
            reasoning_effort: Some("xhigh"),
            ..Default::default()
        };
        assert_eq!(
            env.load(o).unwrap().provider.reasoning_effort(),
            Some("xhigh")
        );
        let env = Env::new(Some("provider = \"bedrock\"\n[bedrock]\nmodel = \"m\"\n"));
        assert!(env.load(o).unwrap_err().to_string().contains("not bedrock"));
        let env = Env::new(Some(
            "provider = \"bedrock\"\n[bedrock]\nmodel = \"m\"\nreasoning_effort = \"x\"\n",
        ));
        assert!(env.load(Overrides::default()).is_err(), "not a bedrock key");
    }

    #[test]
    fn readable_key_warns() {
        let env = Env::new(Some(BASE));
        let p = env.dir.path().join("fov/config.toml");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let c = env.load(Overrides::default()).unwrap();
        assert_eq!(c.warnings.len(), 1);
    }
}
