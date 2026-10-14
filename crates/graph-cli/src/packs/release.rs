use graph_core::packs::{interface_name, ItemKind, LoadedPack};
use semver::Version;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const MIGRATING_FILE: &str = "MIGRATING.md";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
pub enum Bump {
    Patch,
    Minor,
    Major,
}

impl Bump {
    pub fn as_str(self) -> &'static str {
        match self {
            Bump::Patch => "patch",
            Bump::Minor => "minor",
            Bump::Major => "major",
        }
    }

    pub fn next(self, previous: Option<&Version>) -> Version {
        match (self, previous) {
            (Bump::Major, None) => Version::new(1, 0, 0),
            (Bump::Minor, None) => Version::new(0, 1, 0),
            (Bump::Patch, None) => Version::new(0, 0, 1),
            (Bump::Major, Some(v)) => Version::new(v.major + 1, 0, 0),
            (Bump::Minor, Some(v)) => Version::new(v.major, v.minor + 1, 0),
            (Bump::Patch, Some(v)) => Version::new(v.major, v.minor, v.patch + 1),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub bump: Bump,
    pub what: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Signature {
    pub input: Option<Value>,
    pub output: Option<Value>,
}

pub fn interface(loaded: &LoadedPack) -> BTreeMap<String, Signature> {
    let public = |name: &str| !loaded.is_migration(name);
    let pack = &loaded.source.name;
    let tools = loaded
        .tools
        .iter()
        .filter(|doc| {
            public(&graph_core::packs::catalog_name(
                pack,
                ItemKind::Tool,
                &doc.name,
            ))
        })
        .map(|doc| {
            (
                interface_name(ItemKind::Tool, &doc.name),
                Signature {
                    input: doc.input_schema.clone(),
                    output: doc.output_schema.clone(),
                },
            )
        });
    let plans = loaded
        .plans
        .iter()
        .filter(|doc| public(&doc.identifier))
        .filter_map(|doc| {
            let (_, _, bare) = graph_core::packs::split_catalog_name(&doc.identifier)?;
            Some((
                interface_name(ItemKind::Plan, bare),
                Signature {
                    input: doc.input_schema.clone(),
                    output: doc.output_schema.clone(),
                },
            ))
        });
    let agents = loaded.agents.iter().filter_map(|doc| {
        let (_, _, bare) = graph_core::packs::split_catalog_name(&doc.name)?;
        Some((
            interface_name(ItemKind::Agent, bare),
            Signature {
                input: Some(doc.subagent_input_schema()),
                output: None,
            },
        ))
    });
    tools.chain(plans).chain(agents).collect()
}

pub fn compare(
    previous: &BTreeMap<String, Signature>,
    current: &BTreeMap<String, Signature>,
) -> Vec<Change> {
    let mut changes = Vec::new();
    let mut change = |bump: Bump, what: String| changes.push(Change { bump, what });
    for (name, before) in previous {
        let Some(after) = current.get(name) else {
            change(Bump::Major, format!("{name} removed or renamed"));
            continue;
        };
        compare_input(
            name,
            before.input.as_ref(),
            after.input.as_ref(),
            &mut change,
        );
        compare_output(
            name,
            before.output.as_ref(),
            after.output.as_ref(),
            &mut change,
        );
    }
    for name in current.keys().filter(|name| !previous.contains_key(*name)) {
        change(Bump::Minor, format!("{name} added"));
    }
    changes
}

pub fn required(changes: &[Change]) -> Bump {
    changes
        .iter()
        .map(|change| change.bump)
        .max()
        .unwrap_or(Bump::Patch)
}

fn properties(schema: Option<&Value>) -> BTreeMap<String, Option<Value>> {
    schema
        .and_then(|schema| schema.get("properties"))
        .and_then(Value::as_object)
        .map(|properties| {
            properties
                .iter()
                .map(|(name, property)| (name.clone(), property.get("type").cloned()))
                .collect()
        })
        .unwrap_or_default()
}

fn required_names(schema: Option<&Value>) -> BTreeSet<String> {
    schema
        .and_then(|schema| schema.get("required"))
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn retyped(before: &Option<Value>, after: &Option<Value>) -> Option<String> {
    match (before, after) {
        (Some(before), Some(after)) if before != after => Some(format!("from {before} to {after}")),
        _ => None,
    }
}

fn compare_input(
    name: &str,
    before: Option<&Value>,
    after: Option<&Value>,
    change: &mut impl FnMut(Bump, String),
) {
    let (old, new) = (properties(before), properties(after));
    let (was_required, now_required) = (required_names(before), required_names(after));
    for (property, kind) in &old {
        match new.get(property) {
            None => change(Bump::Major, format!("{name}: input '{property}' removed")),
            Some(after) => {
                if let Some(how) = retyped(kind, after) {
                    change(
                        Bump::Major,
                        format!("{name}: input '{property}' retyped {how}"),
                    );
                }
            }
        }
    }
    for property in now_required.difference(&was_required) {
        if old.contains_key(property) {
            change(
                Bump::Major,
                format!("{name}: input '{property}' is now required"),
            );
        } else {
            change(
                Bump::Major,
                format!("{name}: required input '{property}' added"),
            );
        }
    }
    for property in new.keys().filter(|property| !old.contains_key(*property)) {
        if !now_required.contains(property) {
            change(
                Bump::Minor,
                format!("{name}: optional input '{property}' added"),
            );
        }
    }
}

fn compare_output(
    name: &str,
    before: Option<&Value>,
    after: Option<&Value>,
    change: &mut impl FnMut(Bump, String),
) {
    match (before, after) {
        (None, None) => return,
        (Some(_), None) => {
            change(Bump::Major, format!("{name}: output schema removed"));
            return;
        }
        (None, Some(_)) => {
            change(Bump::Minor, format!("{name}: output schema added"));
            return;
        }
        (Some(_), Some(_)) => {}
    }
    let (old, new) = (properties(before), properties(after));
    for (property, kind) in &old {
        match new.get(property) {
            None => change(
                Bump::Major,
                format!("{name}: output property '{property}' removed"),
            ),
            Some(after) => {
                if let Some(how) = retyped(kind, after) {
                    change(
                        Bump::Major,
                        format!("{name}: output property '{property}' retyped {how}"),
                    );
                }
            }
        }
    }
    for property in new.keys().filter(|property| !old.contains_key(*property)) {
        change(
            Bump::Minor,
            format!("{name}: output property '{property}' added"),
        );
    }
}

pub fn migrating_section(dir: &Path, major: u64) -> bool {
    let heading = format!("## v{} to v{major}", major.saturating_sub(1));
    std::fs::read_to_string(dir.join(MIGRATING_FILE))
        .is_ok_and(|text| text.lines().any(|line| line.trim() == heading))
}

pub fn previous_release(tags: &[String], path: &str) -> Option<(String, Version)> {
    tags.iter()
        .filter_map(|tag| Some((tag.clone(), super::refs::release_version(path, tag)?)))
        .max_by(|a, b| a.1.cmp(&b.1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn signature(input: Value, output: Option<Value>) -> Signature {
        Signature {
            input: Some(input),
            output,
        }
    }

    fn interface_of(items: &[(&str, Signature)]) -> BTreeMap<String, Signature> {
        items
            .iter()
            .map(|(name, signature)| (name.to_string(), signature.clone()))
            .collect()
    }

    fn base() -> Signature {
        signature(
            json!({"type": "object", "required": ["file"], "properties": {"file": {"type": "string"}, "limit": {"type": "integer"}}}),
            Some(json!({"type": "object", "properties": {"path": {"type": "string"}}})),
        )
    }

    #[test]
    fn breaking_changes_need_a_major() {
        let before = interface_of(&[("tool__a", base()), ("tool__gone", base())]);
        let after = interface_of(&[(
            "tool__a",
            signature(
                json!({"type": "object", "required": ["file", "mode"], "properties": {"file": {"type": "integer"}, "mode": {"type": "string"}}}),
                Some(json!({"type": "object", "properties": {}})),
            ),
        )]);
        let changes = compare(&before, &after);
        let text: Vec<&str> = changes.iter().map(|c| c.what.as_str()).collect();
        assert!(text.contains(&"tool__gone removed or renamed"), "{text:?}");
        assert!(text.contains(&"tool__a: input 'limit' removed"), "{text:?}");
        assert!(
            text.contains(&"tool__a: input 'file' retyped from \"string\" to \"integer\""),
            "{text:?}"
        );
        assert!(
            text.contains(&"tool__a: required input 'mode' added"),
            "{text:?}"
        );
        assert!(
            text.contains(&"tool__a: output property 'path' removed"),
            "{text:?}"
        );
        assert_eq!(required(&changes), Bump::Major);

        let unschema = interface_of(&[("tool__a", signature(base().input.unwrap(), None))]);
        let changes = compare(&interface_of(&[("tool__a", base())]), &unschema);
        assert_eq!(changes[0].what, "tool__a: output schema removed");
        assert_eq!(required(&changes), Bump::Major);
    }

    #[test]
    fn additions_need_a_minor_and_nothing_needs_a_patch() {
        let before = interface_of(&[("tool__a", base())]);
        let mut wider = base();
        wider.input.as_mut().unwrap()["properties"]["extra"] = json!({"type": "boolean"});
        wider.output.as_mut().unwrap()["properties"]["size"] = json!({"type": "integer"});
        let after = interface_of(&[("tool__a", wider), ("plan__b", base())]);
        let changes = compare(&before, &after);
        let text: Vec<&str> = changes.iter().map(|c| c.what.as_str()).collect();
        assert_eq!(
            text,
            [
                "tool__a: optional input 'extra' added",
                "tool__a: output property 'size' added",
                "plan__b added"
            ]
        );
        assert_eq!(required(&changes), Bump::Minor);
        assert_eq!(required(&compare(&before, &before)), Bump::Patch);
    }

    #[test]
    fn versions_follow_the_bump_and_start_from_nothing() {
        let v = Version::new(1, 4, 2);
        assert_eq!(Bump::Major.next(Some(&v)), Version::new(2, 0, 0));
        assert_eq!(Bump::Minor.next(Some(&v)), Version::new(1, 5, 0));
        assert_eq!(Bump::Patch.next(Some(&v)), Version::new(1, 4, 3));
        assert_eq!(Bump::Major.next(None), Version::new(1, 0, 0));
        assert_eq!(Bump::Minor.next(None), Version::new(0, 1, 0));
        assert_eq!(Bump::Patch.next(None), Version::new(0, 0, 1));
        let tags = [
            "v1.0.0",
            "v1.10.0",
            "v1.9.0",
            "v2.0.0-rc.1",
            "latest",
            "a/v3.0.0",
            "a/v2.1.0",
        ]
        .map(String::from);
        assert_eq!(previous_release(&tags, "").unwrap().0, "v1.10.0");
        assert_eq!(previous_release(&tags, "a").unwrap().0, "a/v3.0.0");
        assert_eq!(previous_release(&tags, "b"), None);
    }
}
