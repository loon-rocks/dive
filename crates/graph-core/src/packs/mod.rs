use graph_config::pack::LayerKind;
use std::collections::BTreeMap;
use std::path::PathBuf;

mod load;
mod resolve;

#[cfg(test)]
mod load_tests;

pub use load::{check_installed_name, load_pack, LoadedPack};
pub use resolve::{resolve_agent, resolve_name, resolve_plan_steps, SELF_PREFIX};

pub const KIND_SEPARATOR: &str = "__";
pub const PACK_ROOT: &str = "pack";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ItemKind {
    Tool,
    Plan,
    Agent,
}

impl ItemKind {
    pub const ALL: [ItemKind; 3] = [ItemKind::Tool, ItemKind::Plan, ItemKind::Agent];

    pub fn as_str(self) -> &'static str {
        match self {
            ItemKind::Tool => "tool",
            ItemKind::Plan => "plan",
            ItemKind::Agent => "agent",
        }
    }

    pub fn parse(text: &str) -> Option<ItemKind> {
        ItemKind::ALL.into_iter().find(|kind| kind.as_str() == text)
    }
}

pub fn catalog_name(pack: &str, kind: ItemKind, name: &str) -> String {
    format!("{pack}__{}__{name}", kind.as_str())
}

pub fn interface_name(kind: ItemKind, name: &str) -> String {
    format!("{}__{name}", kind.as_str())
}

pub fn split_catalog_name(full: &str) -> Option<(&str, ItemKind, &str)> {
    let (pack, rest) = full.split_once(KIND_SEPARATOR)?;
    let (kind, name) = rest.split_once(KIND_SEPARATOR)?;
    let kind = ItemKind::parse(kind)?;
    if pack.is_empty() || name.is_empty() {
        return None;
    }
    Some((pack, kind, name))
}

pub fn is_plan_name(name: &str) -> bool {
    matches!(split_catalog_name(name), Some((_, ItemKind::Plan, _)))
}

pub fn layer_label(layer: LayerKind) -> &'static str {
    match layer {
        LayerKind::Global => "global",
        LayerKind::Project => "project",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackSource {
    pub name: String,
    pub tag: Option<String>,
    pub layer: LayerKind,
    pub dir: PathBuf,
}

impl PackSource {
    pub fn describe(&self) -> String {
        format!(
            "pack {} {} ({})",
            self.name,
            self.tag.as_deref().unwrap_or("untagged"),
            layer_label(self.layer)
        )
    }

    pub fn template_root(&self) -> serde_json::Value {
        serde_json::json!({ "name": self.name })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackItem {
    pub pack: String,
    pub kind: ItemKind,
    pub name: String,
    pub public: bool,
}

impl PackItem {
    pub fn catalog_name(&self) -> String {
        catalog_name(&self.pack, self.kind, &self.name)
    }

    pub fn interface_name(&self) -> String {
        interface_name(self.kind, &self.name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Origin {
    #[default]
    Outside,
    Pack(String),
}

pub fn visible(caller: &Origin, target: &PackItem) -> bool {
    match caller {
        Origin::Pack(pack) if *pack == target.pack => true,
        _ => target.public,
    }
}

pub fn apply_interface(items: &mut [PackItem], include: &[String], exclude: &[String]) {
    let _ = (items, include, exclude);
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackSet {
    pub installed: Vec<String>,
    pub items: BTreeMap<String, PackItem>,
}

impl PackSet {
    pub fn is_empty(&self) -> bool {
        self.installed.is_empty()
    }

    pub fn is_installed(&self, pack: &str) -> bool {
        self.installed.iter().any(|name| name == pack)
    }

    pub fn item(&self, full: &str) -> Option<&PackItem> {
        self.items.get(full)
    }

    pub fn routed<'a>(&self, full: &'a str) -> Option<(&'a str, ItemKind, &'a str)> {
        split_catalog_name(full).filter(|(pack, _, _)| self.is_installed(pack))
    }

    pub fn install(&mut self, pack: &str) {
        if !self.is_installed(pack) {
            self.installed.push(pack.to_string());
        }
    }

    pub fn insert(&mut self, item: PackItem) {
        self.install(&item.pack);
        self.items.insert(item.catalog_name(), item);
    }

    pub fn matching<'a>(&'a self, pattern: &'a str) -> impl Iterator<Item = &'a PackItem> + 'a {
        self.items
            .iter()
            .filter(move |(name, _)| crate::pipeline::catalog::glob_matches(pattern, name))
            .map(|(_, item)| item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(pack: &str, public: bool) -> PackItem {
        PackItem {
            pack: pack.to_string(),
            kind: ItemKind::Tool,
            name: "file_info".to_string(),
            public,
        }
    }

    #[test]
    fn catalog_names_round_trip() {
        let name = catalog_name("example", ItemKind::Plan, "describe_file");
        assert_eq!(name, "example__plan__describe_file");
        assert_eq!(
            split_catalog_name(&name),
            Some(("example", ItemKind::Plan, "describe_file"))
        );
        assert_eq!(split_catalog_name("linear__list_issues"), None);
        assert_eq!(split_catalog_name("example__widget__x"), None);
    }

    #[test]
    fn only_the_owning_pack_sees_private_items() {
        assert!(visible(
            &Origin::Pack("example".into()),
            &item("example", false)
        ));
        assert!(!visible(&Origin::Outside, &item("example", false)));
        assert!(!visible(
            &Origin::Pack("other".into()),
            &item("example", false)
        ));
        assert!(visible(&Origin::Outside, &item("example", true)));
    }

    #[test]
    fn routing_needs_an_installed_pack() {
        let mut set = PackSet::default();
        set.insert(item("example", true));
        assert!(set.routed("example__tool__file_info").is_some());
        assert!(set.routed("other__tool__file_info").is_none());
    }
}
