//! Serde model for config.toml.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub settings: Settings,
    /// Named provider connections, e.g. `[providers.anthropic]`.
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Model roles, e.g. `[models.planner]`: the standard roles the engine
    /// consults and any custom role, all in one shape.
    #[serde(default)]
    pub models: ModelRoles,
    /// MCP server definitions, e.g. `[mcp.github]`.
    #[serde(default)]
    pub mcp: BTreeMap<String, McpServerConfig>,
    #[serde(default)]
    pub plans: PlanPaths,
    #[serde(default)]
    pub tools: ToolPaths,
    #[serde(default)]
    pub storage: StorageConfig,
    #[serde(default)]
    pub user: UserConfig,
    #[serde(default)]
    pub project: ProjectConfig,
    #[serde(default)]
    pub prompts: PromptConfig,
    #[serde(default)]
    pub workbench: WorkbenchConfig,
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    /// Per-model token prices, e.g. `[pricing."claude-sonnet-5"]`. Keyed by
    /// the model id as written in `[models]` — that is what goes on the wire.
    /// Absent prices mean usage is reported in tokens with no dollar figure.
    #[serde(default)]
    pub pricing: BTreeMap<String, ModelPrice>,
    #[serde(default)]
    pub packs: BTreeMap<String, crate::pack::PackEntry>,
}

/// What one model costs, in USD per million tokens.
///
/// Deliberately not shipped with built-in defaults: published prices change,
/// and a stale table that quietly reports the wrong dollar figure is worse
/// than reporting none. An unpriced model still reports its token counts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelPrice {
    /// Uncached input tokens.
    pub input: f64,
    pub output: f64,
    /// Tokens written to the prompt cache. Defaults to `input` x 1.25, the
    /// standard premium for a 5-minute TTL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    /// Tokens served from the prompt cache. Defaults to `input` x 0.10.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<PriceTier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PriceTier {
    pub above: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenCounts {
    pub input: u64,
    pub output: u64,
    pub cache_write: u64,
    pub cache_read: u64,
}

impl TokenCounts {
    pub fn prompt(&self) -> u64 {
        self.input + self.cache_write + self.cache_read
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rates {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
}

impl ModelPrice {
    pub fn tier_for(&self, prompt_tokens: u64) -> Option<&PriceTier> {
        self.tiers
            .iter()
            .filter(|tier| prompt_tokens > tier.above)
            .max_by_key(|tier| tier.above)
    }

    pub fn rates_for(&self, prompt_tokens: u64) -> Rates {
        let base = Rates {
            input: self.input,
            output: self.output,
            cache_write: self.cache_write_rate(),
            cache_read: self.cache_read_rate(),
        };
        let Some(tier) = self.tier_for(prompt_tokens) else {
            return base;
        };
        let input = tier.input.unwrap_or(base.input);
        let scaled = |rate: f64| {
            if base.input == 0.0 {
                rate
            } else {
                rate * input / base.input
            }
        };
        Rates {
            input,
            output: tier.output.unwrap_or(base.output),
            cache_write: tier.cache_write.unwrap_or_else(|| scaled(base.cache_write)),
            cache_read: tier.cache_read.unwrap_or_else(|| scaled(base.cache_read)),
        }
    }

    pub fn cost(&self, tokens: &TokenCounts) -> f64 {
        let rates = self.rates_for(tokens.prompt());
        let per_million = |count: u64, rate: f64| (count as f64) * rate / 1_000_000.0;
        per_million(tokens.input, rates.input)
            + per_million(tokens.output, rates.output)
            + per_million(tokens.cache_write, rates.cache_write)
            + per_million(tokens.cache_read, rates.cache_read)
    }

    /// Cache-write rate, falling back to the standard 1.25x premium.
    pub fn cache_write_rate(&self) -> f64 {
        self.cache_write.unwrap_or(self.input * 1.25)
    }

    /// Cache-read rate, falling back to the standard 0.1x rate.
    ///
    /// Divides rather than multiplying by 0.1: `3.0 * 0.1` lands a rounding
    /// step away from `0.3`, and a rate that prints as `0.30000000000000004`
    /// invites doubt about the whole cost figure.
    pub fn cache_read_rate(&self) -> f64 {
        self.cache_read.unwrap_or(self.input / 10.0)
    }
}

/// System-prompt overrides. Each field replaces the built-in text
/// wholesale; leave unset to keep the default.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct PromptConfig {
    /// Base system prompt for the chat/ask agent loop. The current
    /// date/time and `[user]` name/context are still appended after it.
    pub chat: Option<String>,
    /// Workbench framing and policy, appended to the chat prompt inside
    /// `dive workbench`. The `workbench__*` tool rules are appended
    /// after it and are not overridable.
    pub workbench: Option<String>,
}

/// `dive workbench` settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct WorkbenchConfig {
    /// Where the workbench writes its debug log (tilde-expanded). Default:
    /// `<data_dir>/workbench.log`; the `DIVE_WORKBENCH_LOG` env var wins
    /// over both.
    pub log_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct TelemetryConfig {
    pub endpoint: Option<String>,
    pub protocol: TelemetryProtocol,
    pub timeout_secs: Option<u64>,
    pub service_name: Option<String>,
    pub headers: BTreeMap<String, String>,
    pub resource: BTreeMap<String, String>,
    pub capture_content: bool,
    pub logs: bool,
    #[serde(skip)]
    pub missing_env: Vec<MissingEnv>,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            endpoint: None,
            protocol: TelemetryProtocol::default(),
            timeout_secs: None,
            service_name: None,
            headers: BTreeMap::new(),
            resource: BTreeMap::new(),
            capture_content: true,
            logs: false,
            missing_env: Vec::new(),
        }
    }
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
pub enum TelemetryProtocol {
    #[default]
    #[serde(rename = "http/protobuf")]
    HttpProtobuf,
    #[serde(rename = "http/json")]
    HttpJson,
}

