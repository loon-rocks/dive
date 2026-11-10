use std::collections::BTreeSet;

use super::*;
use crate::agent::doc::{parse_agent_source, AgentDoc};

fn plan(yaml: &str) -> PlanDoc {
    yaml.parse().unwrap()
}

fn agent(name: &str, extra: &str) -> AgentDoc {
    parse_agent_source(&format!(
        "name: {name}\ndescription: d\nmodel: chat\nsystem_prompt: p\n{extra}"
    ))
    .unwrap()
}

fn catalog() -> ToolCatalog {
    ToolCatalog {
        builtin_tools: BTreeSet::from([
            "builtin__infer".to_string(),
            "builtin__reshape".to_string(),
        ]),
        user_tools: BTreeSet::from(["user__searxng_search".to_string()]),
        mcp_servers: BTreeSet::from(["linear".to_string()]),
        ..Default::default()
    }
}

const READ_SOURCES: &str = r#"
identifier: read_sources
name: Read sources
description: d
steps:
  - id: sources
    tool_name: map
    input:
      over: "{{input.urls}}"
      do:
        - id: page
          tool_name: user__crawl4ai_markdown
          input: {}
        - id: gate
          tool_name: route
          input:
            if: { value: "{{page.success}}", op: eq, to: true }
            then:
              - id: extract
                tool_name: builtin__infer
                input: { instruction: x }
"#;

const WEB_SEARCH_RANKED: &str = r#"
identifier: web_search_ranked
name: Ranked
description: d
steps:
  - id: s
    tool_name: user__searxng_search
    input: {}
"#;

fn canvas() -> Requirements {
    let plans = vec![plan(READ_SOURCES), plan(WEB_SEARCH_RANKED)];
    let agents = AgentSet::layered(
        Vec::new(),
        vec![vec![
            agent("chat", "tools: []\nsubagents: [researcher]\n"),
            agent(
                "researcher",
                "tools: [plan__web_search_ranked, plan__read_sources]\n",
            ),
        ]],
    );
    Requirements::compute(&plans, &agents, &catalog())
}

fn crawl_leaf() -> Hop {
    Hop {
        item: Item::Tool("user__crawl4ai_markdown".to_string()),
        via: Via::Step("sources/page".to_string()),
    }
}

#[test]
fn a_plan_reports_the_missing_tool_with_its_step() {
    let requirements = canvas();
    assert_eq!(
        requirements.missing(&Item::Plan("read_sources".to_string())),
        [Missing {
            item: Item::Tool("user__crawl4ai_markdown".to_string()),
            reason: Reason::NotInCatalog,
            path: vec![crawl_leaf()],
        }]
    );
}

#[test]
fn an_agent_inherits_the_missing_tool_through_the_plan_it_calls() {
    let requirements = canvas();
    assert_eq!(
        requirements.missing(&Item::Agent("researcher".to_string()))[0].path,
        vec![
            Hop {
                item: Item::Plan("read_sources".to_string()),
                via: Via::Tool,
            },
            crawl_leaf(),
        ]
    );
}

#[test]
fn a_caller_two_levels_up_gets_the_whole_path() {
    let requirements = canvas();
    assert_eq!(
        requirements.missing(&Item::Agent("chat".to_string()))[0].path,
        vec![
            Hop {
                item: Item::Agent("researcher".to_string()),
                via: Via::Subagent,
            },
            Hop {
                item: Item::Plan("read_sources".to_string()),
                via: Via::Tool,
            },
            crawl_leaf(),
        ]
    );
}

#[test]
fn an_item_whose_references_all_resolve_is_runnable() {
    let requirements = canvas();
    assert!(requirements.runnable(&Item::Plan("web_search_ranked".to_string())));
}

#[test]
fn an_unknown_item_is_runnable() {
    assert!(canvas().runnable(&Item::Plan("nope".to_string())));
}

#[test]
fn an_unconfigured_requires_server_is_missing_via_requires_servers() {
    let doc = plan("identifier: p\nname: p\ndescription: d\nrequires_servers: [github]\nsteps:\n  - id: a\n    tool_name: builtin__infer\n    input: {}\n");
    let requirements = Requirements::compute(&[doc], &AgentSet::default(), &catalog());
    assert_eq!(
        requirements.missing(&Item::Plan("p".to_string())),
        [Missing {
            item: Item::Server("github".to_string()),
            reason: Reason::ServerNotConfigured,
            path: vec![Hop {
                item: Item::Server("github".to_string()),
                via: Via::RequiresServers,
            }],
        }]
    );
}

