//! Read git objects as data, never checkout or execute reviewed code.
use base64::Engine;
use std::path::{Path, PathBuf};
use tokio::process::Command;

use crate::review::{Project, Result, ReviewError};
use crate::review_github::{Github, PullRequest};

#[derive(Debug, Clone)]
pub(crate) struct Change {
    pub(crate) path: String,
    pub(crate) previous: Option<String>,
    pub(crate) diff: String,
    pub(crate) content: String,
}
pub(crate) struct Snapshot {
    pub(crate) head: String,
    pub(crate) changes: Vec<Change>,
    pub(crate) corpus: Vec<(String, String)>,
    pub(crate) skipped: Vec<String>,
}

async fn git(path: &Path, args: &[&str], token: Option<&str>) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    command
        .arg("--literal-pathspecs")
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        .arg("core.fsmonitor=false")
        .arg("-c")
        .arg("diff.external=")
        .arg("-c")
        .arg("core.pager=cat")
        .arg("-C")
        .arg(path)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_EXTERNAL_DIFF")
        .env_remove("GIT_DIFF_OPTS")
        .kill_on_drop(true);
    if let Some(token) = token {
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
        command
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "http.https://github.com/.extraheader")
            .env(
                "GIT_CONFIG_VALUE_0",
                format!("AUTHORIZATION: basic {encoded}"),
            );
    }
    let output = command.output().await?;
    if !output.status.success() {
        // Git's stderr may contain remote-supplied text; don't reflect it into UI/logs.
        return Err(ReviewError::Invalid(format!(
            "Git: операция {} не выполнена ({})",
            args.first().unwrap_or(&""),
            output.status
        )));
    }
    Ok(output.stdout)
}
async fn text(path: &Path, args: &[&str]) -> Result<String> {
    String::from_utf8(git(path, args, None).await?)
        .map_err(|_| ReviewError::Invalid("Git вернул путь или текст не в UTF-8".into()))
}
pub(crate) async fn discover(path: &Path) -> Result<(PathBuf, String)> {
    let root = text(path, &["rev-parse", "--show-toplevel"]).await?;
    let root = std::fs::canonicalize(root.trim())?;
    let repository = text(&root, &["remote", "get-url", "origin"])
        .await
        .and_then(|value| crate::review::remote_repository(&value))
        .unwrap_or_default();
    Ok((root, repository))
}
fn valid_sha(sha: &str) -> bool {
    matches!(sha.len(), 40 | 64) && sha.bytes().all(|c| c.is_ascii_hexdigit())
}
pub(crate) async fn pull_snapshot(
    project: &Project,
    pull: &PullRequest,
    github: &Github,
    directory: &Path,
) -> Result<Snapshot> {
    if !valid_sha(&pull.head.sha) || !valid_sha(&pull.base.sha) {
        return Err(ReviewError::Invalid(
            "GitHub вернул некорректный commit SHA".into(),
        ));
    }
    if pull
        .base
        .repo
        .as_ref()
        .is_some_and(|repo| repo.full_name != project.repository)
    {
        return Err(ReviewError::Invalid(
            "Целевой репозиторий PR не совпадает с проектом".into(),
        ));
    }
    let remote = format!("https://github.com/{}.git", project.repository);
    snapshot_from_remote(pull, github.token(), directory, &remote, &project.sources).await
}

async fn snapshot_from_remote(
    pull: &PullRequest,
    token: Option<&str>,
    directory: &Path,
    remote: &str,
    sources: &[String],
) -> Result<Snapshot> {
    let repo = directory.join("git");
    std::fs::create_dir_all(&repo)?;
    git(&repo, &["init", "--bare", "--template="], None).await?;
    let head_ref = format!("refs/pull/{}/head:refs/agi-review/head", pull.number);
    git(
        &repo,
        &[
            "fetch",
            "--no-tags",
            "--no-recurse-submodules",
            remote,
            &pull.base.sha,
            &head_ref,
        ],
        token,
    )
    .await?;
    if text(&repo, &["rev-parse", "refs/agi-review/head"])
        .await?
        .trim()
        != pull.head.sha
    {
        return Err(ReviewError::Invalid(
            "PR обновлён во время получения Git-объектов; будет проверен новый commit".into(),
        ));
    }
    let base = text(&repo, &["merge-base", &pull.base.sha, &pull.head.sha]).await?;
    snapshot(&repo, base.trim(), Some(&pull.head.sha), sources).await
}
pub(crate) async fn local_snapshot(project: &Project) -> Result<Snapshot> {
    let head = text(&project.path, &["rev-parse", "HEAD"]).await?;
    snapshot(&project.path, head.trim(), None, &project.sources).await
}

