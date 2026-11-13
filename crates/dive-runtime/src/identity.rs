use dive_config::IdentityConfig;
use dive_core::run::Identity;

pub const LOCAL_TENANT: &str = "local";
pub const LOCAL_ENVIRONMENT: &str = "local";
const UNKNOWN_USER: &str = "unknown";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    #[error("serve mode needs [identity].{0} in the project config")]
    Missing(&'static str),
}

fn present(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

pub fn local(
    configured: &IdentityConfig,
    deployment: &str,
    user: Option<String>,
    hostname: impl FnOnce() -> Option<String>,
) -> Identity {
    let user = present(configured.user.as_deref())
        .or_else(|| present(user.as_deref()))
        .or_else(|| present(hostname().as_deref()))
        .unwrap_or_else(|| UNKNOWN_USER.to_string());
    Identity {
        user,
        tenant: present(configured.tenant.as_deref()).unwrap_or_else(|| LOCAL_TENANT.to_string()),
        deployment: present(configured.deployment.as_deref())
            .unwrap_or_else(|| deployment.to_string()),
        environment: present(configured.environment.as_deref())
            .unwrap_or_else(|| LOCAL_ENVIRONMENT.to_string()),
    }
}

pub fn serve(
    configured: &IdentityConfig,
    user: &str,
    tenant: &str,
) -> Result<Identity, IdentityError> {
    Ok(Identity {
        user: user.to_string(),
        tenant: tenant.to_string(),
        deployment: present(configured.deployment.as_deref())
            .ok_or(IdentityError::Missing("deployment"))?,
        environment: present(configured.environment.as_deref())
            .ok_or(IdentityError::Missing("environment"))?,
    })
}

#[derive(Clone)]
pub struct Acting {
    pub name: String,
    pub identity: dive_config::DevIdentity,
    pub minter: std::sync::Arc<crate::dev_key::DevMinter>,
}

#[derive(Clone)]
pub struct RunOrigin {
    identity: Identity,
    acting: Option<Acting>,
}

impl RunOrigin {
    pub fn plain(identity: Identity) -> Self {
        Self {
            identity,
            acting: None,
        }
    }

    pub fn acting(identity: Identity, acting: Acting) -> Self {
        Self {
            identity: Identity {
                user: acting.identity.user.clone(),
                tenant: acting.identity.tenant.clone(),
                ..identity
            },
            acting: Some(acting),
        }
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn acting_as(&self) -> Option<&str> {
        self.acting.as_ref().map(|acting| acting.name.as_str())
    }

    pub fn open(
        &self,
        events: std::sync::Arc<dyn dive_core::EventSink>,
        plan: Option<&str>,
    ) -> dive_core::run::Run {
        let run = dive_core::run::Run::new(self.identity.clone(), events);
        let Some(acting) = &self.acting else {
            return run;
        };
        match acting.minter.mint(&acting.identity, run.id.as_str(), plan) {
            Ok(token) => run.with_token(token),
            Err(error) => {
                tracing::warn!(error = %error, identity = %acting.name, "couldn't mint a run token for the dev identity");
                run
            }
        }
    }
}

pub fn hostname() -> Option<String> {
    let from_command = std::process::Command::new("hostname")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string());
    from_command.filter(|name| !name.is_empty()).or_else(|| {
        std::fs::read_to_string("/etc/hostname")
            .ok()
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> IdentityConfig {
        IdentityConfig::default()
    }

    #[test]
    fn a_local_run_with_no_identity_section_takes_the_user_from_user() {
        let identity = local(&none(), "cli", Some("ada".into()), || Some("box".into()));
        assert_eq!(
            identity,
            Identity {
                user: "ada".into(),
                tenant: "local".into(),
                deployment: "cli".into(),
                environment: "local".into(),
            }
        );
    }

    #[test]
    fn a_local_run_with_no_identity_section_falls_back_to_the_hostname_when_user_is_unset() {
        assert_eq!(
            local(&none(), "desktop", None, || Some("box".into())).user,
            "box"
        );
        assert_eq!(
            local(&none(), "cli", Some(" ".into()), || Some("box".into())).user,
            "box"
        );
    }

    #[test]
    fn a_missing_user_and_hostname_never_fail_the_run() {
        assert_eq!(local(&none(), "cli", None, || None).user, "unknown");
    }

    #[test]
    fn the_identity_section_wins_over_every_fallback() {
        let configured = IdentityConfig {
            user: Some("grace".into()),
            tenant: Some("acme".into()),
            deployment: Some("ci".into()),
            environment: Some("staging".into()),
        };
        let identity = local(&configured, "cli", Some("ada".into()), || {
            panic!("the hostname is never needed when the user is configured")
        });
        assert_eq!(
            identity,
            Identity {
                user: "grace".into(),
                tenant: "acme".into(),
                deployment: "ci".into(),
                environment: "staging".into(),
            }
        );
    }

    #[test]
    fn a_serve_identity_takes_user_and_tenant_from_the_caller_and_needs_deployment_and_environment()
    {
        let configured = IdentityConfig {
            user: Some("ignored".into()),
            tenant: Some("ignored".into()),
            deployment: Some("prod-us".into()),
            environment: Some("production".into()),
        };
        assert_eq!(
            serve(&configured, "sub-1", "tenant-9"),
            Ok(Identity {
                user: "sub-1".into(),
                tenant: "tenant-9".into(),
                deployment: "prod-us".into(),
                environment: "production".into(),
            })
        );
        assert_eq!(
            serve(&none(), "sub-1", "tenant-9"),
            Err(IdentityError::Missing("deployment"))
        );
    }

    #[test]
    fn this_machine_has_a_hostname() {
        assert!(hostname().is_some_and(|name| !name.is_empty()));
    }
}
