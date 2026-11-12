use super::error::ServeError;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
struct Counts {
    total: usize,
    tenants: HashMap<String, usize>,
}

#[derive(Debug)]
pub struct Limits {
    max_runs: usize,
    max_runs_per_tenant: usize,
    counts: Mutex<Counts>,
}

impl Limits {
    pub fn new(max_runs: usize, max_runs_per_tenant: usize) -> Arc<Self> {
        Arc::new(Self {
            max_runs,
            max_runs_per_tenant,
            counts: Mutex::new(Counts::default()),
        })
    }

    fn counts(&self) -> std::sync::MutexGuard<'_, Counts> {
        self.counts
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    pub fn at_capacity(&self) -> bool {
        self.counts().total >= self.max_runs
    }

    pub fn running(&self) -> usize {
        self.counts().total
    }

    pub fn acquire(self: &Arc<Self>, tenant: &str) -> Result<Slot, ServeError> {
        let mut counts = self.counts();
        if counts.total >= self.max_runs {
            return Err(ServeError::AtCapacity(format!(
                "this server is running its limit of {} runs",
                self.max_runs
            )));
        }
        let running = counts.tenants.get(tenant).copied().unwrap_or(0);
        if running >= self.max_runs_per_tenant {
            return Err(ServeError::AtCapacity(format!(
                "tenant {tenant} is running its limit of {} runs",
                self.max_runs_per_tenant
            )));
        }
        counts.total += 1;
        counts.tenants.insert(tenant.to_string(), running + 1);
        Ok(Slot {
            limits: Arc::clone(self),
            tenant: tenant.to_string(),
        })
    }
}

#[derive(Debug)]
pub struct Slot {
    limits: Arc<Limits>,
    tenant: String,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut counts = self.limits.counts();
        counts.total = counts.total.saturating_sub(1);
        if let Some(running) = counts.tenants.get_mut(&self.tenant) {
            *running = running.saturating_sub(1);
            if *running == 0 {
                counts.tenants.remove(&self.tenant);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tenant_over_its_limit_is_refused_while_others_still_start() {
        let limits = Limits::new(3, 1);
        let first = limits.acquire("acme").unwrap();
        assert_eq!(limits.acquire("acme").unwrap_err().code(), "at_capacity");
        let _other = limits.acquire("globex").unwrap();
        drop(first);
        let _again = limits.acquire("acme").unwrap();
        assert_eq!(limits.running(), 2);
    }

    #[test]
    fn the_server_reports_capacity_when_every_run_slot_is_taken() {
        let limits = Limits::new(1, 1);
        let slot = limits.acquire("acme").unwrap();
        assert!(limits.at_capacity());
        assert_eq!(limits.acquire("globex").unwrap_err().code(), "at_capacity");
        drop(slot);
        assert!(!limits.at_capacity());
    }
}