pub(crate) fn safe_path(path: &str) -> bool {
    let path = Path::new(path);
    if path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return false;
    }
    for part in path.components() {
        let part = part.as_os_str().to_string_lossy().to_ascii_lowercase();
        if part.starts_with(".env")
            || part.starts_with("id_rsa")
            || part.starts_with("id_ed25519")
            || matches!(
                part.as_str(),
                ".git" | "target" | "node_modules" | "dist" | "build" | "vendor" | ".aws" | ".ssh"
            )
        {
            return false;
        }
    }
    if path.to_string_lossy().chars().any(char::is_control) {
        return false;
    }
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    !matches!(
        ext.as_str(),
        "pem"
            | "key"
            | "p12"
            | "pfx"
            | "png"
            | "jpg"
            | "jpeg"
            | "webp"
            | "gif"
            | "pdf"
            | "docx"
            | "zip"
            | "sqlite3"
            | "db"
            | "lock"
    )
}
fn selected(path: &str, sources: &[String]) -> bool {
    sources.iter().any(|s| {
        s == "." || path == s || path.starts_with(&format!("{}/", s.trim_end_matches('/')))
    })
}
async fn read_source(repo: &Path, revision: Option<&str>, path: &str) -> Result<Option<String>> {
    if !safe_path(path) {
        return Ok(None);
    }
    let bytes = if let Some(revision) = revision {
        let object = format!("{revision}:{path}");
        if text(repo, &["cat-file", "-t", &object]).await?.trim() != "blob" {
            return Ok(None);
        }
        let size = text(repo, &["cat-file", "-s", &object])
            .await?
            .trim()
            .parse::<usize>()
            .map_err(|_| ReviewError::Invalid("Git: неверный размер объекта".into()))?;
        if size > 256 * 1024 {
            return Ok(None);
        }
        git(repo, &["show", &object], None).await?
    } else {
        let mut current = repo.to_path_buf();
        for component in Path::new(path).components() {
            current.push(component);
            match std::fs::symlink_metadata(&current) {
                Ok(meta) if meta.file_type().is_symlink() => return Ok(None),
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e.into()),
            }
        }
        let meta = std::fs::metadata(&current)?;
        if !meta.is_file() || meta.len() > 256 * 1024 {
            return Ok(None);
        }
        std::fs::read(current)?
    };
    if bytes.contains(&0) {
        return Ok(None);
    }
    Ok(String::from_utf8(bytes)
        .ok()
        .filter(|s| !s.trim().is_empty()))
}

