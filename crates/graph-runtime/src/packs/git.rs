use crate::packs::refs::Target;
use gix::bstr::{BStr, BString, ByteSlice};
use gix::objs::tree::EntryKind;
use graph_config::pack::DEFAULT_REPO;
use std::collections::BTreeMap;
use std::fmt::Display;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::AtomicBool;

#[cfg(any(test, feature = "fixtures"))]
pub mod fixture;

pub fn is_bare_name(text: &str) -> bool {
    text.starts_with(|c: char| c.is_ascii_lowercase())
        && text
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

pub fn source_of(typed: &str, path: Option<&str>) -> (String, String) {
    if is_bare_name(typed) {
        (DEFAULT_REPO.to_string(), path.unwrap_or(typed).to_string())
    } else {
        (typed.to_string(), path.unwrap_or_default().to_string())
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

impl RemoteRefs {
    fn record(&mut self, name: &BStr, peeled: String) {
        let Ok(name) = name.to_str() else {
            return;
        };
        if let Some(tag) = name.strip_prefix("refs/tags/") {
            self.tags.insert(tag.to_string(), peeled);
        } else if let Some(head) = name.strip_prefix("refs/heads/") {
            self.heads.insert(head.to_string(), peeled);
        }
    }
}

fn failed(what: impl Display, error: impl Display) -> String {
    format!("{what}: {error}")
}

fn local_path(url: &str) -> Option<PathBuf> {
    if let Some(path) = url.strip_prefix("file://") {
        return Some(PathBuf::from(path));
    }
    if url.contains("://") || is_scp_like(url) {
        return None;
    }
    Some(PathBuf::from(url))
}

fn is_scp_like(url: &str) -> bool {
    match url.split_once(':') {
        Some((host, _)) => !host.is_empty() && !host.contains('/'),
        None => false,
    }
}

fn open_local(path: &Path) -> Result<gix::Repository, String> {
    gix::open(path).map_err(|e| {
        failed(
            format!("opening the git repository at {}", path.display()),
            e,
        )
    })
}

pub fn ls_remote(url: &str) -> Result<RemoteRefs, String> {
    match local_path(url) {
        Some(path) => local_refs(&open_local(&path)?),
        None => remote_refs(url),
    }
}

fn local_refs(repo: &gix::Repository) -> Result<RemoteRefs, String> {
    let what = "listing refs";
    let mut refs = RemoteRefs::default();
    let platform = repo.references().map_err(|e| failed(what, e))?;
    for reference in platform.all().map_err(|e| failed(what, e))? {
        let mut reference = reference.map_err(|e| failed(what, e))?;
        let name = reference.name().as_bstr().to_owned();
        if !name.starts_with(b"refs/tags/") && !name.starts_with(b"refs/heads/") {
            continue;
        }
        let Ok(id) = reference.peel_to_id() else {
            continue;
        };
        refs.record(name.as_bstr(), id.to_string());
    }
    if let Ok(Some(head)) = repo.head_name() {
        if let Some(branch) = head.as_bstr().strip_prefix(b"refs/heads/") {
            let branch = branch.to_str_lossy().into_owned();
            if refs.heads.contains_key(&branch) {
                refs.default_branch = Some(branch);
            }
        }
    }
    Ok(refs)
}

fn scratch_repo() -> Result<(tempfile::TempDir, gix::Repository), String> {
    let dir = tempfile::tempdir().map_err(|e| failed("creating a scratch repository", e))?;
    let repo = gix::ThreadSafeRepository::init_opts(
        dir.path(),
        gix::create::Kind::Bare,
        Default::default(),
        network_options(),
    )
    .map_err(|e| failed("creating a scratch repository", e))?
    .to_thread_local();
    Ok((dir, repo))
}

fn network_options() -> gix::open::Options {
    gix::open::Options::default().config_overrides(["gitoxide.credentials.terminalPrompt=false"])
}

fn remote_refs(url: &str) -> Result<RemoteRefs, String> {
    use gix::protocol::handshake::Ref;
    let what = format!("listing refs at {url}");
    let (_dir, repo) = scratch_repo()?;
    let remote = repo
        .remote_at(url)
        .map_err(|e| failed(&what, e))?
        .with_fetch_tags(gix::remote::fetch::Tags::None);
    let connection = remote
        .connect(gix::remote::Direction::Fetch)
        .map_err(|e| failed(&what, e))?;
    let (ref_map, _) = connection
        .ref_map(
            gix::progress::Discard,
            gix::remote::ref_map::Options {
                prefix_from_spec_as_filter_on_remote: false,
                ..Default::default()
            },
        )
        .map_err(|e| failed(&what, e))?;
    let mut refs = RemoteRefs::default();
    for remote_ref in &ref_map.remote_refs {
        match remote_ref {
            Ref::Peeled {
                full_ref_name,
                object,
                ..
            }
            | Ref::Direct {
                full_ref_name,
                object,
            } => refs.record(full_ref_name.as_bstr(), object.to_string()),
            Ref::Symbolic {
                full_ref_name,
                target,
                object,
                ..
            } => {
                if full_ref_name == "HEAD" {
                    refs.default_branch = target
                        .strip_prefix(b"refs/heads/")
                        .map(|branch| branch.to_str_lossy().into_owned());
                } else {
                    refs.record(full_ref_name.as_bstr(), object.to_string());
                }
            }
            Ref::Unborn { .. } => {}
        }
    }
    Ok(refs)
}

fn full_ref(target: &Target) -> String {
    match &target.tag {
        Some(tag) => format!("refs/tags/{tag}"),
        None => format!("refs/heads/{}", target.reference),
    }
}

pub fn clone_at(url: &str, target: &Target, dest: &Path) -> Result<String, String> {
    let reference = full_ref(target);
    match local_path(url) {
        Some(path) => {
            let repo = open_local(&path)?;
            let commit = peeled_commit(&repo, &reference)
                .map_err(|e| failed(format!("{url} at {}", target.reference), e))?;
            write_commit(&repo, commit, dest)
        }
        None => fetch_remote(url, &reference, dest)
            .map_err(|e| failed(format!("fetching {url} at {}", target.reference), e)),
    }
}

fn fetch_remote(url: &str, reference: &str, dest: &Path) -> Result<String, String> {
    let scratch = tempfile::tempdir().map_err(|e| e.to_string())?;
    let mut prepare = gix::clone::PrepareFetch::new(
        url,
        scratch.path(),
        gix::create::Kind::Bare,
        Default::default(),
        network_options(),
    )
    .map_err(|e| e.to_string())?
    .with_shallow(gix::remote::fetch::Shallow::DepthAtRemote(
        std::num::NonZeroU32::MIN,
    ))
    .with_revision(Some(reference))
    .map_err(|e| e.to_string())?;
    let (repo, _) = prepare
        .fetch_only(gix::progress::Discard, &AtomicBool::new(false))
        .map_err(|e| e.to_string())?;
    let commit = repo.head_id().map_err(|e| e.to_string())?.detach();
    write_commit(&repo, commit, dest)
}

fn peeled_commit(repo: &gix::Repository, reference: &str) -> Result<gix::ObjectId, String> {
    let mut found = repo
        .find_reference(reference)
        .map_err(|e| failed(format!("no ref {reference}"), e))?;
    let id = found.peel_to_id().map_err(|e| failed(reference, e))?;
    let commit = id
        .object()
        .map_err(|e| failed(reference, e))?
        .peel_to_commit()
        .map_err(|e| failed(reference, e))?;
    Ok(commit.id)
}

fn write_commit(
    repo: &gix::Repository,
    commit: gix::ObjectId,
    dest: &Path,
) -> Result<String, String> {
    let tree = repo
        .find_commit(commit)
        .map_err(|e| failed(format!("reading commit {commit}"), e))?
        .tree()
        .map_err(|e| failed(format!("reading the tree of {commit}"), e))?;
    write_tree(&tree, dest)?;
    Ok(commit.to_string())
}

fn safe_name(name: &BStr) -> Result<&str, String> {
    let text = name
        .to_str()
        .map_err(|_| format!("refusing a non-UTF-8 path {name:?}"))?;
    let refused = text.is_empty()
        || text == "."
        || text == ".."
        || text.eq_ignore_ascii_case(".git")
        || text.contains(['/', '\\', '\0']);
    if refused {
        return Err(format!("refusing to write the tree entry {text:?}"));
    }
    Ok(text)
}

fn write_tree(tree: &gix::Tree<'_>, dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| failed(format!("creating {}", dest.display()), e))?;
    for entry in tree.iter() {
        let entry = entry.map_err(|e| failed("reading a tree", e))?;
        let path = dest.join(safe_name(entry.filename())?);
        let object = || entry.object().map_err(|e| failed("reading an object", e));
        match entry.mode().kind() {
            EntryKind::Tree => write_tree(&object()?.into_tree(), &path)?,
            EntryKind::Blob | EntryKind::BlobExecutable => {
                std::fs::write(&path, &object()?.data)
                    .map_err(|e| failed(format!("writing {}", path.display()), e))?;
                if entry.mode().kind() == EntryKind::BlobExecutable {
                    make_executable(&path)?;
                }
            }
            EntryKind::Link => {
                let target = object()?.data.clone();
                symlink(&target, &path)?;
            }
            EntryKind::Commit => std::fs::create_dir_all(&path)
                .map_err(|e| failed(format!("creating {}", path.display()), e))?,
        }
    }
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)
        .map_err(|e| failed(format!("reading {}", path.display()), e))?
        .permissions();
    permissions.set_mode(permissions.mode() | ((permissions.mode() & 0o444) >> 2));
    std::fs::set_permissions(path, permissions)
        .map_err(|e| failed(format!("marking {} executable", path.display()), e))
}

#[cfg(not(unix))]
fn make_executable(_: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn symlink(target: &[u8], path: &Path) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(target), path)
        .map_err(|e| failed(format!("linking {}", path.display()), e))
}

#[cfg(not(unix))]
fn symlink(target: &[u8], path: &Path) -> Result<(), String> {
    std::fs::write(path, target).map_err(|e| failed(format!("writing {}", path.display()), e))
}

fn discover(dir: &Path) -> Result<gix::Repository, String> {
    gix::discover(dir)
        .map_err(|e| failed(format!("{} is not in a git repository", dir.display()), e))
}

fn workdir(repo: &gix::Repository) -> Result<PathBuf, String> {
    let dir = repo
        .workdir()
        .ok_or_else(|| "the repository has no working tree".to_string())?;
    dir.canonicalize()
        .map_err(|e| failed(format!("resolving {}", dir.display()), e))
}

pub fn toplevel(dir: &Path) -> Result<PathBuf, String> {
    workdir(&discover(dir)?)
}

fn relative_to(top: &Path, dir: &Path) -> Result<String, String> {
    let dir = dir
        .canonicalize()
        .map_err(|e| failed(format!("resolving {}", dir.display()), e))?;
    let inside = dir
        .strip_prefix(top)
        .map_err(|_| format!("{} is outside {}", dir.display(), top.display()))?;
    Ok(inside
        .components()
        .filter_map(|part| match part {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/"))
}

struct Line {
    index: char,
    worktree: char,
    from: Option<BString>,
}

pub fn uncommitted(dir: &Path) -> Result<Vec<String>, String> {
    use gix::status::index_worktree::Item as Worktree;
    use gix::status::plumbing::index_as_worktree::{Change, EntryStatus};
    let what = "reading git status";
    let repo = discover(dir)?;
    let top = workdir(&repo)?;
    let inside = relative_to(&top, dir)?;
    let patterns: Vec<BString> = if inside.is_empty() {
        Vec::new()
    } else {
        vec![format!(":(top){inside}").into()]
    };
    let status = repo
        .status(gix::progress::Discard)
        .map_err(|e| failed(what, e))?
        .untracked_files(gix::status::UntrackedFiles::Collapsed)
        .into_iter(patterns)
        .map_err(|e| failed(what, e))?;
    let mut tracked: BTreeMap<BString, Line> = BTreeMap::new();
    let mut untracked: Vec<BString> = Vec::new();
    for item in status {
        match item.map_err(|e| failed(what, e))? {
            gix::status::Item::TreeIndex(change) => {
                use gix::diff::index::ChangeRef;
                let (location, code, from) = match change {
                    ChangeRef::Addition { location, .. } => (location.into_owned(), 'A', None),
                    ChangeRef::Deletion { location, .. } => (location.into_owned(), 'D', None),
                    ChangeRef::Modification {
                        location,
                        previous_entry_mode,
                        entry_mode,
                        ..
                    } => {
                        let code = if previous_entry_mode
                            .to_tree_entry_mode()
                            .map(|mode| mode.kind())
                            != entry_mode.to_tree_entry_mode().map(|mode| mode.kind())
                        {
                            'T'
                        } else {
                            'M'
                        };
                        (location.into_owned(), code, None)
                    }
                    ChangeRef::Rewrite {
                        source_location,
                        location,
                        copy,
                        ..
                    } => (
                        location.into_owned(),
                        if copy { 'C' } else { 'R' },
                        Some(source_location.into_owned()),
                    ),
                };
                let line = tracked.entry(location).or_default();
                line.index = code;
                line.from = from;
            }
            gix::status::Item::IndexWorktree(Worktree::Modification {
                rela_path, status, ..
            }) => {
                let code = match status {
                    EntryStatus::Conflict { .. } => 'U',
                    EntryStatus::Change(Change::Removed) => 'D',
                    EntryStatus::Change(Change::Type { .. }) => 'T',
                    EntryStatus::Change(_) => 'M',
                    EntryStatus::IntentToAdd => 'A',
                    EntryStatus::NeedsUpdate(_) => continue,
                };
                let line = tracked.entry(rela_path).or_default();
                line.worktree = code;
                if code == 'U' {
                    line.index = 'U';
                }
            }
            gix::status::Item::IndexWorktree(Worktree::DirectoryContents { entry, .. }) => {
                if entry.status != gix::dir::entry::Status::Untracked {
                    continue;
                }
                let mut path = entry.rela_path;
                if entry.disk_kind == Some(gix::dir::entry::Kind::Directory) {
                    path.push(b'/');
                }
                untracked.push(path);
            }
            gix::status::Item::IndexWorktree(Worktree::Rewrite { .. }) => {}
        }
    }
    untracked.sort();
    let code = |c: char| if c == '\0' { ' ' } else { c };
    let mut lines: Vec<String> = tracked
        .into_iter()
        .map(|(path, line)| {
            let path = quoted(path.as_bstr());
            let shown = match &line.from {
                Some(from) => format!("{} -> {path}", quoted(from.as_bstr())),
                None => path,
            };
            format!("{}{} {shown}", code(line.index), code(line.worktree))
        })
        .collect();
    lines.extend(
        untracked
            .iter()
            .map(|path| format!("?? {}", quoted(path.as_bstr()))),
    );
    Ok(lines)
}

impl Default for Line {
    fn default() -> Self {
        Line {
            index: '\0',
            worktree: '\0',
            from: None,
        }
    }
}

fn quoted(path: &BStr) -> String {
    let needs = path
        .iter()
        .any(|&byte| !(0x21..0x7f).contains(&byte) || byte == b'"' || byte == b'\\');
    if !needs {
        return path.to_str_lossy().into_owned();
    }
    let mut out = String::from("\"");
    for &byte in path.iter() {
        match byte {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            0x07 => out.push_str("\\a"),
            0x08 => out.push_str("\\b"),
            0x0b => out.push_str("\\v"),
            0x0c => out.push_str("\\f"),
            0x20..=0x7e => out.push(byte as char),
            _ => out.push_str(&format!("\\{byte:03o}")),
        }
    }
    out.push('"');
    out
}

pub fn local_tags(dir: &Path) -> Result<Vec<String>, String> {
    let what = "listing tags";
    let repo = discover(dir)?;
    let platform = repo.references().map_err(|e| failed(what, e))?;
    let mut tags: Vec<String> = Vec::new();
    for reference in platform.tags().map_err(|e| failed(what, e))? {
        let reference = reference.map_err(|e| failed(what, e))?;
        if let Some(tag) = reference.name().as_bstr().strip_prefix(b"refs/tags/") {
            tags.push(tag.to_str_lossy().into_owned());
        }
    }
    tags.sort();
    Ok(tags)
}

pub fn export(dir: &Path, reference: &str, path: &str, dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| format!("creating {}: {e}", dest.display()))?;
    let repo = discover(dir)?;
    let at = |e: &dyn Display| failed(format!("exporting {reference}:{path}"), e);
    let tree = repo
        .find_reference(reference)
        .map_err(|e| at(&e))?
        .peel_to_id()
        .map_err(|e| at(&e))?
        .object()
        .map_err(|e| at(&e))?
        .peel_to_tree()
        .map_err(|e| at(&e))?;
    if path.is_empty() {
        return write_tree(&tree, dest);
    }
    let entry = tree
        .lookup_entry_by_path(path)
        .map_err(|e| at(&e))?
        .ok_or_else(|| format!("{reference} has no {path}"))?;
    if entry.mode().kind() != EntryKind::Tree {
        return Err(format!("{reference}:{path} is not a directory"));
    }
    let subtree = entry.object().map_err(|e| at(&e))?.into_tree();
    write_tree(&subtree, dest)
}

pub fn annotated_tag(dir: &Path, tag: &str, message: &str) -> Result<String, String> {
    let what = format!("tagging {tag}");
    let repo = discover(dir)?;
    let head = repo.head_commit().map_err(|e| failed(&what, e))?.id;
    let tagger = repo
        .committer()
        .ok_or_else(|| {
            format!(
                "{what}: no committer identity; set user.name and user.email in your git config"
            )
        })?
        .map_err(|e| failed(&what, e))?;
    let message = if message.ends_with('\n') {
        message.to_string()
    } else {
        format!("{message}\n")
    };
    repo.tag(
        tag,
        head,
        gix::objs::Kind::Commit,
        Some(tagger),
        message,
        gix::refs::transaction::PreviousValue::MustNotExist,
    )
    .map_err(|e| failed(&what, e))?;
    Ok(head.to_string())
}

#[cfg(test)]
mod tests;
