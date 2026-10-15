use super::fixture::git;
use super::*;
use crate::packs::content_hash;
use tempfile::TempDir;

const REAL_GIT: &str = "git";

fn real_git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(REAL_GIT)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .args(args)
        .output()
        .ok()?;
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn write(dir: &Path, path: &str, content: &str) {
    let path = dir.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

#[cfg(unix)]
fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o111 != 0
}

struct Upstream {
    _dir: TempDir,
    work: PathBuf,
    bare: PathBuf,
}

fn upstream() -> Upstream {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    git(&work, &["init"]);
    write(&work, "pk/pack.toml", "version = 1\nname = \"pk\"\n");
    write(&work, "pk/tools/a.yaml", "a: 1\n");
    write(&work, "pk/bin/run", "#!/bin/sh\necho hi\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let run = work.join("pk/bin/run");
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("tools/a.yaml", work.join("pk/link.yaml")).unwrap();
    }
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-m", "one"]);
    git(&work, &["tag", "pk/v1.0.0"]);
    write(&work, "pk/tools/a.yaml", "a: 2\n");
    git(&work, &["commit", "-am", "two"]);
    git(&work, &["tag", "-a", "pk/v1.1.0", "-m", "pk v1.1.0"]);
    git(&work, &["branch", "next"]);
    git(&work, &["checkout", "next"]);
    write(&work, "pk/tools/b.yaml", "b: 1\n");
    git(&work, &["add", "pk/tools/b.yaml"]);
    git(&work, &["commit", "-m", "three"]);
    git(&work, &["checkout", "main"]);
    git(dir.path(), &["clone", "--bare", "work", "bare.git"]);
    let bare = dir.path().join("bare.git");
    Upstream {
        _dir: dir,
        work,
        bare,
    }
}

fn target(reference: &str, tag: bool, sha: &str) -> Target {
    Target {
        reference: reference.to_string(),
        tag: tag.then(|| reference.to_string()),
        sha: sha.to_string(),
    }
}

#[test]
fn sources_resolve_to_urls() {
    let official = (
        "github.com/tylerdavis/graph-packs".to_string(),
        "example".to_string(),
    );
    assert_eq!(source_of("example", None), official);
    assert_eq!(
        remote_url(&source_of("example", None).0),
        "https://github.com/tylerdavis/graph-packs"
    );
    assert_eq!(
        source_of("example", Some("tools/example")).1,
        "tools/example"
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
        assert_eq!(source_of(given, None), (given.to_string(), String::new()));
        assert_eq!(source_of(given, Some("a")).1, "a");
        assert_eq!(remote_url(given), given);
    }
}

#[test]
fn only_paths_and_file_urls_read_in_process() {
    assert_eq!(local_path("/tmp/r"), Some(PathBuf::from("/tmp/r")));
    assert_eq!(local_path("file:///tmp/r"), Some(PathBuf::from("/tmp/r")));
    assert_eq!(local_path("./r"), Some(PathBuf::from("./r")));
    assert_eq!(local_path("https://github.com/o/r"), None);
    assert_eq!(local_path("ssh://git@host/o/r.git"), None);
    assert_eq!(local_path("git@github.com:o/r.git"), None);
}

#[test]
fn a_bare_repo_lists_peeled_tags_branches_and_its_default_branch() {
    let upstream = upstream();
    let refs = ls_remote(&upstream.bare.display().to_string()).unwrap();
    let repo = fixture::open(&upstream.work);
    let commit = |spec: &str| fixture::rev_parse(&repo, spec).to_string();
    assert_eq!(refs.default_branch.as_deref(), Some("main"));
    assert_eq!(refs.tags["pk/v1.0.0"], commit("HEAD~1"));
    assert_eq!(refs.tags["pk/v1.1.0"], commit("pk/v1.1.0^{commit}"));
    assert_ne!(refs.tags["pk/v1.1.0"], commit("pk/v1.1.0"));
    assert_eq!(refs.heads["main"], commit("main"));
    assert_eq!(refs.heads["next"], commit("next"));
    assert_eq!(
        refs,
        ls_remote(&upstream.work.display().to_string()).unwrap()
    );
}

