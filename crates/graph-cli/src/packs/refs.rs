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
        match canonical_version(text) {
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

fn canonical_version(text: &str) -> Option<Version> {
    let version = Version::parse(text.strip_prefix('v')?).ok()?;
    let canonical =
        version.pre.is_empty() && version.build.is_empty() && text == format!("v{version}");
    canonical.then_some(version)
}

pub fn tag_prefix(path: &str) -> String {
    if path.is_empty() {
        String::new()
    } else {
        format!("{path}/")
    }
}

pub fn release_tag(path: &str, version: &Version) -> String {
    format!("{}v{version}", tag_prefix(path))
}

pub fn release_version(path: &str, tag: &str) -> Option<Version> {
    canonical_version(tag.strip_prefix(&tag_prefix(path))?)
}

pub fn label(path: &str, tag: Option<&str>) -> Option<String> {
    let tag = tag?;
    Some(
        tag.strip_prefix(&tag_prefix(path))
            .unwrap_or(tag)
            .to_string(),
    )
}

pub fn pack_at(url: &str, path: &str) -> String {
    if path.is_empty() {
        url.to_string()
    } else {
        format!("{path}/ in {url}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub tag: String,
    pub sha: String,
}

pub fn releases(refs: &RemoteRefs, path: &str) -> Vec<Release> {
    let mut releases: Vec<Release> = refs
        .tags
        .iter()
        .filter_map(|(tag, sha)| {
            release_version(path, tag).map(|version| Release {
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

impl Release {
    pub fn label(&self) -> String {
        format!("v{}", self.version)
    }

    pub fn target(&self) -> Target {
        Target {
            reference: self.tag.clone(),
            tag: Some(self.tag.clone()),
            sha: self.sha.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub reference: String,
    pub tag: Option<String>,
    pub sha: String,
}

pub fn resolve(
    refs: &RemoteRefs,
    wanted: &PackRef,
    url: &str,
    path: &str,
) -> Result<Target, String> {
    let releases = releases(refs, path);
    let at = pack_at(url, path);
    match wanted {
        PackRef::Major(major) => newest_in(&releases, *major)
            .map(Release::target)
            .ok_or_else(|| {
                format!(
                    "{at} has no v{major}.*.* release tag{}",
                    available(&releases)
                )
            }),
        PackRef::Tag(version) => releases
            .iter()
            .find(|candidate| candidate.version == *version)
            .map(Release::target)
            .ok_or_else(|| format!("{at} has no tag v{version}{}", available(&releases))),
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
                    "{at} has no branch or release tag named '{branch}'{}",
                    available(&releases)
                )
            }),
    }
}

fn available(releases: &[Release]) -> String {
    if releases.is_empty() {
        return String::new();
    }
    let labels: Vec<String> = releases.iter().map(Release::label).collect();
    format!(" (release tags: {})", labels.join(", "))
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
        let all = releases(
            &refs(&[
                "v1.0.0",
                "v1.10.0",
                "v1.2.0",
                "v2.0.0-rc.1",
                "1.5.0",
                "latest",
            ]),
            "",
        );
        let tags: Vec<&str> = all.iter().map(|r| r.tag.as_str()).collect();
        assert_eq!(tags, ["v1.0.0", "v1.2.0", "v1.10.0"]);
    }

    #[test]
    fn a_pack_sees_only_the_tags_under_its_own_path() {
        let refs = refs(&[
            "a/v1.0.0",
            "a/v1.1.0",
            "b/v2.0.0",
            "v9.0.0",
            "a/b/v3.0.0",
            "ab/v4.0.0",
        ]);
        let tags = |path: &str| -> Vec<String> {
            releases(&refs, path)
                .into_iter()
                .map(|release| release.tag)
                .collect()
        };
        assert_eq!(tags("a"), ["a/v1.0.0", "a/v1.1.0"]);
        assert_eq!(tags("b"), ["b/v2.0.0"]);
        assert_eq!(tags(""), ["v9.0.0"]);
        assert_eq!(tags("a/b"), ["a/b/v3.0.0"]);
        assert_eq!(release_tag("a", &Version::new(1, 2, 0)), "a/v1.2.0");
        assert_eq!(release_tag("", &Version::new(1, 2, 0)), "v1.2.0");
        assert_eq!(label("a", Some("a/v1.1.0")).as_deref(), Some("v1.1.0"));
        assert_eq!(label("", Some("v1.1.0")).as_deref(), Some("v1.1.0"));
        assert_eq!(label("a", None), None);
    }

    #[test]
    fn resolution_follows_the_ref() {
        let refs = refs(&["v1.0.0", "v1.1.0", "v2.0.0"]);
        assert_eq!(
            resolve(&refs, &PackRef::Major(1), "u", "")
                .unwrap()
                .tag
                .as_deref(),
            Some("v1.1.0")
        );
        assert_eq!(
            resolve(&refs, &PackRef::parse("v1.0.0"), "u", "")
                .unwrap()
                .tag
                .as_deref(),
            Some("v1.0.0")
        );
        let branch = resolve(&refs, &PackRef::parse("main"), "u", "").unwrap();
        assert_eq!((branch.tag, branch.sha.as_str()), (None, "head"));
        let error = resolve(&refs, &PackRef::Major(3), "u", "").unwrap_err();
        assert!(error.contains("no v3.*.* release tag"), "{error}");
        assert!(error.contains("v2.0.0"), "{error}");
    }

    #[test]
    fn resolution_under_a_path_adds_the_prefix_and_names_the_directory() {
        let refs = refs(&["pk/v1.0.0", "pk/v1.1.0", "v2.0.0"]);
        let target = resolve(&refs, &PackRef::Major(1), "u", "pk").unwrap();
        assert_eq!(target.reference, "pk/v1.1.0");
        assert_eq!(target.tag.as_deref(), Some("pk/v1.1.0"));
        let error = resolve(&refs, &PackRef::Major(2), "u", "pk").unwrap_err();
        assert_eq!(
            error,
            "pk/ in u has no v2.*.* release tag (release tags: v1.0.0, v1.1.0)"
        );
    }
}
