use super::{edit, section, set_number, set_tiers, view, Layers, ModelsView, Origin, PriceEntry};
use crate::runtime::Workspace;
use crate::secrets::Secrets;
use anyhow::{Context, Result};
use graph_config::{ModelPrice, PriceTier, ProviderKind};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use toml_edit::{value, Item, Table};

pub const CATALOG_URL: &str = "https://models.dev/api.json";

pub const CATALOG_SOURCE: &str = "models.dev";

const KNOWN_HOSTS: &[(&str, &str)] = &[
    ("api.anthropic.com", "anthropic"),
    ("api.openai.com", "openai"),
    ("api.x.ai", "xai"),
    ("api.groq.com", "groq"),
    ("api.together.xyz", "togetherai"),
    ("api.mistral.ai", "mistral"),
    ("generativelanguage.googleapis.com", "google"),
];

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Catalog(BTreeMap<String, CatalogProvider>);

#[derive(Debug, Clone, Default, Deserialize)]
struct CatalogProvider {
    #[serde(default)]
    api: Option<String>,
    #[serde(default)]
    models: BTreeMap<String, CatalogModel>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct CatalogModel {
    #[serde(default)]
    cost: Option<CatalogCost>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct CatalogCost {
    input: Option<f64>,
    output: Option<f64>,
    #[serde(default)]
    cache_read: Option<f64>,
    #[serde(default)]
    cache_write: Option<f64>,
    #[serde(default)]
    tiers: Vec<CatalogTier>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct CatalogTier {
    tier: CatalogThreshold,
    #[serde(default)]
    input: Option<f64>,
    #[serde(default)]
    output: Option<f64>,
    #[serde(default)]
    cache_read: Option<f64>,
    #[serde(default)]
    cache_write: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct CatalogThreshold {
    #[serde(rename = "type")]
    kind: String,
    size: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PriceFill {
    pub added: Vec<String>,
    pub updated: Vec<String>,
    pub unchanged: Vec<String>,
    pub kept: Vec<String>,
    pub not_found: Vec<String>,
}

impl Catalog {
    pub fn parse(json: &str) -> Result<Self> {
        serde_json::from_str(json).context("reading the models.dev catalogue")
    }

    pub async fn fetch() -> Result<Self> {
        let response = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?
            .get(CATALOG_URL)
            .send()
            .await
            .with_context(|| format!("fetching {CATALOG_URL}"))?
            .error_for_status()
            .with_context(|| format!("fetching {CATALOG_URL}"))?;
        let text = response
            .text()
            .await
            .with_context(|| format!("reading {CATALOG_URL}"))?;
        Self::parse(&text)
    }

    pub fn price(
        &self,
        kind: ProviderKind,
        base_url: Option<&str>,
        model: &str,
    ) -> Option<ModelPrice> {
        let provider = self.provider_for(kind, base_url)?;
        let cost = provider.models.get(model)?.cost.as_ref()?;
        Some(ModelPrice {
            input: cost.input?,
            output: cost.output?,
            cache_write: cost.cache_write,
            cache_read: cost.cache_read,
            tiers: cost
                .tiers
                .iter()
                .filter(|tier| tier.tier.kind == "context")
                .map(|tier| PriceTier {
                    above: tier.tier.size,
                    input: tier.input,
                    output: tier.output,
                    cache_write: tier.cache_write,
                    cache_read: tier.cache_read,
                })
                .collect(),
            source: Some(CATALOG_SOURCE.to_string()),
        })
    }

    fn provider_for(&self, kind: ProviderKind, base_url: Option<&str>) -> Option<&CatalogProvider> {
        if let Some(host) = base_url.and_then(host_of) {
            if let Some((_, id)) = KNOWN_HOSTS.iter().find(|(known, _)| *known == host) {
                return self.0.get(*id);
            }
            return self
                .0
                .values()
                .find(|provider| provider.api.as_deref().and_then(host_of) == Some(host));
        }
        let id = match kind {
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::Openai => "openai",
            ProviderKind::Bedrock => "amazon-bedrock",
            ProviderKind::OpenaiCompat | ProviderKind::Systemone => return None,
        };
        self.0.get(id)
    }
}

fn host_of(url: &str) -> Option<&str> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let host = rest.split(['/', ':', '?']).next()?;
    (!host.is_empty()).then_some(host)
}

pub async fn fill_prices(
    workspace: &Workspace,
    secrets: &Secrets,
    refresh: bool,
) -> Result<PriceFill> {
    let catalog = Catalog::fetch().await?;
    let current = view(workspace, secrets);
    fill_prices_in(&Layers::of(workspace), &current, &catalog, refresh)
}

pub fn fill_prices_in(
    layers: &Layers,
    current: &ModelsView,
    catalog: &Catalog,
    refresh: bool,
) -> Result<PriceFill> {
    let kinds: BTreeMap<&str, (ProviderKind, Option<&str>)> = current
        .providers
        .iter()
        .map(|provider| {
            (
                provider.name.as_str(),
                (provider.kind, provider.base_url.as_deref()),
            )
        })
        .collect();
    let mut wanted: BTreeMap<String, Option<(ProviderKind, Option<&str>)>> = BTreeMap::new();
    for role in &current.roles {
        let uses = std::iter::once((role.provider.as_str(), role.model.as_str())).chain(
            role.fallbacks
                .iter()
                .map(|fallback| (fallback.provider.as_str(), fallback.model.as_str())),
        );
        for (provider, model) in uses {
            let found = kinds.get(provider).copied();
            let slot = wanted.entry(model.to_string()).or_insert(found);
            if slot.is_none() {
                *slot = found;
            }
        }
    }
    let editing = if current.global_workspace {
        Origin::Global
    } else {
        Origin::Workspace
    };
    let mut fill = PriceFill::default();
    let mut writes: Vec<(String, ModelPrice)> = Vec::new();
    for (model, provider) in wanted {
        let fetched = provider.and_then(|(kind, base_url)| catalog.price(kind, base_url, &model));
        let Some(entry) = current.pricing.iter().find(|price| price.model == model) else {
            match fetched {
                Some(price) => {
                    fill.added.push(model.clone());
                    writes.push((model, price));
                }
                None => fill.not_found.push(model),
            }
            continue;
        };
        let ours = entry.source.as_deref() == Some(CATALOG_SOURCE) && entry.origin == editing;
        match fetched {
            Some(price) if refresh && ours && !same_price(entry, &price) => {
                fill.updated.push(model.clone());
                writes.push((model, price));
            }
            Some(_) if refresh && !ours => fill.kept.push(model),
            None if refresh => fill.not_found.push(model),
            _ => fill.unchanged.push(model),
        }
    }
    if !writes.is_empty() {
        edit(layers, |doc| {
            let pricing = section(doc, "pricing")?;
            for (model, price) in &writes {
                pricing.insert(model, Item::Table(price_table(price)));
            }
            Ok(())
        })?;
    }
    Ok(fill)
}

fn same_price(entry: &PriceEntry, price: &ModelPrice) -> bool {
    entry.input == price.input
        && entry.output == price.output
        && entry.cache_write == price.cache_write
        && entry.cache_read == price.cache_read
        && entry.tiers == price.tiers
}

fn price_table(price: &ModelPrice) -> Table {
    let mut table = Table::new();
    table.insert("input", value(price.input));
    table.insert("output", value(price.output));
    set_number(&mut table, "cache_write", price.cache_write);
    set_number(&mut table, "cache_read", price.cache_read);
    set_tiers(&mut table, &price.tiers);
    if let Some(source) = &price.source {
        table.insert("source", value(source.as_str()));
    }
    table
}