#[test]
fn clone_at_writes_the_tree_of_an_annotated_tag_a_lightweight_tag_and_a_branch() {
    let upstream = upstream();
    let url = upstream.bare.display().to_string();
    let refs = ls_remote(&url).unwrap();
    for (reference, tag, file, content) in [
        ("pk/v1.1.0", true, "pk/tools/a.yaml", "a: 2\n"),
        ("pk/v1.0.0", true, "pk/tools/a.yaml", "a: 1\n"),
        ("next", false, "pk/tools/b.yaml", "b: 1\n"),
    ] {
        let sha = if tag {
            &refs.tags[reference]
        } else {
            &refs.heads[reference]
        };
        let dest = tempfile::tempdir().unwrap();
        let cloned = clone_at(&url, &target(reference, tag, sha), dest.path()).unwrap();
        assert_eq!(&cloned, sha, "{reference}");
        assert_eq!(
            std::fs::read_to_string(dest.path().join(file)).unwrap(),
            content
        );
        assert!(!dest.path().join(".git").exists());
        #[cfg(unix)]
        {
            assert!(executable(&dest.path().join("pk/bin/run")));
            assert!(!executable(&dest.path().join("pk/pack.toml")));
            assert_eq!(
                std::fs::read_link(dest.path().join("pk/link.yaml")).unwrap(),
                PathBuf::from("tools/a.yaml")
            );
        }
    }
    let missing = clone_at(
        &url,
        &target("nope", false, "x"),
        tempfile::tempdir().unwrap().path(),
    );
    assert!(missing.is_err());
}

#[test]
fn author_reads_tags_status_exports_and_tags_in_process() {
    let upstream = upstream();
    let pack = upstream.work.join("pk");
    assert_eq!(local_tags(&pack).unwrap(), ["pk/v1.0.0", "pk/v1.1.0"]);
    assert_eq!(
        toplevel(&pack).unwrap(),
        upstream.work.canonicalize().unwrap()
    );
    assert!(uncommitted(&pack).unwrap().is_empty());
    write(&upstream.work, "pk/tools/a.yaml", "a: 3\n");
    write(&upstream.work, "pk/new/c.yaml", "c\n");
    write(&upstream.work, "elsewhere.txt", "x\n");
    assert_eq!(
        uncommitted(&pack).unwrap(),
        [" M pk/tools/a.yaml", "?? pk/new/"]
    );
    let tree = tempfile::tempdir().unwrap();
    export(&upstream.work, "pk/v1.0.0", "pk", tree.path()).unwrap();
    assert_eq!(
        std::fs::read_to_string(tree.path().join("tools/a.yaml")).unwrap(),
        "a: 1\n"
    );
    assert!(export(&upstream.work, "pk/v1.0.0", "absent", tree.path()).is_err());
    let head = fixture::rev_parse(&fixture::open(&upstream.work), "HEAD").to_string();
    assert_eq!(
        annotated_tag(&pack, "pk/v1.2.0", "pk pk/v1.2.0").unwrap(),
        head
    );
    assert_eq!(git(&upstream.work, &["cat-file", "-t", "pk/v1.2.0"]), "tag");
    assert!(annotated_tag(&pack, "pk/v1.2.0", "again").is_err());
}

