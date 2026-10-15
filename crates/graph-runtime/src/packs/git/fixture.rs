use gix::bstr::ByteSlice;
use gix::objs::tree::EntryKind;
use gix::refs::transaction::PreviousValue;
use gix::ObjectId;
use std::path::{Path, PathBuf};

const STAGED: &str = "fixture-staged";

pub fn git(dir: &Path, args: &[&str]) -> String {
    let args = without_config(args);
    let args: Vec<&str> = args.iter().copied().filter(|arg| *arg != "-q").collect();
    match args.as_slice() {
        ["init"] => init(dir),
        ["init", "--bare"] => init_bare(dir),
        ["clone", "--bare", from, to] => clone_bare(&dir.join(from), &dir.join(to)),
        ["add", "-A"] => stage(dir, &["-A"]),
        ["add", paths @ ..] => stage(dir, paths),
        ["commit", "-m", message] => commit(dir, false, message),
        ["commit", "-am", message] | ["commit", "-a", "-m", message] => commit(dir, true, message),
        ["tag", "--list"] => tags(dir),
        ["tag", "-a", name, "-m", message] => annotated(dir, name, message),
        ["tag", "-f", name] => lightweight(dir, name, "HEAD", PreviousValue::Any),
        ["tag", "-f", name, rev] => lightweight(dir, name, rev, PreviousValue::Any),
        ["tag", name] => lightweight(dir, name, "HEAD", PreviousValue::MustNotExist),
        ["tag", name, rev] => lightweight(dir, name, rev, PreviousValue::MustNotExist),
        ["branch", name] => branch(dir, name, "HEAD"),
        ["checkout", "-b", name] => {
            branch(dir, name, "HEAD");
            checkout(dir, name)
        }
        ["checkout", "-b", name, start] => {
            branch(dir, name, start);
            checkout(dir, name)
        }
        ["checkout", name] => checkout(dir, name),
        ["reset", "--hard", rev] => reset_hard(dir, rev),
        ["rev-parse", spec] => rev_parse(&open(dir), spec).to_string(),
        ["cat-file", "-t", spec] => object_kind(dir, spec),
        ["remote"] => remotes(dir),
        other => panic!("the gix fixture does not emulate `git {}`", other.join(" ")),
    }
}

fn without_config<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut kept = Vec::new();
    let mut skip = false;
    for arg in args {
        if skip {
            skip = false;
        } else if *arg == "-c" {
            skip = true;
        } else {
            kept.push(*arg);
        }
    }
    kept
}

fn options() -> gix::open::Options {
    gix::open::Options::isolated().config_overrides([
        "user.name=t",
        "user.email=t@example.com",
        "init.defaultBranch=main",
    ])
}

pub fn open(dir: &Path) -> gix::Repository {
    gix::open_opts(dir, options()).unwrap_or_else(|e| panic!("opening {}: {e}", dir.display()))
}

fn init(dir: &Path) -> String {
    std::fs::create_dir_all(dir).unwrap();
    gix::ThreadSafeRepository::init_opts(
        dir,
        gix::create::Kind::WithWorktree,
        Default::default(),
        options(),
    )
    .unwrap_or_else(|e| panic!("initializing {}: {e}", dir.display()));
    std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    let config = dir.join(".git/config");
    let mut text = std::fs::read_to_string(&config).unwrap_or_default();
    text.push_str("[user]\n\tname = t\n\temail = t@example.com\n");
    std::fs::write(config, text).unwrap();
    String::new()
}

