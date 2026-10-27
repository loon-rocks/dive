use super::{visible, Origin, PackItem, PackSet, KIND_SEPARATOR};
use crate::pipeline::catalog::{agent_tool_patterns, glob_matches, step_tools};
use crate::pipeline::Plan;
use crate::tools::ToolDef;

pub fn is_public(item: &PackItem, include: &[String], exclude: &[String]) -> bool {
    let name = item.interface_name();
    let included = include.is_empty() || include.iter().any(|p| glob_matches(p, &name));
    included && !exclude.iter().any(|p| glob_matches(p, &name))
}

fn hidden_message(item: &PackItem) -> String {
    format!(
        "{} '{}' is not in pack '{}''s public interface (narrowed by [packs.{}] include/exclude); \
         only the pack's own plans and agents can call it",
        item.kind.as_str(),
        item.catalog_name(),
        item.pack,
        item.pack,
    )
}

impl PackSet {
    pub fn refusal(&self, caller: &Origin, name: &str) -> Option<String> {
        let item = self.item(name)?;
        (!visible(caller, item)).then(|| hidden_message(item))
    }

    pub fn shows(&self, caller: &Origin, name: &str) -> bool {
        self.item(name).is_none_or(|item| visible(caller, item))
    }

    pub fn retain_shown(&self, caller: &Origin, defs: &mut Vec<ToolDef>) {
        defs.retain(|def| self.shows(caller, &def.name));
    }

    fn pattern_refusal(&self, caller: &Origin, pattern: &str) -> Option<String> {
        if !pattern.contains('*') {
            return self.refusal(caller, pattern);
        }
        let (pack, _) = pattern.split_once(KIND_SEPARATOR)?;
        let matched: Vec<&PackItem> = self.matching(pattern).collect();
        if matched.is_empty() || matched.iter().any(|item| visible(caller, item)) {
            return None;
        }
        Some(format!(
            "`tools` pattern '{pattern}' matches only items outside pack '{pack}''s public \
             interface (narrowed by [packs.{pack}] include/exclude); only the pack's own plans \
             and agents can call them"
        ))
    }