impl TelemetryConfig {
    pub const DEFAULT_SERVICE_NAME: &'static str = "dive";
    pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

    pub fn with_env(&self, env: &dyn Fn(&str) -> Option<String>) -> Result<Self, String> {
        let mut resolved = self.clone();
        if let Some(endpoint) = env("OTEL_EXPORTER_OTLP_ENDPOINT").filter(|v| !v.is_empty()) {
            resolved.endpoint = Some(endpoint);
        }
        if let Some(protocol) = env("OTEL_EXPORTER_OTLP_PROTOCOL") {
            resolved.protocol = match protocol.as_str() {
                "http/protobuf" => TelemetryProtocol::HttpProtobuf,
                "http/json" => TelemetryProtocol::HttpJson,
                other => {
                    let problem = format!(
                        "OTEL_EXPORTER_OTLP_PROTOCOL must be http/protobuf or http/json, got {other:?}"
                    );
                    return Err(problem);
                }
            };
        }
        if let Some(timeout) = env("OTEL_EXPORTER_OTLP_TIMEOUT") {
            let millis: u64 = timeout.trim().parse().map_err(|_| {
                format!(
                    "OTEL_EXPORTER_OTLP_TIMEOUT must be a number of milliseconds, got {timeout:?}"
                )
            })?;
            resolved.timeout_secs = Some(millis.div_ceil(1000).max(1));
        }
        if let Some(name) = env("OTEL_SERVICE_NAME").filter(|v| !v.is_empty()) {
            resolved.service_name = Some(name);
        }
        if let Some(headers) = env("OTEL_EXPORTER_OTLP_HEADERS") {
            for pair in headers.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                let (key, value) = pair.split_once('=').ok_or_else(|| {
                    format!("OTEL_EXPORTER_OTLP_HEADERS entry {pair:?} is not key=value")
                })?;
                resolved
                    .headers
                    .insert(key.trim().to_string(), value.trim().to_string());
            }
        }
        Ok(resolved)
    }

    pub fn service_name(&self) -> &str {
        self.service_name
            .as_deref()
            .unwrap_or(Self::DEFAULT_SERVICE_NAME)
    }

    pub fn timeout_secs(&self) -> u64 {
        self.timeout_secs.unwrap_or(Self::DEFAULT_TIMEOUT_SECS)
    }

    pub fn signal_url(&self, signal: &str) -> Option<String> {
        let base = self.endpoint.as_deref()?.trim_end_matches('/');
        Some(format!("{base}/{signal}"))
    }
}

