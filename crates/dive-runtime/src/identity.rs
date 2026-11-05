use dive_core::run::Identity;

pub const LOCAL_TENANT: &str = "local";
pub const LOCAL_ENVIRONMENT: &str = "local";
const UNKNOWN_USER: &str = "unknown";

pub fn local(
    deployment: &str,
    user: Option<String>,
    hostname: impl FnOnce() -> Option<String>,
) -> Identity {
    let present = |value: Option<String>| value.filter(|value| !value.trim().is_empty());
    let user = present(user)
        .or_else(|| present(hostname()))
        .unwrap_or_else(|| UNKNOWN_USER.to_string());
    Identity {
        user,
        tenant: LOCAL_TENANT.to_string(),
        deployment: deployment.to_string(),
        environment: LOCAL_ENVIRONMENT.to_string(),
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

    #[test]
    fn a_local_identity_takes_the_user_from_user() {
        let identity = local("cli", Some("ada".into()), || Some("box".into()));
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
    fn a_local_identity_falls_back_to_the_hostname_when_user_is_unset() {
        assert_eq!(local("desktop", None, || Some("box".into())).user, "box");
        assert_eq!(
            local("cli", Some(" ".into()), || Some("box".into())).user,
            "box"
        );
    }

    #[test]
    fn a_missing_user_and_hostname_never_fail_the_run() {
        assert_eq!(local("cli", None, || None).user, "unknown");
    }

    #[test]
    fn this_machine_has_a_hostname() {
        assert!(hostname().is_some_and(|name| !name.is_empty()));
    }
}
