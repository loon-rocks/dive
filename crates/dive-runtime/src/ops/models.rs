use crate::runtime::Workspace;
use crate::secrets::Secrets;
use anyhow::{anyhow, bail, Context, Result};
use dive_config::secrets::{InProject, Origin as SecretOrigin, Resolved, SecretSource};
use dive_config::{
    FallbackChoice, ModelChoice, ModelKind, ModelPrice, PriceTier, ProviderConfig, ProviderKind,
    CONFIG_FORMAT, FORMAT_KEY,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

pub const STANDARD_ROLES: [&str; 9] = [
    "default", "chat", "planner", "outliner", "solver", "repair", "judge", "decider", "image",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Workspace,
    Global,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyState {
    None,
    Set,
    Missing,
    InFile,
    Unreadable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderEntry {
    pub name: String,
    pub kind: ProviderKind,
    pub api_key: Option<String>,
    pub key_names: Vec<String>,
    pub key: KeyState,
    pub base_url: Option<String>,
    pub region: Option<String>,
    pub profile: Option<String>,
    pub origin: Origin,
    pub used_by: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fallback {
    pub provider: String,
    pub model: String,
    pub temperature: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoleEntry {
    pub role: String,
    pub standard: bool,
    pub provider: String,
    pub model: String,
    pub temperature: Option<f32>,
    pub fallbacks: Vec<Fallback>,
    pub kind: Option<ModelKind>,
    pub description: Option<String>,
    pub origin: Origin,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceEntry {
    pub model: String,
    pub input: f64,
    pub output: f64,
    #[serde(default)]
    pub output_image: Option<f64>,
    pub cache_write: Option<f64>,
    pub cache_read: Option<f64>,
    #[serde(default)]
    pub tiers: Vec<PriceTier>,
    #[serde(default)]
    pub source: Option<String>,
    pub origin: Origin,
    pub used_by: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelsView {
    pub file: PathBuf,
    pub global_workspace: bool,
    pub blocked: Option<String>,
    pub providers: Vec<ProviderEntry>,
    pub roles: Vec<RoleEntry>,
    pub pricing: Vec<PriceEntry>,
    pub unpriced: Vec<String>,
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderEdit {
    pub name: String,
    pub kind: ProviderKind,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub region: Option<String>,
    pub profile: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoleEdit {
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<ModelKind>,
    pub role: String,
    pub provider: String,
    pub model: String,
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub fallbacks: Vec<Fallback>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceEdit {
    pub model: String,
    pub input: f64,
    pub output: f64,
    #[serde(default)]
    pub output_image: Option<f64>,
    pub cache_write: Option<f64>,
    pub cache_read: Option<f64>,
    #[serde(default)]
    pub tiers: Option<Vec<PriceTier>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderProbe {
    pub ok: bool,
    pub listed: bool,
    pub models: Vec<String>,
    pub millis: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layers {
    pub global: PathBuf,
    pub file: PathBuf,
    pub global_workspace: bool,
    pub load_paths: Vec<PathBuf>,
}

impl Layers {
    pub fn of(workspace: &Workspace) -> Self {
        let global = dive_config::expand_tilde(&dive_config::global_config_path());
        let global_workspace = workspace.is_global();
        let file = if global_workspace {
            global.clone()
        } else {
            dive_config::legacy::project_dir(workspace.root()).join("config.toml")
        };
        let load_paths = if global_workspace {
            vec![global.clone()]
        } else {
            vec![global.clone(), file.clone()]
        };
        Self {
            global,
            file,
            global_workspace,
            load_paths,
        }
    }

    fn read_paths(&self) -> Vec<(PathBuf, Origin)> {
        if self.global_workspace {
            vec![(self.file.clone(), Origin::Workspace)]
        } else {
            vec![
                (self.global.clone(), Origin::Global),
                (self.file.clone(), Origin::Workspace),
            ]
        }
    }
}

pub fn view(workspace: &Workspace, secrets: &Secrets) -> ModelsView {
    let project = crate::secrets::project_id(workspace);
    let resolver = InProject::new(secrets.resolver(), project.clone());
    let mut view = view_layers(&Layers::of(workspace), &resolver);
    if secrets.store.status().is_ok() {
        for provider in &mut view.providers {
            let unreadable = provider.key == KeyState::Missing
                && provider.key_names.iter().any(|name| {
                    project
                        .as_deref()
                        .map(Some)
                        .into_iter()
                        .chain(std::iter::once(None))
                        .any(|layer| secrets.store.get_in(name, layer).is_err())
                });
            if unreadable {
                provider.key = KeyState::Unreadable;
            }
        }
    }
    view
}

struct Merged {
    providers: BTreeMap<String, (toml::Value, Origin)>,
    models: BTreeMap<String, (toml::Value, Origin)>,
    pricing: BTreeMap<String, (toml::Value, Origin)>,
    problems: Vec<String>,
}

fn merge_value(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(base), toml::Value::Table(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(existing) => merge_value(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn read_layers(layers: &Layers) -> Merged {
    let mut merged = Merged {
        providers: BTreeMap::new(),
        models: BTreeMap::new(),
        pricing: BTreeMap::new(),
        problems: Vec::new(),
    };
    for (path, origin) in layers.read_paths() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let table: toml::Table = match text.parse() {
            Ok(table) => table,
            Err(error) => {
                merged
                    .problems
                    .push(format!("parsing {}: {error}", path.display()));
                continue;
            }
        };
        for (section, target) in [
            ("providers", &mut merged.providers),
            ("models", &mut merged.models),
            ("pricing", &mut merged.pricing),
        ] {
            let Some(toml::Value::Table(entries)) = table.get(section) else {
                continue;
            };
            for (name, entry) in entries {
                match target.get_mut(name) {
                    Some((existing, existing_origin)) => {
                        merge_value(existing, entry.clone());
                        *existing_origin = origin;
                    }
                    None => {
                        target.insert(name.clone(), (entry.clone(), origin));
                    }
                }
            }
        }
    }
    merged
}

fn key_state(api_key: Option<&str>, resolver: &dyn SecretSource) -> (Vec<String>, KeyState) {
    let Some(api_key) = api_key.filter(|key| !key.is_empty()) else {
        return (Vec::new(), KeyState::None);
    };
    let references = dive_core::secrets::env_references(api_key);
    if references.is_empty() {
        return (Vec::new(), KeyState::InFile);
    }
    let names: Vec<String> = references.iter().map(|r| r.name.to_string()).collect();
    let all_set = references.iter().all(|reference| {
        reference.default.is_some()
            || resolver
                .resolve(reference.name)
                .is_some_and(|found| !found.value.is_empty())
    });
    (
        names,
        if all_set {
            KeyState::Set
        } else {
            KeyState::Missing
        },
    )
}

pub fn view_layers(layers: &Layers, resolver: &dyn SecretSource) -> ModelsView {
    let merged = read_layers(layers);
    let mut problems = merged.problems;
    let mut providers: Vec<ProviderEntry> = Vec::new();
    for (name, (raw, origin)) in merged.providers {
        match raw.try_into::<ProviderConfig>() {
            Ok(provider) => {
                let (key_names, key) = key_state(provider.api_key.as_deref(), resolver);
                providers.push(ProviderEntry {
                    name,
                    kind: provider.kind,
                    api_key: provider.api_key,
                    key_names,
                    key,
                    base_url: provider.base_url,
                    region: provider.region,
                    profile: provider.profile,
                    origin,
                    used_by: Vec::new(),
                });
            }
            Err(error) => problems.push(format!("[providers.{name}]: {error}")),
        }
    }
    let mut roles: Vec<RoleEntry> = Vec::new();
    for (role, (raw, origin)) in merged.models {
        match raw.try_into::<ModelChoice>() {
            Ok(choice) => roles.push(RoleEntry {
                standard: STANDARD_ROLES.contains(&role.as_str()),
                kind: Some(
                    dive_config::Role::from_name(&role)
                        .map(dive_config::Role::kind)
                        .unwrap_or(choice.kind.unwrap_or_default()),
                ),
                provider: choice.provider,
                model: choice.model,
                temperature: choice.temperature,
                fallbacks: choice
                    .fallbacks
                    .into_iter()
                    .map(|fallback| Fallback {
                        provider: fallback.provider,
                        model: fallback.model,
                        temperature: fallback.temperature,
                    })
                    .collect(),
                description: choice.description,
                origin,
                role,
            }),
            Err(error) => problems.push(format!("[models.{role}]: {error}")),
        }
    }
    roles.sort_by_key(|role| {
        (
            STANDARD_ROLES
                .iter()
                .position(|name| *name == role.role)
                .unwrap_or(STANDARD_ROLES.len()),
            role.role.clone(),
        )
    });
    for provider in &mut providers {
        for role in &roles {
            if role.provider == provider.name {
                provider.used_by.push(role.role.clone());
            }
            for fallback in &role.fallbacks {
                if fallback.provider == provider.name {
                    provider.used_by.push(format!("{} fallback", role.role));
                }
            }
        }
    }
    let mut pricing: Vec<PriceEntry> = Vec::new();
    for (model, (raw, origin)) in merged.pricing {
        match raw.try_into::<ModelPrice>() {
            Ok(price) => pricing.push(PriceEntry {
                used_by: roles
                    .iter()
                    .filter(|role| {
                        role.model == model || role.fallbacks.iter().any(|f| f.model == model)
                    })
                    .map(|role| role.role.clone())
                    .collect(),
                input: price.input,
                output: price.output,
                output_image: price.output_image,
                cache_write: price.cache_write,
                cache_read: price.cache_read,
                tiers: price.tiers,
                source: price.source,
                origin,
                model,
            }),
            Err(error) => problems.push(format!("[pricing.\"{model}\"]: {error}")),
        }
    }
    let mut unpriced: Vec<String> = roles
        .iter()
        .flat_map(|role| {
            std::iter::once(role.model.clone())
                .chain(role.fallbacks.iter().map(|f| f.model.clone()))
        })
        .filter(|model| !pricing.iter().any(|price| &price.model == model))
        .collect();
    unpriced.sort();
    unpriced.dedup();
    ModelsView {
        file: layers.file.clone(),
        global_workspace: layers.global_workspace,
        blocked: blocked(&layers.file),
        providers,
        roles,
        pricing,
        unpriced,
        problems,
    }
}

fn blocked(file: &Path) -> Option<String> {
    if !file.exists() {
        return None;
    }
    match dive_config::inspect(file) {
        Err(error) => Some(error),
        Ok(layer) if layer.version > CONFIG_FORMAT => Some(format!(
            "{} is config version {}, newer than this app reads ({CONFIG_FORMAT}).",
            file.display(),
            layer.version
        )),
        Ok(_) => None,
    }
}

#[derive(Debug)]
struct Placeholder;

impl SecretSource for Placeholder {
    fn resolve(&self, _name: &str) -> Option<Resolved> {
        Some(Resolved {
            value: "placeholder".to_string(),
            source: SecretOrigin::Env,
        })
    }
}

fn check_name(what: &str, name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        bail!("{what} names use letters, digits, hyphens and underscores");
    }
    Ok(())
}

fn validate(layers: &Layers, candidate: &str) -> Result<()> {
    let dir = layers
        .file
        .parent()
        .context("the config file has no folder")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let temp = tempfile::Builder::new()
        .prefix(".config-check")
        .suffix(".toml")
        .tempfile_in(dir)
        .context("creating a temporary config to check")?;
    std::fs::write(temp.path(), candidate).context("writing the config to check")?;
    let paths: Vec<PathBuf> = layers
        .load_paths
        .iter()
        .map(|path| {
            if *path == layers.file {
                temp.path().to_path_buf()
            } else {
                path.clone()
            }
        })
        .collect();
    let loaded = dive_config::load_from(&paths, &Placeholder).map_err(|error| {
        anyhow!(format!("{error:#}").replace(
            &temp.path().display().to_string(),
            &layers.file.display().to_string()
        ))
    })?;
    let config = loaded.config;
    for (role, choice) in config.models.iter() {
        if !config.providers.contains_key(&choice.provider) {
            bail!(
                "[models.{role}] uses provider '{}', which isn't configured",
                choice.provider
            );
        }
    }
    dive_llm::ModelRouter::from_config(&config).map_err(|error| anyhow!(error.to_string()))?;
    Ok(())
}

fn edit(layers: &Layers, change: impl FnOnce(&mut DocumentMut) -> Result<()>) -> Result<()> {
    if let Some(reason) = blocked(&layers.file) {
        bail!(reason);
    }
    let text = match std::fs::read_to_string(&layers.file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            format!("{FORMAT_KEY} = {CONFIG_FORMAT}\n")
        }
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", layers.file.display()))
        }
    };
    let mut doc: DocumentMut = text
        .parse()
        .with_context(|| format!("parsing {}", layers.file.display()))?;
    change(&mut doc)?;
    let candidate = doc.to_string();
    validate(layers, &candidate)?;
    let temp = layers.file.with_extension("toml.tmp");
    std::fs::write(&temp, &candidate).with_context(|| format!("writing {}", temp.display()))?;
    std::fs::rename(&temp, &layers.file)
        .with_context(|| format!("writing {}", layers.file.display()))?;
    Ok(())
}

fn section<'a>(doc: &'a mut DocumentMut, name: &str) -> Result<&'a mut Table> {
    let item = doc.entry(name).or_insert_with(|| {
        let mut table = Table::new();
        table.set_implicit(true);
        Item::Table(table)
    });
    item.as_table_mut()
        .ok_or_else(|| anyhow!("[{name}] must be a table"))
}

fn take_entry(table: &mut Table, original: Option<&str>, name: &str) -> Result<Table> {
    if original != Some(name) && table.contains_key(name) {
        bail!("'{name}' already exists in this file");
    }
    let existing = original
        .and_then(|original| table.remove(original))
        .and_then(|item| match item {
            Item::Table(table) => Some(table),
            Item::Value(toml_edit::Value::InlineTable(inline)) => Some(inline.into_table()),
            _ => None,
        });
    Ok(existing.unwrap_or_default())
}

fn set_optional(table: &mut Table, key: &str, text: Option<&str>) {
    match text.map(str::trim).filter(|text| !text.is_empty()) {
        Some(text) => {
            table.insert(key, value(text));
        }
        None => {
            table.remove(key);
        }
    }
}

fn set_number(table: &mut Table, key: &str, number: Option<f64>) {
    match number {
        Some(number) => {
            table.insert(key, value(number));
        }
        None => {
            table.remove(key);
        }
    }
}

fn kind_name(kind: ProviderKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default()
}

pub fn save_provider(
    workspace: &Workspace,
    original: Option<&str>,
    edit: &ProviderEdit,
) -> Result<()> {
    save_provider_in(&Layers::of(workspace), original, edit)
}

pub fn save_provider_in(
    layers: &Layers,
    original: Option<&str>,
    change: &ProviderEdit,
) -> Result<()> {
    check_name("Provider", &change.name)?;
    edit(layers, |doc| {
        let providers = section(doc, "providers")?;
        let mut entry = take_entry(providers, original, &change.name)?;
        entry.insert("type", value(kind_name(change.kind)));
        set_optional(&mut entry, "api_key", change.api_key.as_deref());
        set_optional(&mut entry, "base_url", change.base_url.as_deref());
        set_optional(&mut entry, "region", change.region.as_deref());
        set_optional(&mut entry, "profile", change.profile.as_deref());
        providers.insert(&change.name, Item::Table(entry));
        if let Some(old) = original.filter(|old| *old != change.name) {
            rename_provider_uses(doc, old, &change.name);
        }
        Ok(())
    })
}

fn rename_provider_uses(doc: &mut DocumentMut, old: &str, new: &str) {
    let Some(models) = doc.get_mut("models").and_then(Item::as_table_like_mut) else {
        return;
    };
    for (_, role) in models.iter_mut() {
        let Some(role) = role.as_table_like_mut() else {
            continue;
        };
        if role.get("provider").and_then(Item::as_str) == Some(old) {
            role.insert("provider", value(new));
        }
        if let Some(fallbacks) = role
            .get_mut("fallbacks")
            .and_then(Item::as_array_of_tables_mut)
        {
            for fallback in fallbacks.iter_mut() {
                if fallback.get("provider").and_then(Item::as_str) == Some(old) {
                    fallback.insert("provider", value(new));
                }
            }
        }
    }
}

pub fn remove_provider(workspace: &Workspace, name: &str) -> Result<()> {
    remove_provider_in(&Layers::of(workspace), name)
}

pub fn remove_provider_in(layers: &Layers, name: &str) -> Result<()> {
    let current = view_layers(layers, &Placeholder);
    let shadowed = !layers.global_workspace && global_has(layers, "providers", name);
    if !shadowed {
        if let Some(provider) = current.providers.iter().find(|p| p.name == name) {
            if !provider.used_by.is_empty() {
                bail!(
                    "{name} is used by {}. Point them at another provider first.",
                    provider.used_by.join(", ")
                );
            }
        }
    }
    edit(layers, |doc| remove_from(doc, "providers", name))
}

fn global_has(layers: &Layers, section: &str, name: &str) -> bool {
    std::fs::read_to_string(&layers.global)
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .and_then(|table| {
            table
                .get(section)
                .and_then(|s| s.as_table())
                .map(|s| s.contains_key(name))
        })
        .unwrap_or(false)
}

fn remove_from(doc: &mut DocumentMut, section: &str, name: &str) -> Result<()> {
    let removed = doc
        .get_mut(section)
        .and_then(Item::as_table_like_mut)
        .and_then(|table| table.remove(name));
    if removed.is_none() {
        bail!("'{name}' isn't set in this file");
    }
    Ok(())
}

pub fn save_role(workspace: &Workspace, original: Option<&str>, edit: &RoleEdit) -> Result<()> {
    save_role_in(&Layers::of(workspace), original, edit)
}

pub fn save_role_in(layers: &Layers, original: Option<&str>, change: &RoleEdit) -> Result<()> {
    check_name("Role", &change.role)?;
    if change.model.trim().is_empty() {
        bail!("choose a model for {}", change.role);
    }
    edit(layers, |doc| {
        let models = section(doc, "models")?;
        let mut entry = take_entry(models, original, &change.role)?;
        let old_fallbacks: Vec<FallbackChoice> = entry
            .get("fallbacks")
            .map(|item| item.to_string())
            .and_then(|text| toml::from_str::<toml::Table>(&format!("f = {text}")).ok())
            .and_then(|table| table.get("f").cloned())
            .and_then(|value| value.try_into().ok())
            .unwrap_or_default();
        let custom = !STANDARD_ROLES.contains(&change.role.as_str());
        if let Some(original) = original.filter(|_| entry.contains_key("provider")) {
            let before = dive_config::Role::from_name(original)
                .map(dive_config::Role::kind)
                .or_else(|| {
                    entry.get("type").and_then(Item::as_str).and_then(|text| {
                        [ModelKind::Chat, ModelKind::Decision, ModelKind::Image]
                            .into_iter()
                            .find(|kind| kind.as_str() == text)
                    })
                })
                .unwrap_or_default();
            let after = dive_config::Role::from_name(&change.role)
                .map(dive_config::Role::kind)
                .or(change.kind)
                .unwrap_or_default();
            if before != after {
                bail!(
                    "{original} is {} role, and a saved role's type can't change: the agents, plans and tools that use it expect {} model. Add a new role for {} model instead.",
                    before.with_article(),
                    before.with_article(),
                    after.with_article()
                );
            }
        }
        match change.kind {
            Some(kind) if custom && kind != ModelKind::Chat => {
                entry.insert("type", value(kind.as_str()));
            }
            _ => {
                entry.remove("type");
            }
        }
        entry.insert("provider", value(change.provider.trim()));
        entry.insert("model", value(change.model.trim()));
        set_number(&mut entry, "temperature", change.temperature.map(f64::from));
        match change
            .description
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        {
            Some(text) => {
                entry.insert("description", value(text));
            }
            None => {
                entry.remove("description");
            }
        }
        if change.fallbacks.is_empty() {
            entry.remove("fallbacks");
        } else {
            let mut array = ArrayOfTables::new();
            for fallback in &change.fallbacks {
                let mut table = Table::new();
                table.insert("provider", value(fallback.provider.trim()));
                table.insert("model", value(fallback.model.trim()));
                set_number(
                    &mut table,
                    "temperature",
                    fallback.temperature.map(f64::from),
                );
                let window = old_fallbacks
                    .iter()
                    .find(|old| old.provider == fallback.provider && old.model == fallback.model)
                    .and_then(|old| old.context_window);
                if let Some(window) = window {
                    table.insert("context_window", value(i64::from(window)));
                }
                array.push(table);
            }
            entry.insert("fallbacks", Item::ArrayOfTables(array));
        }
        models.insert(&change.role, Item::Table(entry));
        Ok(())
    })
}

pub fn remove_role(workspace: &Workspace, role: &str) -> Result<()> {
    remove_role_in(&Layers::of(workspace), role)
}

pub fn remove_role_in(layers: &Layers, role: &str) -> Result<()> {
    edit(layers, |doc| remove_from(doc, "models", role))
}

pub fn save_price(workspace: &Workspace, original: Option<&str>, edit: &PriceEdit) -> Result<()> {
    save_price_in(&Layers::of(workspace), original, edit)
}

pub fn save_price_in(layers: &Layers, original: Option<&str>, change: &PriceEdit) -> Result<()> {
    let model = change.model.trim();
    if model.is_empty() {
        bail!("name the model id this price is for");
    }
    for (what, number) in [
        ("input", Some(change.input)),
        ("output", Some(change.output)),
        ("output image", change.output_image),
        ("cache write", change.cache_write),
        ("cache read", change.cache_read),
    ] {
        if number.is_some_and(|n| !n.is_finite() || n < 0.0) {
            bail!("the {what} price must be zero or more");
        }
    }
    if let Some(tiers) = &change.tiers {
        check_tiers(tiers)?;
    }
    edit(layers, |doc| {
        let pricing = section(doc, "pricing")?;
        let mut entry = take_entry(pricing, original, model)?;
        entry.insert("input", value(change.input));
        entry.insert("output", value(change.output));
        set_number(&mut entry, "output_image", change.output_image);
        set_number(&mut entry, "cache_write", change.cache_write);
        set_number(&mut entry, "cache_read", change.cache_read);
        if let Some(tiers) = &change.tiers {
            set_tiers(&mut entry, tiers);
        }
        entry.remove("source");
        pricing.insert(model, Item::Table(entry));
        Ok(())
    })
}

fn check_tiers(tiers: &[PriceTier]) -> Result<()> {
    for (index, tier) in tiers.iter().enumerate() {
        if tiers[..index].iter().any(|other| other.above == tier.above) {
            bail!("two tiers start above {} prompt tokens", tier.above);
        }
        for (what, number) in [
            ("input", tier.input),
            ("output", tier.output),
            ("cache write", tier.cache_write),
            ("cache read", tier.cache_read),
        ] {
            if number.is_some_and(|n| !n.is_finite() || n < 0.0) {
                bail!(
                    "the {what} price above {} prompt tokens must be zero or more",
                    tier.above
                );
            }
        }
    }
    Ok(())
}

fn set_tiers(entry: &mut Table, tiers: &[PriceTier]) {
    if tiers.is_empty() {
        entry.remove("tiers");
        return;
    }
    let mut sorted = tiers.to_vec();
    sorted.sort_by_key(|tier| tier.above);
    let mut array = ArrayOfTables::new();
    for tier in sorted {
        let mut table = Table::new();
        table.insert("above", value(tier.above as i64));
        set_number(&mut table, "input", tier.input);
        set_number(&mut table, "output", tier.output);
        set_number(&mut table, "cache_write", tier.cache_write);
        set_number(&mut table, "cache_read", tier.cache_read);
        array.push(table);
    }
    entry.insert("tiers", Item::ArrayOfTables(array));
}

pub fn remove_price(workspace: &Workspace, model: &str) -> Result<()> {
    remove_price_in(&Layers::of(workspace), model)
}

pub fn remove_price_in(layers: &Layers, model: &str) -> Result<()> {
    edit(layers, |doc| remove_from(doc, "pricing", model))
}

pub fn default_key_name(kind: ProviderKind) -> Option<&'static str> {
    match kind {
        ProviderKind::Anthropic => Some("ANTHROPIC_API_KEY"),
        ProviderKind::Openai => Some("OPENAI_API_KEY"),
        ProviderKind::Systemone => Some("SYSTEMONE_API_KEY"),
        ProviderKind::Gemini => Some("GEMINI_API_KEY"),
        ProviderKind::OpenaiCompat | ProviderKind::Bedrock => None,
    }
}

fn expand_key(
    api_key: Option<&str>,
    resolver: &dyn SecretSource,
) -> Result<Option<String>, String> {
    let Some(api_key) = api_key.filter(|key| !key.is_empty()) else {
        return Ok(None);
    };
    let mut expanded = api_key.to_string();
    for reference in dive_core::secrets::env_references(api_key) {
        let found = resolver
            .resolve(reference.name)
            .map(|found| found.value)
            .filter(|value| !value.is_empty())
            .or(reference.default.map(str::to_string));
        let Some(found) = found else {
            return Err(format!("{} isn't set", reference.name));
        };
        let literal = match reference.default {
            Some(default) => format!("${{{}:-{default}}}", reference.name),
            None => format!("${{{}}}", reference.name),
        };
        expanded = expanded.replace(&literal, &found);
    }
    Ok(Some(expanded))
}

pub async fn probe(
    workspace: &Workspace,
    secrets: &Secrets,
    provider: &ProviderEdit,
) -> ProviderProbe {
    let resolver = InProject::new(secrets.resolver(), crate::secrets::project_id(workspace));
    probe_with(provider, &resolver).await
}

pub async fn probe_with(provider: &ProviderEdit, resolver: &dyn SecretSource) -> ProviderProbe {
    let started = Instant::now();
    let failed = |error: String| ProviderProbe {
        ok: false,
        listed: false,
        models: Vec::new(),
        millis: started.elapsed().as_millis() as u64,
        error: Some(error),
    };
    if !dive_llm::catalog::lists_models(provider.kind) {
        return failed(format!(
            "{} can't be tested without making a request that costs credit",
            kind_name(provider.kind)
        ));
    }
    let key = match expand_key(provider.api_key.as_deref(), resolver) {
        Ok(key) => key,
        Err(error) => return failed(error),
    };
    match dive_llm::catalog::list_models(
        provider.kind,
        provider.base_url.as_deref(),
        key.as_deref(),
    )
    .await
    {
        Ok(models) => ProviderProbe {
            ok: true,
            listed: true,
            models,
            millis: started.elapsed().as_millis() as u64,
            error: None,
        },
        Err(error) => failed(describe(error)),
    }
}

fn describe(error: dive_llm::LlmError) -> String {
    match error {
        dive_llm::LlmError::Api { status: 401, .. }
        | dive_llm::LlmError::Api { status: 403, .. } => {
            "the provider refused the API key".to_string()
        }
        dive_llm::LlmError::Api { status, .. } => format!("the provider answered {status}"),
        dive_llm::LlmError::Http(error) if error.is_connect() || error.is_timeout() => {
            "couldn't reach the provider".to_string()
        }
        other => other.to_string(),
    }
}

mod catalog;
pub use catalog::{fill_prices, fill_prices_in, Catalog, PriceFill, CATALOG_SOURCE, CATALOG_URL};

#[cfg(test)]
mod tests;