/// Runtime-state storage. Defaults to plain files under `data_dir`, so a
/// fresh install needs zero configuration; `memory` runs ephemeral (CI
/// jobs, tests). Centralized backends (postgres/remote) slot in behind the
/// same trait.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct StorageConfig {
    pub backend: StorageBackend,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<FilesConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FilesConfig {
    pub backend: FilesBackend,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_key_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_access_key: Option<String>,
    #[serde(skip)]
    pub missing_env: Vec<MissingEnv>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FilesBackend {
    File,
    Memory,
    S3,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum FilesMode {
    #[default]
    Path,
    Url,
    Base64,
    Off,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum StorageBackend {
    #[default]
    File,
    Memory,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    pub data_dir: PathBuf,
    pub max_agent_iterations: u32,
    pub planning_attempts: u32,
    pub history_limit: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("~/.local/share/dive"),
            max_agent_iterations: 15,
            planning_attempts: 2,
            history_limit: 20,
        }
    }
}

/// A `${VAR}` reference whose variable was unset when the config loaded.
///
/// Recorded instead of failing the load, for the sections whose values are
/// only needed when the entry is actually *used* (`[providers.*]`,
/// `[mcp.*]`). The value keeps its literal `${VAR}` text; the component that
/// owns the entry reports this — with the variable name — the moment the
/// entry is exercised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingEnv {
    /// The field inside the entry, e.g. `api_key` or `headers.Authorization`.
    pub field: String,
    /// The environment variable that was not set.
    pub var: String,
}

impl MissingEnv {
    /// One sentence naming the variable and where the config references it,
    /// e.g. `environment variable ANTHROPIC_API_KEY (providers.anthropic.api_key) is not set`.
    pub fn describe(&self, entry: &str) -> String {
        format!(
            "environment variable {} ({entry}.{}) is not set; set it, or store it with `dive secret set {}`",
            self.var, self.field, self.var
        )
    }
}

/// Render every [`MissingEnv`] of one entry as a single reason string.
pub fn describe_missing_env(entry: &str, missing: &[MissingEnv]) -> String {
    missing
        .iter()
        .map(|m| m.describe(entry))
        .collect::<Vec<_>>()
        .join("; ")
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    #[serde(rename = "type")]
    pub kind: ProviderKind,
    /// API key; supports `${ENV_VAR}` expansion.
    pub api_key: Option<String>,
    /// Base URL for `openai_compat` (e.g. Ollama at http://localhost:11434/v1).
    pub base_url: Option<String>,
    /// Bedrock only.
    pub region: Option<String>,
    /// Bedrock only: AWS shared-config profile name.
    pub profile: Option<String>,
    /// Unset `${VAR}` references in this entry, recorded at load. A provider
    /// with any is unusable and says so when a model resolves to it.
    #[serde(skip)]
    pub missing_env: Vec<MissingEnv>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Anthropic,
    Openai,
    OpenaiCompat,
    Bedrock,
    Systemone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelKind {
    Chat,
    Decision,
}

impl ModelKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ModelKind::Chat => "chat",
            ModelKind::Decision => "decision",
        }
    }
}

impl std::fmt::Display for ModelKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ProviderKind {
    pub fn model_kind(self) -> ModelKind {
        match self {
            ProviderKind::Systemone => ModelKind::Decision,
            _ => ModelKind::Chat,
        }
    }
}

/// A role's resolved model choice.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelChoice {
    pub provider: String,
    pub model: String,
    pub temperature: Option<f32>,
    /// What this model is good for. A role carrying one is advertised to
    /// the planner as a routing signal wherever roles are selectable (e.g.
    /// `builtin__infer`'s `model` input), so write it for that audience.
    pub description: Option<String>,
    /// Failover candidates, tried in order when this model's provider is
    /// down (transient errors after its own retries are exhausted).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallbacks: Vec<FallbackChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
}

/// One failover candidate for a [`ModelChoice`]. Deliberately narrower than
/// `ModelChoice`: no description (never planner-routed on its own) and no
/// nested fallbacks (one flat chain per entry).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FallbackChoice {
    pub provider: String,
    pub model: String,
    /// Overrides the request temperature when set; otherwise the primary's
    /// effective temperature carries over.
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct ModelRoles(BTreeMap<String, ModelChoice>);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    Chat,
    Drafter,
    Planner,
    Outliner,
    Solver,
    Repair,
    Judge,
    Decider,
}

