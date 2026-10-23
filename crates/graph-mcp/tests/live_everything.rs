//! Live integration test against @modelcontextprotocol/server-everything.
//! Requires npx; run explicitly: cargo test -p graph-mcp -- --ignored

use graph_core::ToolRegistry;
use graph_mcp::McpManager;
use serde_json::json;
use std::collections::BTreeMap;

fn manager() -> McpManager {
    let config: graph_config::McpServerConfig = toml::from_str(
        r#"
        command = "npx"
        args = ["-y", "@modelcontextprotocol/server-everything"]
        "#,
    )
    .unwrap();
    McpManager::new(BTreeMap::from([("everything".to_string(), config)]))
}

#[tokio::test]
#[ignore = "requires npx and network"]
async fn discovers_and_invokes_tools() {
    let manager = manager();

    let tools = manager.tools().await.unwrap();
    assert!(tools.iter().any(|t| t.name == "everything__echo"));

    // Text result parses through the text→JSON fallback.
    let echoed = manager
        .invoke("everything__echo", json!({"message": "hello graph"}))
        .await
        .unwrap();
    assert!(!echoed.is_error);
    assert!(echoed.result.to_string().contains("hello graph"));

    // structuredContent is preferred when the server provides it.
    let structured = manager
        .invoke(
            "everything__get-structured-content",
            json!({"location": "Chicago"}),
        )
        .await
        .unwrap();
    assert!(!structured.is_error);
    assert!(
        structured.result.is_object() && structured.result.get("text").is_none(),
        "expected structured object, got: {}",
        structured.result
    );

    // Unknown tools error cleanly.
    if let Ok(outcome) = manager.invoke("everything__nope", json!({})).await {
        assert!(outcome.is_error)
    }
}

#[tokio::test]
#[ignore = "requires npx and network"]
async fn reports_server_info_and_tools_for_pack_requirements() {
    let manager = manager();
    let identity = manager
        .server_identity("everything")
        .await
        .expect("configured")
        .expect("connects");
    assert!(!identity.name.is_empty());
    assert!(semver_like(&identity.version), "{}", identity.version);
    assert!(identity.tools.iter().any(|t| t.name == "everything__echo"));
    assert!(manager.server_identity("missing").await.is_none());
    manager.shutdown().await;
}

fn semver_like(version: &str) -> bool {
    version.split('.').count() >= 2
}

#[tokio::test]
#[ignore = "requires npx and network"]
async fn images_a_tool_returns_are_stored_and_listed_as_file_references() {
    use graph_core::files::{BlobRef, BlobStore};
    let files = std::sync::Arc::new(graph_store::ObjectFiles::memory().unwrap());
    let manager = manager().with_files(files.clone());
    let tools = manager.tools().await.unwrap();
    let image_tool = tools
        .iter()
        .find(|t| t.name.contains("tiny-image") || t.name.contains("TinyImage"))
        .unwrap_or_else(|| {
            panic!(
                "no tiny image tool in {:?}",
                tools.iter().map(|t| &t.name).collect::<Vec<_>>()
            )
        })
        .name
        .clone();
    let outcome = manager.invoke(&image_tool, json!({})).await.unwrap();
    assert!(!outcome.is_error, "{}", outcome.result);
    let listed = outcome.result["files"]
        .as_array()
        .unwrap_or_else(|| panic!("no files in {}", outcome.result));
    assert_eq!(listed.len(), 1, "{}", outcome.result);
    assert_eq!(listed[0]["mime"], json!("image/png"));
    let blob = BlobRef::parse(listed[0]["ref"].as_str().unwrap()).unwrap();
    let bytes = files.get(&blob).await.unwrap();
    assert!(bytes.starts_with(b"\x89PNG"), "not a PNG");
    assert!(listed[0]["width"].as_u64().is_some_and(|w| w > 0));
    manager.shutdown().await;
}
