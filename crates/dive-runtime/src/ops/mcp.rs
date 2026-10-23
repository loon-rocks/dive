use crate::ops::tool::ToolListing;
use anyhow::Result;
use dive_core::ToolRegistry;
use dive_mcp::McpManager;
use serde::Serialize;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct McpServerSummary {
    pub name: String,
    pub transport: String,
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct McpServerList {
    pub servers: Vec<McpServerSummary>,
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpTest {
    pub server: String,
    pub ok: bool,
    pub tools: usize,
    pub declare_output_schema: usize,
    pub elapsed_ms: u128,
    #[serde(skip)]
    pub elapsed: Duration,
}

pub fn list(config: &dive_config::Config) -> McpServerList {
    let servers: Vec<McpServerSummary> = config
        .mcp
        .iter()
        .map(|(name, server)| {
            let (transport, target) = match (&server.command, &server.url) {
                (Some(command), _) => (
                    "stdio",
                    format!("{} {}", command, server.args.join(" "))
                        .trim_end()
                        .to_string(),
                ),
                (_, Some(url)) => ("http", url.clone()),
                _ => ("invalid", String::new()),
            };
            McpServerSummary {
                name: name.clone(),
                transport: transport.to_string(),
                target,
            }
        })
        .collect();
    McpServerList {
        count: servers.len(),
        servers,
    }
}

pub async fn tools(manager: &McpManager, server: Option<&str>) -> Result<ToolListing> {
    let defs = match server {
        Some(name) => manager.connect(name).await?,
        None => manager.tools().await?,
    };
    Ok(ToolListing::new(defs))
}

pub async fn test(manager: &McpManager, server: &str) -> Result<McpTest> {
    let started = Instant::now();
    let defs = manager.connect(server).await?;
    let elapsed = started.elapsed();
    Ok(McpTest {
        server: server.to_string(),
        ok: true,
        tools: defs.len(),
        declare_output_schema: defs.iter().filter(|d| d.output_schema.is_some()).count(),
        elapsed_ms: elapsed.as_millis(),
        elapsed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn list_names_each_transport_and_target() {
        let config: dive_config::Config = toml::from_str(
            "[mcp.everything]\ncommand = \"npx\"\nargs = [\"-y\", \"server-everything\"]\n\n[mcp.linear]\nurl = \"https://mcp.linear.app/mcp\"\n\n[mcp.bare]\ncommand = \"serve\"\n",
        )
        .unwrap();
        let listed = list(&config);
        assert_eq!(listed.count, 3);
        assert_eq!(
            serde_json::to_value(&listed).unwrap()["servers"],
            json!([
                {"name": "bare", "transport": "stdio", "target": "serve"},
                {"name": "everything", "transport": "stdio", "target": "npx -y server-everything"},
                {"name": "linear", "transport": "http", "target": "https://mcp.linear.app/mcp"},
            ])
        );
    }

    #[tokio::test]
    async fn tools_and_test_fail_for_an_unconfigured_server() {
        let manager = McpManager::new(Default::default());
        assert!(tools(&manager, Some("ghost")).await.is_err());
        assert!(test(&manager, "ghost").await.is_err());
        assert_eq!(tools(&manager, None).await.unwrap().count, 0);
        manager.shutdown().await;
    }

    #[test]
    fn a_test_result_serializes_with_the_cli_keys() {
        let result = McpTest {
            server: "s".to_string(),
            ok: true,
            tools: 2,
            declare_output_schema: 1,
            elapsed_ms: 12,
            elapsed: Duration::from_millis(12),
        };
        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            json!({"server": "s", "ok": true, "tools": 2, "declareOutputSchema": 1, "elapsedMs": 12})
        );
    }
}