#[test]
fn installs_match_what_git_clone_writes() {
    let upstream = upstream();
    let url = upstream.bare.display().to_string();
    let refs = ls_remote(&url).unwrap();
    let scratch = tempfile::tempdir().unwrap();
    for (reference, tag) in [("pk/v1.1.0", true), ("pk/v1.0.0", true), ("next", false)] {
        let ours = scratch
            .path()
            .join(format!("ours-{}", reference.replace('/', "-")));
        let theirs = scratch
            .path()
            .join(format!("theirs-{}", reference.replace('/', "-")));
        let sha = if tag {
            &refs.tags[reference]
        } else {
            &refs.heads[reference]
        };
        let cloned = clone_at(&url, &target(reference, tag, sha), &ours).unwrap();
        let file_url = format!("file://{url}");
        let Some(_) = real_git(
            scratch.path(),
            &[
                "-c",
                "advice.detachedHead=false",
                "clone",
                "--quiet",
                "--depth",
                "1",
                "--branch",
                reference,
                &file_url,
                &theirs.display().to_string(),
            ],
        ) else {
            return;
        };
        let head = real_git(&theirs, &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(cloned, head.trim(), "{reference}");
        assert_eq!(
            content_hash(&ours).unwrap(),
            content_hash(&theirs).unwrap(),
            "{reference}"
        );
    }
}

#[test]
fn the_network_transport_fetches_the_same_tree_as_the_in_process_read() {
    let upstream = upstream();
    if real_git(upstream.work.as_path(), &["--version"]).is_none() {
        return;
    }
    let file_url = format!("file://{}", upstream.bare.display());
    let local = ls_remote(&upstream.bare.display().to_string()).unwrap();
    assert_eq!(remote_refs(&file_url).unwrap(), local);
    let fetched = tempfile::tempdir().unwrap();
    let read = tempfile::tempdir().unwrap();
    let sha = fetch_remote(&file_url, "refs/tags/pk/v1.1.0", fetched.path()).unwrap();
    assert_eq!(sha, local.tags["pk/v1.1.0"]);
    clone_at(
        &upstream.bare.display().to_string(),
        &target("pk/v1.1.0", true, &sha),
        read.path(),
    )
    .unwrap();
    assert_eq!(
        content_hash(fetched.path()).unwrap(),
        content_hash(read.path()).unwrap()
    );
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

#[test]
fn refs_match_git_ls_remote() {
    let upstream = upstream();
    let url = upstream.bare.display().to_string();
    let Some(output) = real_git(upstream.work.as_path(), &["ls-remote", "--symref", &url]) else {
        return;
    };
    assert_eq!(ls_remote(&url).unwrap(), parse_ls_remote(&output));
}

#[test]
fn status_matches_git_status_porcelain() {
    let upstream = upstream();
    let work = &upstream.work;
    if real_git(work, &["--version"]).is_none() {
        return;
    }
    write(work, "pk/tools/a.yaml", "a: 9\n");
    std::fs::remove_file(work.join("pk/bin/run")).unwrap();
    write(work, "pk/fresh.yaml", "f\n");
    write(work, "pk/deep/er/x.yaml", "x\n");
    write(work, "pk/with space.yaml", "s\n");
    write(work, "pk/staged.yaml", "s\n");
    write(work, "outside.txt", "o\n");
    real_git(work, &["add", "pk/staged.yaml"]);
    for dir in [work.join("pk"), work.clone()] {
        let theirs = real_git(&dir, &["status", "--porcelain", "--", "."]).unwrap();
        let theirs: Vec<&str> = theirs.lines().collect();
        assert_eq!(uncommitted(&dir).unwrap(), theirs, "{}", dir.display());
    }
}

#[test]
fn exports_and_tags_match_git() {
    let upstream = upstream();
    let work = &upstream.work;
    let Some(listed) = real_git(work, &["tag", "--list"]) else {
        return;
    };
    assert_eq!(
        local_tags(work).unwrap(),
        listed.lines().collect::<Vec<_>>()
    );
    let top = real_git(&work.join("pk"), &["rev-parse", "--show-toplevel"]).unwrap();
    assert_eq!(
        toplevel(&work.join("pk")).unwrap(),
        PathBuf::from(top.trim())
    );
    let ours = tempfile::tempdir().unwrap();
    let theirs = tempfile::tempdir().unwrap();
    export(work, "pk/v1.1.0", "pk", ours.path()).unwrap();
    let archive = theirs.path().join("export.tar");
    real_git(
        work,
        &[
            "archive",
            "--format=tar",
            "-o",
            &archive.display().to_string(),
            "pk/v1.1.0:pk",
        ],
    );
    let mut archive = tar_entries(&archive);
    archive.sort();
    let mut exported = Vec::new();
    collect_files(ours.path(), Path::new(""), &mut exported);
    exported.sort();
    assert_eq!(exported, archive);
}

fn tar_entries(archive: &Path) -> Vec<(String, Vec<u8>)> {
    let bytes = std::fs::read(archive).unwrap();
    let mut entries = Vec::new();
    let mut offset = 0;
    let mut long_name: Option<String> = None;
    while offset + 512 <= bytes.len() {
        let header = &bytes[offset..offset + 512];
        if header.iter().all(|byte| *byte == 0) {
            break;
        }
        let field = |range: std::ops::Range<usize>| {
            String::from_utf8_lossy(&header[range])
                .trim_end_matches('\0')
                .trim()
                .to_string()
        };
        let size = usize::from_str_radix(&field(124..136), 8).unwrap_or(0);
        let kind = header[156];
        let prefix = field(345..500);
        let name = long_name.take().unwrap_or_else(|| {
            let name = field(0..100);
            if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            }
        });
        let body = bytes[offset + 512..offset + 512 + size].to_vec();
        match kind {
            b'0' | 0 => entries.push((name, body)),
            b'2' => entries.push((name, field(157..257).into_bytes())),
            b'L' => {
                long_name = Some(
                    String::from_utf8_lossy(&body)
                        .trim_end_matches('\0')
                        .to_string(),
                )
            }
            _ => {}
        }
        offset += 512 + size.div_ceil(512) * 512;
    }
    entries
}

fn collect_files(root: &Path, relative: &Path, files: &mut Vec<(String, Vec<u8>)>) {
    for entry in std::fs::read_dir(root.join(relative)).unwrap() {
        let entry = entry.unwrap();
        let path = relative.join(entry.file_name());
        let kind = entry.file_type().unwrap();
        let name = path.display().to_string();
        if kind.is_symlink() {
            let target = std::fs::read_link(root.join(&path)).unwrap();
            files.push((name, target.display().to_string().into_bytes()));
        } else if kind.is_dir() {
            collect_files(root, &path, files);
        } else {
            files.push((name, std::fs::read(root.join(&path)).unwrap()));
        }
    }
}