impl Role {
    pub const ALL: [Role; 8] = [
        Role::Chat,
        Role::Drafter,
        Role::Planner,
        Role::Outliner,
        Role::Solver,
        Role::Repair,
        Role::Judge,
        Role::Decider,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Chat => "chat",
            Role::Drafter => "drafter",
            Role::Planner => "planner",
            Role::Outliner => "outliner",
            Role::Solver => "solver",
            Role::Repair => "repair",
            Role::Judge => "judge",
            Role::Decider => "decider",
        }
    }

    pub fn kind(self) -> ModelKind {
        match self {
            Role::Decider => ModelKind::Decision,
            _ => ModelKind::Chat,
        }
    }

    pub fn falls_back_to_default(self) -> bool {
        self.kind() == ModelKind::Chat
    }

    pub fn from_name(name: &str) -> Option<Role> {
        Role::ALL.into_iter().find(|role| role.as_str() == name)
    }

    pub fn fallback(self) -> Option<Role> {
        match self {
            Role::Outliner => Some(Role::Planner),
            Role::Drafter => Some(Role::Chat),
            _ => None,
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

pub const DEFAULT_ROLE: &str = "default";

impl ModelRoles {
    pub fn new(roles: BTreeMap<String, ModelChoice>) -> Self {
        Self(roles)
    }

    pub fn get(&self, name: &str) -> Option<&ModelChoice> {
        self.0.get(name)
    }

    pub fn resolve(&self, name: &str) -> Option<&ModelChoice> {
        if let Some(choice) = self.0.get(name) {
            return Some(choice);
        }
        let role = Role::from_name(name)?;
        if !role.falls_back_to_default() {
            return None;
        }
        match role.fallback() {
            Some(fallback) => self.resolve(fallback.as_str()),
            None => self.0.get(DEFAULT_ROLE),
        }
    }

    pub fn resolve_role(&self, role: Role) -> Option<&ModelChoice> {
        self.resolve(role.as_str())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    pub fn known_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.names().collect();
        names.push(DEFAULT_ROLE);
        names.extend(Role::ALL.iter().map(|role| role.as_str()));
        names.retain(|name| self.resolve(name).is_some());
        names.sort_unstable();
        names.dedup();
        names
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &ModelChoice)> {
        self.0.iter().map(|(name, choice)| (name.as_str(), choice))
    }

    pub fn described(&self) -> impl Iterator<Item = (&str, &ModelChoice)> {
        self.iter()
            .filter(|(_, choice)| choice.description.is_some())
    }

    pub fn all_choices(&self) -> impl Iterator<Item = &ModelChoice> {
        self.0.values()
    }
}

impl FromIterator<(String, ModelChoice)> for ModelRoles {
    fn from_iter<I: IntoIterator<Item = (String, ModelChoice)>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl<const N: usize> From<[(&str, ModelChoice); N]> for ModelRoles {
    fn from(entries: [(&str, ModelChoice); N]) -> Self {
        entries
            .into_iter()
            .map(|(name, choice)| (name.to_string(), choice))
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// stdio transport: command to spawn.
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment for the spawned process; values support `${ENV_VAR}`.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Streamable-HTTP transport: server URL. Mutually exclusive with `command`.
    pub url: Option<String>,
    /// HTTP headers; values support `${ENV_VAR}`.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Only expose these tools (exact names, pre-namespacing).
    pub include_tools: Option<Vec<String>>,
    /// Hide these tools.
    #[serde(default)]
    pub exclude_tools: Vec<String>,
    /// Output schema/example overrides keyed by tool name.
    #[serde(default)]
    pub tool_overrides: BTreeMap<String, ToolOverride>,
    #[serde(default, skip_serializing_if = "is_default_files_mode")]
    pub files: FilesMode,
    /// Unset `${VAR}` references in this entry, recorded at load. A server
    /// with any refuses to connect and says so — a missing token must not
    /// silently become an empty header or child environment variable.
    #[serde(skip)]
    pub missing_env: Vec<MissingEnv>,
}

fn is_default_files_mode(mode: &FilesMode) -> bool {
    *mode == FilesMode::default()
}

impl McpServerConfig {
    pub fn validate(&self, name: &str) -> Result<(), String> {
        if crate::pack::RESERVED_NAMES.contains(&name) {
            return Err(format!(
                "[mcp.{name}]: '{name}' is reserved for Dive's own tool names ({}); rename the server",
                crate::pack::RESERVED_NAMES.join(", ")
            ));
        }
        match (&self.command, &self.url) {
            (Some(_), Some(_)) => Err(format!(
                "mcp server '{name}': `command` and `url` are mutually exclusive"
            )),
            (None, None) => Err(format!(
                "mcp server '{name}': one of `command` (stdio) or `url` (http) is required"
            )),
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct ToolOverride {
    pub description: Option<String>,
    pub output_schema: Option<serde_json::Value>,
    pub output_example: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct PlanPaths {
    pub paths: Vec<PathBuf>,
}

impl Default for PlanPaths {
    fn default() -> Self {
        Self {
            paths: vec![
                PathBuf::from("./.dive/plans"),
                PathBuf::from("~/.config/dive/plans"),
            ],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct ToolPaths {
    pub paths: Vec<PathBuf>,
    pub always_loaded: Vec<String>,
}

impl Default for ToolPaths {
    fn default() -> Self {
        Self {
            paths: vec![
                PathBuf::from("./.dive/tools"),
                PathBuf::from("~/.config/dive/tools"),
            ],
            always_loaded: [
                "builtin__infer",
                "builtin__reshape",
                "builtin__flatten",
                "builtin__dedupe",
                "builtin__sort",
                "builtin__take",
                "builtin__rank",
            ]
            .map(String::from)
            .to_vec(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct ProjectConfig {
    pub id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct UserConfig {
    pub name: Option<String>,
    /// Freeform context injected into the chat and planner prompts.
    pub context: Option<String>,
    pub timezone: Option<String>,
}

#[cfg(test)]
mod pricing_tests {
    use super::*;

    fn haiku() -> ModelPrice {
        ModelPrice {
            input: 0.10,
            output: 0.50,
            cache_write: None,
            cache_read: Some(0.01),
            tiers: vec![PriceTier {
                above: 100_000,
                input: Some(0.50),
                output: Some(2.50),
                cache_write: None,
                cache_read: None,
            }],
            source: None,
        }
    }

    fn rounded(usd: f64) -> f64 {
        (usd * 1e9).round() / 1e9
    }

    fn tokens(input: u64, output: u64, cache_write: u64, cache_read: u64) -> TokenCounts {
        TokenCounts {
            input,
            output,
            cache_write,
            cache_read,
        }
    }

    #[test]
    fn a_prompt_over_the_threshold_reprices_every_token_kind() {
        let cost = haiku().cost(&tokens(20_000, 2_000, 0, 85_000));
        assert_eq!(rounded(cost), 0.01925);
    }

    #[test]
    fn a_prompt_at_the_threshold_stays_on_the_base_rates() {
        let cost = haiku().cost(&tokens(100_000, 0, 0, 0));
        assert_eq!(rounded(cost), 0.01);
        let cost = haiku().cost(&tokens(100_001, 0, 0, 0));
        assert_eq!(rounded(cost), 0.0500005);
    }

    #[test]
    fn cache_reads_and_writes_count_toward_the_threshold() {
        let price = haiku();
        assert!(price
            .tier_for(tokens(1_000, 0, 50_000, 50_000).prompt())
            .is_some());
        assert!(price
            .tier_for(tokens(100_000, 50_000, 0, 0).prompt())
            .is_none());
    }

    #[test]
    fn a_tier_without_cache_rates_scales_them_with_input() {
        let rates = haiku().rates_for(200_000);
        assert_eq!(rounded(rates.cache_read), 0.05);
        assert_eq!(rounded(rates.cache_write), 0.625);
    }

    #[test]
    fn a_tier_without_an_output_rate_keeps_the_base_output() {
        let mut price = haiku();
        price.tiers[0].output = None;
        assert_eq!(price.rates_for(200_000).output, 0.50);
    }

    #[test]
    fn the_highest_tier_the_prompt_passes_wins() {
        let mut price = haiku();
        price.tiers.insert(
            0,
            PriceTier {
                above: 500_000,
                input: Some(1.0),
                output: None,
                cache_write: None,
                cache_read: None,
            },
        );
        assert_eq!(price.rates_for(150_000).input, 0.50);
        assert_eq!(price.rates_for(600_000).input, 1.0);
    }

    #[test]
    fn a_large_openai_prompt_doubles_input_and_lifts_output() {
        let price = ModelPrice {
            input: 5.0,
            output: 30.0,
            cache_write: None,
            cache_read: None,
            tiers: vec![PriceTier {
                above: 272_000,
                input: Some(10.0),
                output: Some(45.0),
                cache_write: None,
                cache_read: None,
            }],
            source: None,
        };
        assert_eq!(rounded(price.cost(&tokens(280_000, 4_000, 0, 0))), 2.98);
    }
}