#[test]
fn an_unconfigured_server_tool_names_the_server() {
    let doc = plan("identifier: p\nname: p\ndescription: d\nsteps:\n  - id: a\n    tool_name: github__list_prs\n    input: {}\n");
    let requirements = Requirements::compute(&[doc], &AgentSet::default(), &catalog());
    assert_eq!(
        requirements.missing(&Item::Plan("p".to_string()))[0].item,
        Item::Server("github".to_string())
    );
}

#[test]
fn a_configured_server_tool_is_not_missing() {
    let doc = plan("identifier: p\nname: p\ndescription: d\nsteps:\n  - id: a\n    tool_name: linear__list_issues\n    input: {}\n");
    let requirements = Requirements::compute(&[doc], &AgentSet::default(), &catalog());
    assert!(requirements.runnable(&Item::Plan("p".to_string())));
}

#[test]
fn a_pattern_that_matches_no_user_tool_is_missing() {
    let agents = AgentSet::layered(
        Vec::new(),
        vec![vec![agent("a", "tools: [user__repo_*]\n")]],
    );
    let requirements = Requirements::compute(&[], &agents, &catalog());
    assert_eq!(
        requirements.missing(&Item::Agent("a".to_string()))[0].reason,
        Reason::PatternMatchesNothing
    );
}

#[test]
fn a_pattern_that_matches_a_user_tool_is_not_missing() {
    let agents = AgentSet::layered(
        Vec::new(),
        vec![vec![agent("a", "tools: [user__searx*]\n")]],
    );
    let requirements = Requirements::compute(&[], &agents, &catalog());
    assert!(requirements.runnable(&Item::Agent("a".to_string())));
}

#[test]
fn a_wildcard_namespace_is_never_missing() {
    let agents = AgentSet::layered(
        Vec::new(),
        vec![vec![agent("a", "tools: [\"*__search\", \"*\"]\n")]],
    );
    let requirements = Requirements::compute(&[], &agents, &catalog());
    assert!(requirements.runnable(&Item::Agent("a".to_string())));
}

#[test]
fn a_missing_plan_reference_is_not_in_catalog() {
    let agents = AgentSet::layered(Vec::new(), vec![vec![agent("a", "tools: [plan__gone]\n")]]);
    let requirements = Requirements::compute(&[], &agents, &catalog());
    assert_eq!(
        requirements.missing(&Item::Agent("a".to_string())),
        [Missing {
            item: Item::Plan("gone".to_string()),
            reason: Reason::NotInCatalog,
            path: vec![Hop {
                item: Item::Plan("gone".to_string()),
                via: Via::Tool,
            }],
        }]
    );
}

#[test]
fn a_missing_subagent_is_not_in_catalog_via_subagent() {
    let agents = AgentSet::layered(
        Vec::new(),
        vec![vec![agent("a", "tools: []\nsubagents: [ghost]\n")]],
    );
    let requirements = Requirements::compute(&[], &agents, &catalog());
    assert_eq!(
        requirements.missing(&Item::Agent("a".to_string()))[0].path,
        vec![Hop {
            item: Item::Agent("ghost".to_string()),
            via: Via::Subagent,
        }]
    );
}

#[test]
fn a_missing_handoff_target_is_not_in_catalog_via_handoff() {
    let agents = AgentSet::layered(
        Vec::new(),
        vec![vec![agent("a", "tools: []\nhandoffs: [ghost]\n")]],
    );
    let requirements = Requirements::compute(&[], &agents, &catalog());
    assert_eq!(
        requirements.missing(&Item::Agent("a".to_string()))[0].path,
        vec![Hop {
            item: Item::Agent("ghost".to_string()),
            via: Via::Handoff,
        }]
    );
}

#[test]
fn an_agent_tool_from_a_pack_that_is_not_installed_is_skipped() {
    let agents = AgentSet::layered(
        Vec::new(),
        vec![vec![agent("a", "tools: [web__tool__search]\n")]],
    );
    let requirements = Requirements::compute(&[], &agents, &catalog());
    assert!(requirements.runnable(&Item::Agent("a".to_string())));
}

