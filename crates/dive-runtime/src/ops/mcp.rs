use crate::ops::tool::ToolListing;
use crate::runtime::{system_sign_ins, Workspace};
use anyhow::{anyhow, Result};
use dive_core::ToolRegistry;
use dive_mcp::{McpManager, SignIn, SignInScope, SignIns};
use serde::Serialize;
use std::future::Future;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct McpServerSummary {
    pub name: String,
    pub transport: String,
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sign_in: Option<SignIn>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SignedOut {
    pub server: String,
    pub scope: SignInScope,
    pub removed: bool,
    pub now: Option<SignIn>,
}

pub fn list(config: &dive_config::Config, sign_ins: Option<&SignIns>) -> McpServerList {
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
            let sign_in = match (sign_ins, &server.url) {
                (Some(sign_ins), Some(url)) => sign_ins.status(url).ok().flatten(),
                _ => None,
            };
            McpServerSummary {
                name: name.clone(),
                transport: transport.to_string(),
                target,
                sign_in,
            }
        })
        .collect();
    McpServerList {
        count: servers.len(),
        servers,
    }
}

pub fn defined_in(workspace: &Workspace, server: &str) -> SignInScope {
    if workspace.is_global() {
        return SignInScope::Global;
    }
    let path = workspace.project_dir().join("config.toml");
    let defined = std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| raw.parse::<toml::Table>().ok())
        .and_then(|table| table.get("mcp")?.get(server).cloned())
        .is_some();
    if defined {
        SignInScope::Project
    } else {
        SignInScope::Global
    }
}

fn url_of<'a>(manager: &'a McpManager, server: &str) -> Result<&'a str> {
    if !manager.server_names().contains(&server) {
        return Err(anyhow!("no MCP server named '{server}' is configured"));
    }
    manager.url_of(server).ok_or_else(|| {
        anyhow!("mcp server '{server}' runs a command, and only servers with a `url` sign in")
    })
}

fn sign_ins_of(manager: &McpManager) -> SignIns {
    manager.sign_ins().unwrap_or_else(|| system_sign_ins(None))
}

pub async fn sign_in(
    manager: &McpManager,
    workspace: &Workspace,
    server: &str,
    scope: Option<SignInScope>,
    open: impl FnOnce(&str),
    cancelled: impl Future<Output = ()>,
) -> Result<SignIn> {
    let url = url_of(manager, server)?.to_string();
    let scope = scope.unwrap_or_else(|| defined_in(workspace, server));
    let project = match scope {
        SignInScope::Project if workspace.is_global() => {
            return Err(anyhow!(
                "there's no project here to sign in for; run this inside a project, or sign in globally"
            ))
        }
        SignInScope::Project => Some(dive_config::ensure_project_id(
            &workspace.project_dir().join("config.toml"),
        )?),
        SignInScope::Global => crate::secrets::project_id(workspace),
    };
    let sign_ins = sign_ins_of(manager).for_project(project);
    let signed = sign_ins
        .sign_in(server, &url, scope, open, cancelled)
        .await?;
    manager.disconnect(server).await;
    Ok(signed)
}

pub async fn sign_out(
    manager: &McpManager,
    workspace: &Workspace,
    server: &str,
    scope: Option<SignInScope>,
) -> Result<SignedOut> {
    let url = url_of(manager, server)?.to_string();
    let sign_ins = sign_ins_of(manager).for_project(crate::secrets::project_id(workspace));
    let scope = match scope {
        Some(scope) => scope,
        None => sign_ins
            .status(&url)?
            .map(|current| current.scope)
            .unwrap_or_else(|| defined_in(workspace, server)),
    };
    let removed = sign_ins.sign_out(&url, scope)?;
    manager.disconnect(server).await;
    Ok(SignedOut {
        server: server.to_string(),
        scope,
        removed,
        now: sign_ins.status(&url)?,
    })
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
        let listed = list(&config, None);
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

    fn project_with(config: &str) -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".dive")).unwrap();
        std::fs::write(dir.path().join(".dive/config.toml"), config).unwrap();
        let workspace = Workspace::new(dir.path());
        (dir, workspace)
    }

    #[test]
    fn a_server_signs_in_where_it_is_defined() {
        let (_dir, workspace) = project_with("[mcp.local]\nurl = \"https://local.example/mcp\"\n");
        assert_eq!(defined_in(&workspace, "local"), SignInScope::Project);
        assert_eq!(defined_in(&workspace, "linear"), SignInScope::Global);
    }

    #[tokio::test]
    async fn signing_out_of_an_override_falls_back_to_the_global_sign_in() {
        use dive_core::secrets::{MemorySignIns, SignInStore};
        let (_dir, workspace) = project_with("[project]\nid = \"acme\"\n");
        let url = "https://mcp.linear.app/mcp";
        let saved = |at: u64| {
            serde_json::json!({
                "signed_in_at": at,
                "credentials": {"client_id": "c", "token_response": null, "granted_scopes": [], "token_received_at": at}
            })
            .to_string()
        };
        let store = std::sync::Arc::new(MemorySignIns::new());
        store.save(url, None, &saved(10)).unwrap();
        store.save(url, Some("acme"), &saved(20)).unwrap();
        let config: dive_config::Config =
            toml::from_str(&format!("[mcp.linear]\nurl = \"{url}\"\n")).unwrap();
        let manager = McpManager::new(config.mcp.clone())
            .with_sign_ins(SignIns::new(store.clone(), Some("acme".to_string())));
        assert_eq!(
            list(&config, manager.sign_ins().as_ref()).servers[0]
                .sign_in
                .as_ref()
                .map(|s| s.scope),
            Some(SignInScope::Project)
        );
        let out = sign_out(&manager, &workspace, "linear", None)
            .await
            .unwrap();
        assert_eq!(out.scope, SignInScope::Project);
        assert!(out.removed);
        assert_eq!(
            out.now.map(|now| (now.scope, now.signed_in_at)),
            Some((SignInScope::Global, 10))
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn only_url_servers_sign_in() {
        let config: dive_config::Config =
            toml::from_str("[mcp.everything]\ncommand = \"npx\"\n").unwrap();
        let manager = McpManager::new(config.mcp.clone());
        let (_dir, workspace) = project_with("");
        let refused = sign_out(&manager, &workspace, "everything", None)
            .await
            .unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("only servers with a `url` sign in"),
            "{refused}"
        );
        let missing = sign_out(&manager, &workspace, "ghost", None)
            .await
            .unwrap_err();
        assert!(
            missing.to_string().contains("no MCP server named 'ghost'"),
            "{missing}"
        );
    }
}