    pub fn plan_problems(&self, caller: &Origin, plan: &Plan) -> Vec<String> {
        if self.is_empty() {
            return Vec::new();
        }
        let calls = step_tools(plan)
            .into_iter()
            .filter_map(|(step, tool)| Some((step, self.refusal(caller, &tool)?)));
        let patterns = agent_tool_patterns(plan)
            .into_iter()
            .filter_map(|(step, pattern)| Some((step, self.pattern_refusal(caller, &pattern)?)));
        calls
            .chain(patterns)
            .map(|(step, problem)| format!("step {step}: {problem}"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packs::{apply_interface, ItemKind};
    use crate::pipeline::doc::PlanDoc;

    fn item(pack: &str, kind: ItemKind, name: &str, public: bool) -> PackItem {
        PackItem {
            pack: pack.to_string(),
            kind,
            name: name.to_string(),
            public,
        }
    }

    fn example() -> PackSet {
        let mut set = PackSet::default();
        set.insert(item("example", ItemKind::Plan, "describe_file", true));
        set.insert(item("example", ItemKind::Tool, "file_info", false));
        set.insert(item("example", ItemKind::Agent, "reviewer", false));
        set.insert(item("other", ItemKind::Plan, "peek", true));
        set
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn interface(include: &[&str], exclude: &[&str]) -> Vec<(String, bool)> {
        let mut items = vec![
            item("example", ItemKind::Plan, "describe_file", false),
            item("example", ItemKind::Plan, "summarize", false),
            item("example", ItemKind::Tool, "file_info", false),
            item("example", ItemKind::Agent, "reviewer", false),
        ];
        apply_interface(&mut items, &strings(include), &strings(exclude));
        items
            .into_iter()
            .map(|item| (item.interface_name(), item.public))
            .collect()
    }

    fn public_names(include: &[&str], exclude: &[&str]) -> Vec<String> {
        interface(include, exclude)
            .into_iter()
            .filter(|(_, public)| *public)
            .map(|(name, _)| name)
            .collect()
    }

    #[test]
    fn no_include_or_exclude_makes_everything_public() {
        assert_eq!(public_names(&[], &[]).len(), 4);
    }

    #[test]
    fn include_globs_narrow_the_interface_to_what_they_match() {
        assert_eq!(
            public_names(&["plan__*"], &[]),
            strings(&["plan__describe_file", "plan__summarize"])
        );
        assert_eq!(
            public_names(&["plan__describe_file", "agent__*"], &[]),
            strings(&["plan__describe_file", "agent__reviewer"])
        );
    }

    #[test]
    fn exclude_wins_over_include() {
        assert_eq!(
            public_names(&[], &["tool__*", "plan__summarize"]),
            strings(&["plan__describe_file", "agent__reviewer"])
        );
        assert_eq!(
            public_names(&["plan__*"], &["plan__summarize"]),
            strings(&["plan__describe_file"])
        );
    }

    #[test]
    fn globs_match_the_interface_name_not_the_catalog_name() {
        assert!(public_names(&["example__plan__*"], &[]).is_empty());
        assert_eq!(public_names(&[], &["example__tool__*"]).len(), 4);
    }

    #[test]
    fn apply_interface_resets_public_from_the_patterns() {
        let mut items = vec![item("example", ItemKind::Tool, "file_info", true)];
        apply_interface(&mut items, &[], &strings(&["tool__file_info"]));
        assert!(!items[0].public);
    }

    #[test]
    fn the_origin_is_the_pack_a_file_came_from() {
        let source = crate::packs::PackSource {
            name: "example".to_string(),
            tag: None,
            layer: dive_config::pack::LayerKind::Project,
            dir: std::path::PathBuf::from(".dive/packs/example"),
        };
        assert_eq!(Origin::of(Some(&source)), Origin::Pack("example".into()));
        assert_eq!(Origin::of(None), Origin::Outside);
    }

    #[test]
    fn refusals_name_the_item_and_its_pack() {
        let set = example();
        let message = set
            .refusal(&Origin::Outside, "example__tool__file_info")
            .unwrap();
        assert!(message.contains("'example__tool__file_info'"), "{message}");
        assert!(message.contains("pack 'example'"), "{message}");
        assert!(set
            .refusal(&Origin::Pack("example".into()), "example__tool__file_info")
            .is_none());
        assert!(set
            .refusal(&Origin::Pack("other".into()), "example__tool__file_info")
            .is_some());
        assert!(set
            .refusal(&Origin::Outside, "example__plan__describe_file")
            .is_none());
        assert!(set
            .refusal(&Origin::Outside, "linear__list_issues")
            .is_none());
    }

    #[test]
    fn prefixed_aliases_never_reach_pack_items() {
        let set = example();
        assert_eq!(set.agent_target("agent__example__agent__reviewer"), None);
        assert_eq!(set.plan_target("plan__example__plan__describe_file"), None);
        assert_eq!(
            set.agent_target("example__agent__reviewer"),
            Some("example__agent__reviewer")
        );
    }

    #[test]
    fn retain_shown_drops_hidden_defs() {
        let set = example();
        let def = |name: &str| ToolDef {
            name: name.to_string(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
            output_example: None,
            read_only: None,
            file_input: Default::default(),
        };
        let mut defs = vec![
            def("example__tool__file_info"),
            def("example__plan__describe_file"),
            def("example__agent__reviewer"),
            def("t__search"),
        ];
        set.retain_shown(&Origin::Outside, &mut defs);
        let names: Vec<&str> = defs.iter().map(|def| def.name.as_str()).collect();
        assert_eq!(names, ["example__plan__describe_file", "t__search"]);
    }

    fn plan(yaml: &str) -> Plan {
        str::parse::<PlanDoc>(yaml).unwrap().steps
    }

    const AGENT_PATTERNS: &str = r#"
identifier: patterns
name: Patterns
description: agent tool patterns
steps:
  - id: E0
    tool_name: agent
    input:
      prompt: look
      tools: ["example__tool__*", "example__plan__*", "example__tool__file_info"]
"#;

    #[test]
    fn agent_tool_patterns_are_judged_by_what_they_reach() {
        let set = example();
        let problems = set.plan_problems(&Origin::Outside, &plan(AGENT_PATTERNS));
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("'example__tool__*'"), "{problems:?}");
        assert!(problems[1].contains("'example__tool__file_info'"));
        assert!(set
            .plan_problems(&Origin::Pack("example".into()), &plan(AGENT_PATTERNS))
            .is_empty());
    }

    #[test]
    fn an_empty_pack_set_has_nothing_to_refuse() {
        let set = PackSet::default();
        assert!(set
            .plan_problems(&Origin::Outside, &plan(AGENT_PATTERNS))
            .is_empty());
    }
}
