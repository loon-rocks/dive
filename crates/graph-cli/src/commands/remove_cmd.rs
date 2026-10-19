use crate::commands::outcome::Outcome;
use anyhow::Result;
use graph_runtime::ops::remove::{self, ItemKind};
use serde_json::json;

pub fn run(kind: ItemKind, name: &str) -> Result<Outcome> {
    let runtime = crate::host::runtime()?;
    Ok(match remove::remove(&runtime, kind, name) {
        Ok(removed) => {
            Outcome::ok(serde_json::to_value(&removed)?).with_note(removed.lines().join("\n  "))
        }
        Err(error) => Outcome::rejected(json!({
            "error": error.to_string(),
            "kind": kind,
            "name": name,
        })),
    })
}
