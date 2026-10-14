use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};

pub const DEFAULT_HOST: &str = "github.com/tylerdavis/graph-pack-";

pub fn is_bare_name(text: &str) -> bool {
    text.starts_with(|c: char| c.is_ascii_lowercase())
        && text
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

pub fn source_of(typed: &str) -> String {
    if is_bare_name(typed) {
        format!("{DEFAULT_HOST}{typed}")
    } else {
        typed.to_string()
    }
}

pub fn remote_url(source: &str) -> String {
    let explicit = source.contains("://")
        || source.starts_with("git@")
        || source.starts_with('/')
        || source.starts_with('.')
        || source.starts_with('~');
    if explicit {
        return graph_config::expand_tilde(Path::new(source))
            .display()
            .to_string();
    }
    let host = source.split('/').next().unwrap_or_default();
    if host.contains('.') && source.contains('/') && !Path::new(source).exists() {
        return format!("https://{source}");
    }
    source.to_string()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteRefs {
    pub tags: BTreeMap<String, String>,
    pub heads: BTreeMap<String, String>,
    pub default_branch: Option<String>,
}

pub fn ls_remote(url: &str) -> Result<RemoteRefs, String> {
    let output = git(None, &["ls-remote", "--symref", url])?;
    Ok(parse_ls_remote(&output))
}

fn parse_ls_remote(output: &str) -> RemoteRefs {
    let mut refs = RemoteRefs::default();
    let mut peeled = BTreeMap::new();
    for line in output.lines() {
        let Some((left, name)) = line.split_once('\t') else {
            continue;
        };
        if let Some(target) = left.strip_prefix("ref: ") {
            if name == "HEAD" {
                refs.default_branch = target.strip_prefix("refs/heads/").map(str::to_string);
            }
            continue;
        }
        if let Some(tag) = name.strip_prefix("refs/tags/") {
            match tag.strip_suffix("^{}") {
                Some(tag) => {
                    peeled.insert(tag.to_string(), left.to_string());
                }
                None => {
                    refs.tags.insert(tag.to_string(), left.to_string());
                }
            }
        } else if let Some(head) = name.strip_prefix("refs/heads/") {
            refs.heads.insert(head.to_string(), left.to_string());
        }
    }
    refs.tags.extend(peeled);
    refs
}

pub fn clone_at(url: &str, reference: &str, dest: &Path) -> Result<String, String> {
    let dest_text = dest.display().to_string();
    git(
        None,
        &[
            "-c",
            "advice.detachedHead=false",
            "clone",
            "--quiet",
            "--depth",
            "1",
            "--branch",
            reference,
            url,
            &dest_text,
        ],
    )?;
    let sha = git(Some(dest), &["rev-parse", "HEAD"])?;
    Ok(sha.trim().to_string())
}

pub fn toplevel(dir: &Path) -> Result<std::path::PathBuf, String> {
    let output = git(Some(dir), &["rev-parse", "--show-toplevel"])?;
    Ok(std::path::PathBuf::from(output.trim()))
}

pub fn uncommitted(dir: &Path) -> Result<Vec<String>, String> {
    let output = git(Some(dir), &["status", "--porcelain"])?;
    Ok(output.lines().map(str::to_string).collect())
}

pub fn local_tags(dir: &Path) -> Result<Vec<String>, String> {
    let output = git(Some(dir), &["tag", "--list"])?;
    Ok(output.lines().map(str::to_string).collect())
}

pub fn export(dir: &Path, reference: &str, dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| format!("creating {}: {e}", dest.display()))?;
    let archive = dest.join(".export.tar");
    let archive_text = archive.display().to_string();
    git(
        Some(dir),
        &["archive", "--format=tar", "-o", &archive_text, reference],
    )?;
    let output = Command::new("tar")
        .args(["-xf", &archive_text, "-C"])
        .arg(dest)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("running tar: {e}"))?;
    let _ = std::fs::remove_file(&archive);
    if !output.status.success() {
        return Err(format!(
            "tar -xf {archive_text}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

pub fn annotated_tag(dir: &Path, tag: &str, message: &str) -> Result<String, String> {
    git(Some(dir), &["tag", "-a", tag, "-m", message])?;
    let sha = git(Some(dir), &["rev-parse", &format!("{tag}^{{commit}}")])?;
    Ok(sha.trim().to_string())
}

fn git(dir: Option<&Path>, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new("git");
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    let output = command
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("running git: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git {}: {}", args.join(" "), stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_resolve_to_urls() {
        assert_eq!(
            source_of("example"),
            "github.com/tylerdavis/graph-pack-example"
        );
        assert_eq!(
            remote_url(&source_of("example")),
            "https://github.com/tylerdavis/graph-pack-example"
        );
        assert_eq!(remote_url("github.com/o/r"), "https://github.com/o/r");
        for given in [
            "https://github.com/o/r",
            "ssh://git@host/o/r.git",
            "git@github.com:o/r.git",
            "file:///tmp/r",
            "/tmp/r",
            "./r",
        ] {
            assert_eq!(source_of(given), given);
            assert_eq!(remote_url(given), given);
        }
    }

    #[test]
    fn ls_remote_output_prefers_peeled_tags_and_reads_the_default_branch() {
        let refs = parse_ls_remote(
            "ref: refs/heads/main\tHEAD\naaa\tHEAD\naaa\trefs/heads/main\nbbb\trefs/heads/next\nccc\trefs/tags/v1.0.0\nddd\trefs/tags/v1.0.0^{}\neee\trefs/tags/v2.0.0\n",
        );
        assert_eq!(refs.default_branch.as_deref(), Some("main"));
        assert_eq!(refs.heads["next"], "bbb");
        assert_eq!(refs.tags["v1.0.0"], "ddd");
        assert_eq!(refs.tags["v2.0.0"], "eee");
    }
}
