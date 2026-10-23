use super::{rejection, updates_outcome};
use crate::commands::outcome::Outcome;
use anyhow::Result;
use dive_config::Config;
use dive_core::EventSink;
use dive_runtime::ops::pack::major as engine;
use dive_runtime::packs::layer::Layer;
use std::sync::Arc;

pub use engine::Consent;

pub struct Majors<'a> {
    pub consent: &'a Consent,
    pub config: &'a Config,
    pub events: Arc<dyn EventSink>,
}

pub async fn update(
    layer: &Layer,
    names: &[String],
    force: bool,
    majors: &Majors<'_>,
) -> Result<Outcome> {
    let majors = engine::Majors {
        consent: majors.consent,
        config: majors.config,
        events: majors.events.clone(),
        options: crate::host::options(),
    };
    match engine::update(layer, names, force, &majors).await {
        Ok(updates) => updates_outcome(updates),
        Err(error) => rejection(error),
    }
}
