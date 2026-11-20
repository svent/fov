//! Command-line options (§4).

use std::path::PathBuf;

use clap::{Parser, ValueEnum};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderName {
    Openrouter,
    Openai,
    Bedrock,
}

impl ProviderName {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderName::Openrouter => "openrouter",
            ProviderName::Openai => "openai",
            ProviderName::Bedrock => "bedrock",
        }
    }
}

/// The four settings that have both a config key and an option.
#[derive(Clone, Debug, Default, clap::Args)]
pub struct SettingArgs {
    /// Head of the input sent to the model, in KiB (default 16).
    #[arg(long, value_name = "N")]
    pub context_head_kib: Option<u64>,
    /// Limit when the head holds no complete line, in KiB (default 64).
    #[arg(long, value_name = "N")]
    pub context_head_max_kib: Option<u64>,
    /// Preview timeout on the full input, in ms (default 3000).
    #[arg(long, value_name = "N")]
    pub preview_timeout_ms: Option<u64>,
    /// Sample size after that timeout, in KiB (default 1024).
    #[arg(long, value_name = "N")]
    pub preview_sample_kib: Option<u64>,
}

/// Turn a plain-language request into a pipeline of standard text tools.
#[derive(Clone, Debug, Parser)]
#[command(name = "fov", version)]
pub struct Cli {
    /// Input file. If omitted, stdin is read.
    pub input: Option<PathBuf>,
    /// Write the result to FILE instead of stdout.
    #[arg(short, long, value_name = "FILE", conflicts_with = "no_output")]
    pub output: Option<PathBuf>,
    /// Don't run the final pipeline; print only the command, to stdout.
    #[arg(short, long)]
    pub no_output: bool,
    /// Overrides the configured provider.
    #[arg(long, value_name = "NAME")]
    pub provider: Option<ProviderName>,
    /// Overrides the provider's configured model.
    #[arg(long, value_name = "ID")]
    pub model: Option<String>,
    /// Overrides the configured reasoning effort (openai, openrouter), e.g. low or xhigh.
    #[arg(long, value_name = "LEVEL")]
    pub reasoning_effort: Option<String>,
    #[command(
        flatten,
        next_help_heading = "Settings (override the config file and the defaults)"
    )]
    pub settings: SettingArgs,
}