fn init_bare(dir: &Path) -> String {
    std::fs::create_dir_all(dir).unwrap();
    gix::ThreadSafeRepository::init_opts(
        dir,
        gix::create::Kind::Bare,
        Default::default(),
        options(),
    )
    .unwrap_or_else(|e| panic!("initializing {}: {e}", dir.display()));
    std::fs::write(dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    String::new()
}

fn clone_bare(from: &Path, to: &Path) -> String {
    copy_dir(&from.join(".git"), to);
    let _ = std::fs::remove_file(to.join("index"));
    let _ = std::fs::remove_file(to.join(STAGED));
    let config = std::fs::read_to_string(to.join("config")).unwrap_or_default();
    let config = if config.contains("bare = false") {
        config.replace("bare = false", "bare = true")
    } else {
        format!("{config}[core]\n\tbare = true\n")
    };
    std::fs::write(to.join("config"), config).unwrap();
    String::new()
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn stage(dir: &Path, paths: &[&str]) -> String {
    let file = open(dir).git_dir().join(STAGED);
    let mut staged = std::fs::read_to_string(&file).unwrap_or_default();
    for path in paths {
        staged.push_str(path);
        staged.push('\n');
    }
    std::fs::write(file, staged).unwrap();
    String::new()
}

fn head_commit(repo: &gix::Repository) -> Option<ObjectId> {
    repo.head_id().ok().map(|id| id.detach())
}

fn head_tree(repo: &gix::Repository) -> ObjectId {
    match head_commit(repo) {
        Some(commit) => repo
            .find_commit(commit)
            .unwrap()
            .tree_id()
            .unwrap()
            .detach(),
        None => ObjectId::empty_tree(repo.object_hash()),
    }
}

fn workdir(repo: &gix::Repository) -> PathBuf {
    repo.workdir().expect("a worktree").to_path_buf()
}

fn worktree_files(root: &Path, relative: &Path, files: &mut Vec<String>) {
    for entry in std::fs::read_dir(root.join(relative)).unwrap() {
        let entry = entry.unwrap();
        if relative.as_os_str().is_empty() && entry.file_name() == ".git" {
            continue;
        }
        let path = relative.join(entry.file_name());
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            worktree_files(root, &path, files);
        } else {
            files.push(slashed(&path));
        }
    }
}

fn slashed(path: &Path) -> String {
    path.components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn tracked(repo: &gix::Repository, tree: ObjectId) -> Vec<String> {
    let mut files = Vec::new();
    collect_tree(repo, tree, "", &mut files);
    files
}

fn collect_tree(repo: &gix::Repository, tree: ObjectId, prefix: &str, files: &mut Vec<String>) {
    let tree = repo.find_tree(tree).unwrap();
    for entry in tree.iter() {
        let entry = entry.unwrap();
        let path = format!("{prefix}{}", entry.filename());
        match entry.mode().kind() {
            EntryKind::Tree => collect_tree(repo, entry.object_id(), &format!("{path}/"), files),
            _ => files.push(path),
        }
    }
}

fn blob(repo: &gix::Repository, file: &Path) -> (EntryKind, ObjectId) {
    let metadata = std::fs::symlink_metadata(file).unwrap();
    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(file).unwrap();
        let id = repo
            .write_blob(target.to_string_lossy().as_bytes())
            .unwrap()
            .detach();
        return (EntryKind::Link, id);
    }
    let id = repo
        .write_blob(std::fs::read(file).unwrap())
        .unwrap()
        .detach();
    (kind_of(&metadata), id)
}

#[cfg(unix)]
fn kind_of(metadata: &std::fs::Metadata) -> EntryKind {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o111 != 0 {
        EntryKind::BlobExecutable
    } else {
        EntryKind::Blob
    }
}

#[cfg(not(unix))]
fn kind_of(_: &std::fs::Metadata) -> EntryKind {
    EntryKind::Blob
}

fn staged_tree(repo: &gix::Repository, all_tracked: bool) -> ObjectId {
    let root = workdir(repo);
    let file = repo.git_dir().join(STAGED);
    let staged = std::fs::read_to_string(&file).unwrap_or_default();
    let _ = std::fs::remove_file(&file);
    let base = head_tree(repo);
    let mut paths: Vec<String> = Vec::new();
    let everything = staged.lines().any(|line| line == "-A");
    let mut editor = if everything {
        worktree_files(&root, Path::new(""), &mut paths);
        repo.edit_tree(ObjectId::empty_tree(repo.object_hash()))
            .unwrap()
    } else {
        for line in staged.lines() {
            let path = root.join(line);
            if path.is_dir() {
                let mut inside = Vec::new();
                worktree_files(&root, Path::new(line), &mut inside);
                paths.extend(inside);
            } else {
                paths.push(line.to_string());
            }
        }
        if all_tracked {
            paths.extend(tracked(repo, base));
        }
        repo.edit_tree(base).unwrap()
    };
    for path in paths {
        let file = root.join(&path);
        if std::fs::symlink_metadata(&file).is_ok() {
            let (kind, id) = blob(repo, &file);
            editor.upsert(path.as_str(), kind, id).unwrap();
        } else {
            editor.remove(path.as_str()).unwrap();
        }
    }
    editor.write().unwrap().detach()
}

fn write_index(repo: &gix::Repository, tree: ObjectId) {
    let mut index = repo.index_from_tree(&tree).unwrap();
    index.write(Default::default()).unwrap();
}

fn commit(dir: &Path, all_tracked: bool, message: &str) -> String {
    let repo = open(dir);
    let tree = staged_tree(&repo, all_tracked);
    let parents: Vec<ObjectId> = head_commit(&repo).into_iter().collect();
    let id = repo
        .commit("HEAD", format!("{message}\n"), tree, parents)
        .unwrap()
        .detach();
    write_index(&repo, tree);
    id.to_string()
}

pub fn rev_parse(repo: &gix::Repository, spec: &str) -> ObjectId {
    repo.rev_parse_single(spec)
        .unwrap_or_else(|e| panic!("rev-parse {spec}: {e}"))
        .detach()
}

fn commit_of(repo: &gix::Repository, spec: &str) -> ObjectId {
    repo.find_object(rev_parse(repo, spec))
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id
}

fn lightweight(dir: &Path, name: &str, rev: &str, constraint: PreviousValue) -> String {
    let repo = open(dir);
    let target = commit_of(&repo, rev);
    repo.tag_reference(name, target, constraint).unwrap();
    String::new()
}

fn annotated(dir: &Path, name: &str, message: &str) -> String {
    let repo = open(dir);
    let target = commit_of(&repo, "HEAD");
    let tagger = repo.committer().unwrap().unwrap();
    repo.tag(
        name,
        target,
        gix::objs::Kind::Commit,
        Some(tagger),
        format!("{message}\n"),
        PreviousValue::MustNotExist,
    )
    .unwrap();
    String::new()
}

fn tags(dir: &Path) -> String {
    let repo = open(dir);
    let mut names: Vec<String> = repo
        .references()
        .unwrap()
        .tags()
        .unwrap()
        .map(|reference| {
            reference
                .unwrap()
                .name()
                .as_bstr()
                .strip_prefix(b"refs/tags/")
                .unwrap()
                .to_str_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names.join("\n")
}

fn branch(dir: &Path, name: &str, start: &str) -> String {
    let repo = open(dir);
    let head = commit_of(&repo, start);
    repo.reference(
        format!("refs/heads/{name}").as_str(),
        head,
        PreviousValue::MustNotExist,
        "branch: Created from HEAD",
    )
    .unwrap();
    String::new()
}

fn replace_worktree(repo: &gix::Repository, from: ObjectId, to: ObjectId) {
    let root = workdir(repo);
    for path in tracked(repo, from) {
        let _ = std::fs::remove_file(root.join(path));
    }
    let tree = repo.find_tree(to).unwrap();
    super::write_tree(&tree, &root).unwrap();
    write_index(repo, to);
}

fn checkout(dir: &Path, name: &str) -> String {
    let repo = open(dir);
    let from = head_tree(&repo);
    let target = commit_of(&repo, &format!("refs/heads/{name}"));
    std::fs::write(
        repo.git_dir().join("HEAD"),
        format!("ref: refs/heads/{name}\n"),
    )
    .unwrap();
    let to = repo
        .find_commit(target)
        .unwrap()
        .tree_id()
        .unwrap()
        .detach();
    replace_worktree(&repo, from, to);
    String::new()
}

fn reset_hard(dir: &Path, rev: &str) -> String {
    let repo = open(dir);
    let from = head_tree(&repo);
    let target = commit_of(&repo, rev);
    let branch = repo.head_name().unwrap().expect("HEAD names a branch");
    repo.reference(branch, target, PreviousValue::Any, "reset: moving")
        .unwrap();
    let to = repo
        .find_commit(target)
        .unwrap()
        .tree_id()
        .unwrap()
        .detach();
    replace_worktree(&repo, from, to);
    String::new()
}

fn object_kind(dir: &Path, spec: &str) -> String {
    let repo = open(dir);
    let kind = repo.find_object(rev_parse(&repo, spec)).unwrap().kind;
    kind.to_string()
}

fn remotes(dir: &Path) -> String {
    let repo = open(dir);
    repo.remote_names()
        .iter()
        .map(|name| name.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}
