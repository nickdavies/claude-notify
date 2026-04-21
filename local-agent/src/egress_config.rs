use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct LocalAgentConfig {
    #[serde(default)]
    pub egress: EgressConfig,
    #[serde(default = "default_managed_agent_runner")]
    pub default_agent: ManagedRunnerSpec,
    #[serde(default)]
    pub managed_runners: HashMap<String, ManagedRunnerSpec>,
}

impl Default for LocalAgentConfig {
    fn default() -> Self {
        Self {
            egress: EgressConfig::default(),
            default_agent: default_managed_agent_runner(),
            managed_runners: HashMap::new(),
        }
    }
}

impl LocalAgentConfig {
    pub fn managed_runner_for_operation(&self, operation: &str) -> Option<&ManagedRunnerSpec> {
        let key = operation.trim();
        if key.is_empty() {
            return None;
        }
        self.managed_runners.get(key)
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ManagedRunnerSpec {
    #[serde(default)]
    pub kind: ManagedRunnerKind,
    #[serde(default)]
    pub opencode: OpencodeRunnerConfig,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub container_image: Option<String>,
    #[serde(default)]
    pub file_mounts: Vec<ManagedRunnerFileMount>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ManagedRunnerKind {
    #[default]
    Generic,
    Opencode,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpencodeRunnerConfig {
    #[serde(default = "default_opencode_validation_attempts")]
    pub max_validation_attempts: u32,
}

impl Default for OpencodeRunnerConfig {
    fn default() -> Self {
        Self {
            max_validation_attempts: default_opencode_validation_attempts(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ManagedRunnerFileMount {
    pub host_path: String,
    pub container_path: String,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EgressConfig {
    #[serde(default)]
    pub llm: LlmHostConfig,
    #[serde(default)]
    pub build: BuildHostConfig,
    #[serde(default)]
    pub proxy: ProxyConfig,
}

impl Default for EgressConfig {
    fn default() -> Self {
        Self {
            llm: LlmHostConfig::default(),
            build: BuildHostConfig::default(),
            proxy: ProxyConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct LlmHostConfig {
    #[serde(default)]
    pub default_provider: Option<String>,
    #[serde(default)]
    pub providers: HashMap<String, Vec<String>>,
}

impl LlmHostConfig {
    pub fn hosts_for_provider(&self, provider: Option<&str>) -> Vec<String> {
        let provider_key = provider
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase())
            .or_else(|| {
                self.default_provider
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_ascii_lowercase())
            });

        let mut out = Vec::new();
        let Some(provider_key) = provider_key else {
            return out;
        };

        if let Some(hosts) = self.providers.get(&provider_key) {
            out.extend(normalize_hosts(hosts));
        }
        out
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct BuildHostConfig {
    #[serde(default = "default_github_hosts")]
    pub github_hosts: Vec<String>,
    #[serde(default = "default_go_registry_hosts")]
    pub go_registry_hosts: Vec<String>,
    #[serde(default = "default_crates_registry_hosts")]
    pub crates_registry_hosts: Vec<String>,
}

impl Default for BuildHostConfig {
    fn default() -> Self {
        Self {
            github_hosts: default_github_hosts(),
            go_registry_hosts: default_go_registry_hosts(),
            crates_registry_hosts: default_crates_registry_hosts(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProxyConfig {
    #[serde(default = "default_proxy_image")]
    pub image: String,
    #[serde(default = "default_proxy_port")]
    pub listen_port: u16,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            image: default_proxy_image(),
            listen_port: default_proxy_port(),
        }
    }
}

pub fn load_local_agent_config(path_override: Option<&Path>) -> anyhow::Result<LocalAgentConfig> {
    let path = match path_override {
        Some(path) => path.to_path_buf(),
        None => default_local_agent_config_path()?,
    };

    if !path.exists() {
        return Ok(LocalAgentConfig::default());
    }

    let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let mut parsed: LocalAgentConfig =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    normalize_config(&mut parsed);
    Ok(parsed)
}

fn normalize_config(config: &mut LocalAgentConfig) {
    config.egress.llm.providers = config
        .egress
        .llm
        .providers
        .iter()
        .map(|(provider, hosts)| (provider.to_ascii_lowercase(), normalize_hosts(hosts)))
        .collect();
    config.egress.build.github_hosts = normalize_hosts(&config.egress.build.github_hosts);
    config.egress.build.go_registry_hosts = normalize_hosts(&config.egress.build.go_registry_hosts);
    config.egress.build.crates_registry_hosts =
        normalize_hosts(&config.egress.build.crates_registry_hosts);

    normalize_runner_spec(&mut config.default_agent);
    config.managed_runners = config
        .managed_runners
        .drain()
        .map(|(name, mut spec)| {
            normalize_runner_spec(&mut spec);
            (name.trim().to_string(), spec)
        })
        .filter(|(name, _)| !name.is_empty())
        .collect();
}

fn normalize_runner_spec(spec: &mut ManagedRunnerSpec) {
    spec.command = spec.command.trim().to_string();
    spec.args = spec
        .args
        .iter()
        .map(|arg| arg.trim().to_string())
        .filter(|arg| !arg.is_empty())
        .collect();
    if spec
        .container_image
        .as_deref()
        .is_some_and(|image| image.trim().is_empty())
    {
        spec.container_image = None;
    }
    spec.file_mounts = spec
        .file_mounts
        .iter()
        .filter_map(|mount| {
            let host_path = mount.host_path.trim();
            let container_path = mount.container_path.trim();
            if host_path.is_empty() || container_path.is_empty() {
                return None;
            }
            Some(ManagedRunnerFileMount {
                host_path: host_path.to_string(),
                container_path: container_path.to_string(),
                read_only: mount.read_only,
            })
        })
        .collect();
    if spec.opencode.max_validation_attempts == 0 {
        spec.opencode.max_validation_attempts = default_opencode_validation_attempts();
    }
}

pub fn normalize_hosts(hosts: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();

    for host in hosts {
        let host = host.trim().to_ascii_lowercase();
        if host.is_empty() {
            continue;
        }

        if host.contains("://") || host.contains('/') || host.contains(char::is_whitespace) {
            continue;
        }

        let normalized = host.trim_start_matches(['*', '.']).to_string();
        if normalized.is_empty() {
            continue;
        }
        if seen.insert(normalized.clone()) {
            out.push(normalized);
        }
    }

    out
}

fn default_local_agent_config_path() -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set; pass --local-agent-config")?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("agent-hub")
        .join("local-agent.json"))
}

fn default_proxy_image() -> String {
    // Use pinned stable image digest to avoid beta/default-latest drift.
    "ubuntu/squid@sha256:6a097f68bae708cedbabd6188d68c7e2e7a38cedd05a176e1cc0ba29e3bbe029"
        .to_string()
}

fn default_managed_agent_runner() -> ManagedRunnerSpec {
    ManagedRunnerSpec {
        kind: ManagedRunnerKind::Opencode,
        opencode: OpencodeRunnerConfig::default(),
        command: "sh".to_string(),
        args: vec![
            "-lc".to_string(),
            "if [ -f \"$AGENT_PROMPT_USER_PATH\" ]; then opencode run < \"$AGENT_PROMPT_USER_PATH\"; else opencode run; fi".to_string(),
        ],
        container_image: None,
        file_mounts: Vec::new(),
    }
}

const fn default_proxy_port() -> u16 {
    3128
}

fn default_github_hosts() -> Vec<String> {
    vec![
        "github.com".to_string(),
        "api.github.com".to_string(),
        "objects.githubusercontent.com".to_string(),
        "codeload.github.com".to_string(),
        "raw.githubusercontent.com".to_string(),
    ]
}

fn default_go_registry_hosts() -> Vec<String> {
    vec![
        "proxy.golang.org".to_string(),
        "sum.golang.org".to_string(),
        "go.dev".to_string(),
    ]
}

fn default_crates_registry_hosts() -> Vec<String> {
    vec![
        "index.crates.io".to_string(),
        "static.crates.io".to_string(),
        "crates.io".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn llm_hosts_resolve_from_provider_or_default() {
        let cfg = LlmHostConfig {
            default_provider: Some("openai".to_string()),
            providers: HashMap::from([
                (
                    "openai".to_string(),
                    vec!["api.openai.com".to_string(), "api.openai.com".to_string()],
                ),
                (
                    "anthropic".to_string(),
                    vec!["api.anthropic.com".to_string()],
                ),
            ]),
        };

        assert_eq!(
            cfg.hosts_for_provider(Some("anthropic")),
            vec!["api.anthropic.com"]
        );
        assert_eq!(cfg.hosts_for_provider(None), vec!["api.openai.com"]);
    }

    #[test]
    fn normalize_hosts_skips_invalid_values() {
        let hosts = normalize_hosts(&[
            "api.openai.com".to_string(),
            "https://api.openai.com".to_string(),
            " api.openai.com ".to_string(),
            "*.github.com".to_string(),
            ".objects.githubusercontent.com".to_string(),
            "/tmp/sock".to_string(),
        ]);

        assert_eq!(
            hosts,
            vec![
                "api.openai.com",
                "github.com",
                "objects.githubusercontent.com"
            ]
        );
    }

    #[test]
    fn normalize_runner_spec_strips_blank_fields() {
        let mut config = LocalAgentConfig {
            egress: EgressConfig::default(),
            default_agent: ManagedRunnerSpec {
                kind: ManagedRunnerKind::Generic,
                opencode: OpencodeRunnerConfig::default(),
                command: " sh ".to_string(),
                args: vec![" -lc ".to_string(), " ".to_string()],
                container_image: Some(" ".to_string()),
                file_mounts: vec![ManagedRunnerFileMount {
                    host_path: " ".to_string(),
                    container_path: "/x".to_string(),
                    read_only: true,
                }],
            },
            managed_runners: HashMap::from([(
                " deploy ".to_string(),
                ManagedRunnerSpec {
                    kind: ManagedRunnerKind::Generic,
                    opencode: OpencodeRunnerConfig::default(),
                    command: " runner ".to_string(),
                    args: vec!["--go".to_string()],
                    container_image: None,
                    file_mounts: vec![ManagedRunnerFileMount {
                        host_path: "/a ".to_string(),
                        container_path: " /b".to_string(),
                        read_only: false,
                    }],
                },
            )]),
        };

        normalize_config(&mut config);
        assert_eq!(config.default_agent.command, "sh");
        assert_eq!(config.default_agent.args, vec!["-lc"]);
        assert!(config.default_agent.container_image.is_none());
        assert!(config.default_agent.file_mounts.is_empty());
        assert_eq!(config.default_agent.kind, ManagedRunnerKind::Generic);
        assert_eq!(config.default_agent.opencode.max_validation_attempts, 3);
        assert!(config.managed_runners.contains_key("deploy"));
        assert_eq!(
            config.managed_runners["deploy"].file_mounts[0].host_path,
            "/a"
        );
        assert_eq!(
            config.managed_runners["deploy"].file_mounts[0].container_path,
            "/b"
        );
    }

    #[test]
    fn default_managed_runner_uses_opencode_run_non_interactive() {
        let runner = default_managed_agent_runner();

        assert_eq!(runner.kind, ManagedRunnerKind::Opencode);
        assert_eq!(runner.opencode.max_validation_attempts, 3);
        assert_eq!(runner.command, "sh");
        assert_eq!(runner.args[0], "-lc");
        assert!(runner.args[1].contains("opencode run"));
        assert!(runner.args[1].contains("$AGENT_PROMPT_USER_PATH"));
    }
}

const fn default_opencode_validation_attempts() -> u32 {
    3
}