#[test]
fn a_plan_step_on_a_pack_that_is_not_installed_is_missing() {
    let doc = plan("identifier: p\nname: p\ndescription: d\nsteps:\n  - id: a\n    tool_name: web__tool__search\n    input: {}\n");
    let requirements = Requirements::compute(&[doc], &AgentSet::default(), &catalog());
    assert_eq!(
        requirements.missing(&Item::Plan("p".to_string()))[0].item,
        Item::Tool("web__tool__search".to_string())
    );
}

#[test]
fn workbench_and_native_tools_are_never_missing() {
    let agents = AgentSet::layered(
        Vec::new(),
        vec![vec![agent(
            "a",
            "tools: [workbench__set_artifact, builtin__validate_plan]\n",
        )]],
    );
    let requirements = Requirements::compute(&[], &agents, &catalog());
    assert!(requirements.runnable(&Item::Agent("a".to_string())));
}

#[test]
fn two_missing_leaves_are_ordered_by_path_length() {
    let plans = vec![plan(READ_SOURCES)];
    let agents = AgentSet::layered(
        Vec::new(),
        vec![vec![agent(
            "a",
            "tools: [plan__read_sources, user__gone]\n",
        )]],
    );
    let requirements = Requirements::compute(&plans, &agents, &catalog());
    let items: Vec<&Item> = requirements
        .missing(&Item::Agent("a".to_string()))
        .iter()
        .map(|missing| &missing.item)
        .collect();
    assert_eq!(
        items,
        [
            &Item::Tool("user__gone".to_string()),
            &Item::Tool("user__crawl4ai_markdown".to_string()),
        ]
    );
}

#[test]
fn the_same_leaf_reached_twice_keeps_the_shortest_path() {
    let plans = vec![plan(READ_SOURCES)];
    let agents = AgentSet::layered(
        Vec::new(),
        vec![vec![agent(
            "a",
            "tools: [plan__read_sources, user__crawl4ai_markdown]\n",
        )]],
    );
    let requirements = Requirements::compute(&plans, &agents, &catalog());
    assert_eq!(
        requirements.missing(&Item::Agent("a".to_string())),
        [Missing {
            item: Item::Tool("user__crawl4ai_markdown".to_string()),
            reason: Reason::NotInCatalog,
            path: vec![Hop {
                item: Item::Tool("user__crawl4ai_markdown".to_string()),
                via: Via::Tool,
            }],
        }]
    );
}

#[test]
fn a_handoff_cycle_terminates_and_carries_the_other_members_missing_tool() {
    let agents = AgentSet::layered(
        Vec::new(),
        vec![vec![
            agent("a", "tools: []\nhandoffs: [b]\n"),
            agent("b", "tools: [user__gone]\nhandoffs: [a]\n"),
        ]],
    );
    let requirements = Requirements::compute(&[], &agents, &catalog());
    assert_eq!(
        requirements.missing(&Item::Agent("a".to_string()))[0].item,
        Item::Tool("user__gone".to_string())
    );
}

#[test]
fn a_plan_cycle_with_resolvable_steps_is_runnable() {
    let plans = vec![
        plan("identifier: a\nname: a\ndescription: d\nsteps:\n  - id: s\n    tool_name: plan__b\n    input: {}\n"),
        plan("identifier: b\nname: b\ndescription: d\nsteps:\n  - id: s\n    tool_name: plan__a\n    input: {}\n"),
    ];
    let requirements = Requirements::compute(&plans, &AgentSet::default(), &catalog());
    assert!(requirements.runnable(&Item::Plan("a".to_string())));
}

#[test]
fn a_missing_entry_serializes_with_kind_and_via_tags() {
    let requirements = canvas();
    let json =
        serde_json::to_value(&requirements.missing(&Item::Agent("researcher".to_string()))[0])
            .unwrap();
    assert_eq!(
        json,
        serde_json::json!({
            "item": {"kind": "tool", "name": "user__crawl4ai_markdown"},
            "reason": "not_in_catalog",
            "path": [
                {"item": {"kind": "plan", "name": "read_sources"}, "via": "tool"},
                {"item": {"kind": "tool", "name": "user__crawl4ai_markdown"}, "via": "step", "step": "sources/page"}
            ]
        })
    );
}
