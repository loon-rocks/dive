use super::git::RemoteRefs;
use semver::Version;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackRef {
    Major(u64),
    Tag(Version),
    Branch(String),
}

impl PackRef {
    pub fn parse(text: &str) -> PackRef {
        if let Some(major) = text
            .strip_prefix('v')
            .filter(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
            .and_then(|digits| digits.parse().ok())
        {
            return PackRef::Major(major);
        }
        match release_version(text) {
            Some(version) => PackRef::Tag(version),
            None => PackRef::Branch(text.to_string()),
        }
    }

    pub fn major_line(&self) -> Option<u64> {
        match self {
            PackRef::Major(major) => Some(*major),
            PackRef::Tag(version) => Some(version.major),
            PackRef::Branch(_) => None,
        }
    }
}

pub fn release_version(tag: &str) -> Option<Version> {
    let version = Version::parse(tag.strip_prefix('v')?).ok()?;
    let canonical =
        version.pre.is_empty() && version.build.is_empty() && tag == format!("v{version}");
    canonical.then_some(version)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub tag: String,
    pub sha: String,
}

pub fn releases(refs: &RemoteRefs) -> Vec<Release> {
    let mut releases: Vec<Release> = refs
        .tags
        .iter()
        .filter_map(|(tag, sha)| {
            release_version(tag).map(|version| Release {
                version,
                tag: tag.clone(),
                sha: sha.clone(),
            })
        })
        .collect();
    releases.sort_by(|a, b| a.version.cmp(&b.version));
    releases
}

pub fn newest(releases: &[Release]) -> Option<&Release> {
    releases.last()
}

pub fn newest_in(releases: &[Release], major: u64) -> Option<&Release> {
    releases
        .iter()
        .rev()
        .find(|release| release.version.major == major)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub reference: String,
    pub tag: Option<String>,
    pub sha: String,
}

pub fn resolve(refs: &RemoteRefs, wanted: &PackRef, url: &str) -> Result<Target, String> {
    let releases = releases(refs);
    let release = |release: &Release| Target {
        reference: release.tag.clone(),
        tag: Some(release.tag.clone()),
        sha: release.sha.clone(),
    };
    match wanted {
        PackRef::Major(major) => newest_in(&releases, *major).map(release).ok_or_else(|| {
            format!(
                "{url} has no v{major}.*.* release tag{}",
                available(&releases)
            )
        }),
        PackRef::Tag(version) => releases
            .iter()
            .find(|candidate| candidate.version == *version)
            .map(release)
            .ok_or_else(|| format!("{url} has no tag v{version}{}", available(&releases))),
        PackRef::Branch(branch) => refs
            .heads
            .get(branch)
            .map(|sha| Target {
                reference: branch.clone(),
                tag: None,
                sha: sha.clone(),
            })
            .ok_or_else(|| {
                format!(
                    "{url} has no branch or release tag named '{branch}'{}",
                    available(&releases)
                )
            }),
    }
}

fn available(releases: &[Release]) -> String {
    if releases.is_empty() {
        return String::new();
    }
    let tags: Vec<&str> = releases.iter().map(|r| r.tag.as_str()).collect();
    format!(" (release tags: {})", tags.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(tags: &[&str]) -> RemoteRefs {
        RemoteRefs {
            tags: tags
                .iter()
                .enumerate()
                .map(|(index, tag)| (tag.to_string(), format!("sha{index}")))
                .collect(),
            heads: [("main".to_string(), "head".to_string())].into(),
            default_branch: Some("main".to_string()),
        }
    }

    #[test]
    fn refs_parse_into_major_lines_pinned_tags_and_branches() {
        assert_eq!(PackRef::parse("v2"), PackRef::Major(2));
        assert_eq!(
            PackRef::parse("v2.1.0"),
            PackRef::Tag(Version::new(2, 1, 0))
        );
        assert_eq!(PackRef::parse("main"), PackRef::Branch("main".into()));
        assert_eq!(
            PackRef::parse("v2.1.0-rc.1"),
            PackRef::Branch("v2.1.0-rc.1".into())
        );
    }

    #[test]
    fn pre_releases_and_non_semver_tags_are_not_releases() {
        let all = releases(&refs(&[
            "v1.0.0",
            "v1.10.0",
            "v1.2.0",
            "v2.0.0-rc.1",
            "1.5.0",
            "latest",
        ]));
        let tags: Vec<&str> = all.iter().map(|r| r.tag.as_str()).collect();
        assert_eq!(tags, ["v1.0.0", "v1.2.0", "v1.10.0"]);
    }

    #[test]
    fn resolution_follows_the_ref() {
        let refs = refs(&["v1.0.0", "v1.1.0", "v2.0.0"]);
        assert_eq!(
            resolve(&refs, &PackRef::Major(1), "u")
                .unwrap()
                .tag
                .as_deref(),
            Some("v1.1.0")
        );
        assert_eq!(
            resolve(&refs, &PackRef::parse("v1.0.0"), "u")
                .unwrap()
                .tag
                .as_deref(),
            Some("v1.0.0")
        );
        let branch = resolve(&refs, &PackRef::parse("main"), "u").unwrap();
        assert_eq!((branch.tag, branch.sha.as_str()), (None, "head"));
        let error = resolve(&refs, &PackRef::Major(3), "u").unwrap_err();
        assert!(error.contains("no v3.*.* release tag"), "{error}");
        assert!(error.contains("v2.0.0"), "{error}");
    }
}