pub(crate) async fn snapshot(
    repo: &Path,
    base: &str,
    head: Option<&str>,
    sources: &[String],
) -> Result<Snapshot> {
    let mut args = vec![
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--find-renames",
        "--name-status",
        "-z",
        base,
    ];
    if let Some(head) = head {
        args.push(head);
    }
    args.push("--");
    let names = text(repo, &args).await?;
    let mut parts = names.split('\0').filter(|p| !p.is_empty());
    let mut changes = Vec::new();
    let mut skipped = Vec::new();
    while let Some(status) = parts.next() {
        let first = parts
            .next()
            .ok_or_else(|| ReviewError::Invalid("Git: неполный список изменений".into()))?;
        let (path, previous) = if status.starts_with(['R', 'C']) {
            (
                parts
                    .next()
                    .ok_or_else(|| ReviewError::Invalid("Git: неполное переименование".into()))?,
                Some(first.to_owned()),
            )
        } else {
            (first, None)
        };
        if !safe_path(path)
            || previous.as_deref().is_some_and(|old| !safe_path(old))
            || changes.len() >= 100
        {
            skipped.push(format!("{path}: исключённый путь или лимит 100 файлов"));
            continue;
        }
        let content = if status.starts_with('D') {
            read_source(repo, Some(base), path).await?
        } else {
            read_source(repo, head, path).await?
        };
        let Some(content) = content else {
            skipped.push(format!("{path}: не поддерживается или превышает 256 KiB"));
            continue;
        };
        if !status.starts_with(['A', 'D'])
            && read_source(repo, Some(base), previous.as_deref().unwrap_or(path))
                .await?
                .is_none()
        {
            skipped.push(format!(
                "{path}: прежний файл не поддерживается или превышает 256 KiB"
            ));
            continue;
        }
        let mut diff_args = vec![
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--find-renames",
            "--unified=5",
            base,
        ];
        if let Some(head) = head {
            diff_args.push(head);
        }
        diff_args.push("--");
        diff_args.push(path);
        if let Some(previous) = &previous {
            diff_args.push(previous);
        }
        let diff = text(repo, &diff_args).await?;
        if diff.lines().any(|line| {
            line.starts_with("Binary files ")
                || line == "GIT binary patch"
                || line.starts_with("new mode 120000")
                || line.starts_with("old mode 120000")
                || line.starts_with("new file mode 120000")
                || line.starts_with("deleted file mode 120000")
                || line.ends_with("mode 160000")
        }) {
            skipped.push(format!("{path}: бинарный файл, symlink или submodule"));
            continue;
        }
        changes.push(Change {
            path: path.into(),
            previous,
            diff,
            content,
        });
    }
    let list = if let Some(head) = head {
        text(repo, &["ls-tree", "-r", "-z", head]).await?
    } else {
        text(repo, &["ls-files", "--stage", "-z"]).await?
    };
    let mut corpus = Vec::new();
    let mut corpus_bytes = 0;
    for item in list.split('\0').filter(|s| !s.is_empty()) {
        let Some((metadata, path)) = item.split_once('\t') else {
            continue;
        };
        if !selected(path, sources) || !safe_path(path) {
            continue;
        }
        if !metadata.starts_with("100") {
            skipped.push(format!("{path}: symlink/submodule исключён из RAG"));
            continue;
        }
        if let Some(content) = read_source(repo, head, path).await? {
            if corpus_bytes + content.len() > 10 * 1024 * 1024 || corpus.len() >= 2000 {
                skipped.push(format!("{path}: лимит RAG 10 MiB / 2000 файлов"));
                continue;
            }
            corpus_bytes += content.len();
            corpus.push((path.to_owned(), content));
        } else {
            skipped.push(format!("{path}: пропущен при построении RAG"));
        }
    }
    Ok(Snapshot {
        head: head.unwrap_or(base).into(),
        changes,
        corpus,
        skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn excludes_sensitive_paths_and_accepts_sources() {
        for path in [
            ".env",
            "src/.env.local",
            "../key",
            "/tmp/code",
            "cert.pem",
            "node_modules/a.js",
            "foo\nbar",
        ] {
            assert!(!safe_path(path), "{path}");
        }
        assert!(safe_path("src/api.rs"));
        assert!(safe_path("projetcDocs/llm_docs.md"));
    }
    #[tokio::test]
    async fn fetches_fork_head_from_base_pull_ref_and_rejects_changed_commit() {
        let directory = std::env::temp_dir().join(format!("agi-fork-ref-{}", uuid::Uuid::new_v4()));
        let origin = directory.join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "--template="], None).await.unwrap();
        std::fs::write(origin.join("a.rs"), "fn base() {}\n").unwrap();
        git(&origin, &["add", "."], None).await.unwrap();
        let commit = [
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "fixture",
        ];
        git(&origin, &commit, None).await.unwrap();
        let base = text(&origin, &["rev-parse", "HEAD"])
            .await
            .unwrap()
            .trim()
            .to_owned();
        std::fs::write(origin.join("a.rs"), "fn fork_head() {}\n").unwrap();
        let marker = directory.join("must-not-execute");
        std::fs::write(
            origin.join("script.sh"),
            format!("touch '{}'\n", marker.display()),
        )
        .unwrap();
        git(&origin, &["add", "."], None).await.unwrap();
        git(&origin, &commit, None).await.unwrap();
        let head = text(&origin, &["rev-parse", "HEAD"])
            .await
            .unwrap()
            .trim()
            .to_owned();
        git(&origin, &["update-ref", "refs/pull/1/head", &head], None)
            .await
            .unwrap();
        let mut pull: PullRequest = serde_json::from_value(serde_json::json!({"number":1,"title":"fork","head":{"sha":head,"repo":{"full_name":"other/fork"}},"base":{"sha":base,"repo":{"full_name":"owner/repo"}}})).unwrap();
        let snapshot = snapshot_from_remote(
            &pull,
            None,
            &directory.join("run"),
            origin.to_str().unwrap(),
            &[".".into()],
        )
        .await
        .unwrap();
        assert_eq!(snapshot.head, head);
        assert!(
            snapshot
                .changes
                .iter()
                .any(|c| c.diff.contains("fork_head"))
        );
        assert!(!marker.exists());
        pull.head.sha = base;
        assert!(
            snapshot_from_remote(
                &pull,
                None,
                &directory.join("stale"),
                origin.to_str().unwrap(),
                &[".".into()]
            )
            .await
            .is_err()
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[tokio::test]
    async fn snapshots_combined_local_changes_deletions_and_renames() {
        let dir = std::env::temp_dir().join(format!("agi-review-git-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "--template="], None).await.unwrap();
        std::fs::write(dir.join("a.rs"), "fn before() {}\n").unwrap();
        std::fs::write(dir.join("gone.rs"), "fn removed() {}\n").unwrap();
        git(&dir, &["add", "."], None).await.unwrap();
        git(
            &dir,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "initial",
            ],
            None,
        )
        .await
        .unwrap();
        let head = text(&dir, &["rev-parse", "HEAD"]).await.unwrap();
        git(&dir, &["mv", "a.rs", "b.rs"], None).await.unwrap();
        std::fs::write(dir.join("b.rs"), "fn before() {}\nfn after() {}\n").unwrap();
        std::fs::remove_file(dir.join("gone.rs")).unwrap();
        std::fs::write(dir.join("untracked.rs"), "not included").unwrap();
        let result = snapshot(&dir, head.trim(), None, &[".".into()])
            .await
            .unwrap();
        assert_eq!(result.changes.len(), 2);
        assert!(
            result
                .changes
                .iter()
                .any(|c| c.path == "gone.rs" && c.content.contains("removed"))
        );
        assert!(result.changes.iter().any(|c| c.path == "b.rs"
            && c.previous.as_deref() == Some("a.rs")
            && c.diff.contains("after")));
        assert!(!result.corpus.iter().any(|(p, _)| p == "untracked.rs"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
