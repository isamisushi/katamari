//! GitHub owns the PR base/head relationship, including fork heads and
//! merged PRs. Fetch its aggregate diff with the HTTP client already used
//! for updates and LSP downloads; no extra executable or checkout is needed.

use anyhow::{Context, Result, bail};
use std::io::Read;
use std::num::NonZeroU64;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Resolve only repository identity locally. Guessing a range from main or
/// a synthetic merge ref would give incorrect results for release branches,
/// conflicting PRs, or PRs whose base has advanced since merging.
pub fn pull_request_diff(repo_root: &Path, number: NonZeroU64) -> Result<String> {
    let env = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
    let host = env("GH_HOST").unwrap_or_else(|| "github.com".into());
    let source = match env("GH_REPO") {
        Some(repo) => repo,
        None => remote_url(repo_root)?,
    };
    let repository = Repository::parse(&source, &host)?;
    let token = token_for_host(&repository.host, &env);
    request_diff(
        &http_agent(),
        &repository.diff_url(number),
        token.as_deref(),
    )
}

struct Repository {
    host: String,
    owner: String,
    name: String,
}

impl Repository {
    /// Accept Git's common HTTPS/SSH URL spellings and GH_REPO's shorthand.
    /// Only github.com or an explicitly trusted GH_HOST may receive tokens;
    /// a clone's remote URL alone must never select a credential destination.
    fn parse(source: &str, configured_host: &str) -> Result<Self> {
        let configured_host = configured_host.to_ascii_lowercase();
        let (host, path) = if let Some(rest) = source
            .strip_prefix("https://")
            .or_else(|| source.strip_prefix("ssh://"))
            .or_else(|| source.strip_prefix("git://"))
        {
            let (authority, path) = rest
                .split_once('/')
                .context("expected a GitHub repository URL")?;
            let host = authority.rsplit('@').next().unwrap();
            // An SSH port is not an HTTPS API port. GH_HOST may provide
            // a separate Enterprise API port explicitly.
            let host = if source.starts_with("ssh://") {
                host.split_once(':').map_or(host, |(host, _)| host)
            } else {
                host
            };
            (host, path)
        } else if let Some(rest) = source.strip_prefix("git@") {
            rest.split_once(':')
                .context("expected git@HOST:OWNER/REPO")?
        } else {
            let parts: Vec<_> = source.split('/').collect();
            match parts.as_slice() {
                [_, _] => (configured_host.as_str(), source),
                [host, _, _] => (*host, source.split_once('/').unwrap().1),
                _ => bail!("expected a GitHub remote URL or GH_REPO=[HOST/]OWNER/REPO"),
            }
        };
        let mut host = host.to_ascii_lowercase();
        if (source.starts_with("ssh://") || source.starts_with("git@"))
            && configured_host.split(':').next() == Some(host.as_str())
        {
            host = configured_host.clone();
        }
        let valid_host = host.split_once(':').map_or_else(
            || valid_hostname(&host),
            |(name, port)| valid_hostname(name) && port.parse::<u16>().is_ok_and(|p| p != 0),
        );
        if !valid_host || (host != "github.com" && host != configured_host) {
            bail!("unsupported GitHub host; set GH_HOST explicitly for GitHub Enterprise");
        }
        let path = path.trim_end_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        let (owner, name) = path.split_once('/').context("expected OWNER/REPO")?;
        if !valid_component(owner) || !valid_component(name) {
            bail!("invalid GitHub repository path; expected OWNER/REPO");
        }
        Ok(Self {
            host,
            owner: owner.into(),
            name: name.into(),
        })
    }

    fn diff_url(&self, number: NonZeroU64) -> String {
        let api = if self.host == "github.com" {
            "https://api.github.com".into()
        } else {
            format!("https://{}/api/v3", self.host)
        };
        format!("{api}/repos/{}/{}/pulls/{number}", self.owner, self.name)
    }
}

fn valid_hostname(host: &str) -> bool {
    !host.is_empty()
        && host.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// Reject traversal, percent escapes, queries, and fragments before building
/// an authenticated URL. GitHub owner/repository names need none of them.
fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

/// Keep github.com and Enterprise credentials separate even when a shell
/// exports both. Never inspect gh's credential files or invoke its binary.
fn token_for_host(host: &str, env: &impl Fn(&str) -> Option<String>) -> Option<String> {
    let keys = if host == "github.com" {
        ["GH_TOKEN", "GITHUB_TOKEN"]
    } else {
        ["GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN"]
    };
    keys.into_iter()
        .filter_map(env)
        .find(|value| !value.is_empty())
}

/// Prefer the conventional upstream remote in a fork clone, then origin.
/// An explicit GH_REPO bypasses remote discovery; ambiguous custom remote
/// names fail rather than silently reviewing another repository's PR number.
fn remote_url(root: &Path) -> Result<String> {
    let remotes = git_output(root, &["remote"])?;
    let names: Vec<_> = remotes.lines().collect();
    let name = if names.contains(&"upstream") {
        "upstream"
    } else if names.contains(&"origin") {
        "origin"
    } else if names.len() == 1 {
        names[0]
    } else {
        bail!("cannot select a GitHub remote; set GH_REPO=[HOST/]OWNER/REPO");
    };
    git_output(root, &["remote", "get-url", "--", name])
}

fn git_output(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .context("failed to read Git remotes")?;
    if !output.status.success() {
        // Git diagnostics may contain a credential embedded in a remote URL.
        bail!("could not read Git remotes; set GH_REPO=[HOST/]OWNER/REPO");
    }
    Ok(String::from_utf8(output.stdout)
        .context("Git remote is not UTF-8")?
        .trim()
        .into())
}

/// Redirects are refused so a renamed repository or proxy cannot move a
/// bearer credential to another endpoint. GH_REPO can select the new name.
fn http_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .redirects(0)
        .build()
}

/// Never show an error page or truncated response as a successful review.
/// Bound memory use, reject invalid UTF-8, and retain an empty diff as valid.
fn request_diff(agent: &ureq::Agent, url: &str, token: Option<&str>) -> Result<String> {
    let mut request = agent
        .get(url)
        .set(
            "User-Agent",
            concat!("katamari/", env!("CARGO_PKG_VERSION")),
        )
        .set("Accept", "application/vnd.github.diff");
    if let Some(token) = token {
        if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
            bail!("GitHub token contains invalid characters");
        }
        request = request.set("Authorization", &format!("Bearer {token}"));
    }
    let response = match request.call() {
        Ok(response) => response,
        Err(ureq::Error::Status(status, _)) => bail!(
            "GitHub PR request failed (HTTP {status}); {}",
            match status {
                401 =>
                    "check GH_TOKEN/GITHUB_TOKEN (Enterprise: GH_ENTERPRISE_TOKEN/GITHUB_ENTERPRISE_TOKEN)",
                403 | 429 => "check token permissions or wait for the API rate limit to reset",
                404 =>
                    "check the repository and PR number; private PRs require a token with repository access",
                _ => "GitHub could not return the diff; retry later",
            }
        ),
        Err(ureq::Error::Transport(error)) => {
            // Do not print request headers, URLs from redirects, or token values.
            bail!(
                "GitHub PR request failed ({:?}); check network, TLS, and proxy settings",
                error.kind()
            )
        }
    };
    if response.status() != 200 {
        bail!(
            "GitHub PR request returned HTTP {}; redirects are not followed; check GH_REPO",
            response.status()
        );
    }
    const MAX_DIFF_BYTES: u64 = 32 * 1024 * 1024;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_DIFF_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("failed to read the complete GitHub PR diff")?;
    if bytes.len() as u64 > MAX_DIFF_BYTES {
        bail!("GitHub PR diff exceeds 32 MiB; refusing to show an incomplete review");
    }
    let diff = String::from_utf8(bytes).context("GitHub PR diff is not UTF-8")?;
    if !diff.is_empty() && !diff.starts_with("diff --git ") {
        bail!("GitHub returned an unexpected response instead of a unified diff");
    }
    Ok(diff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::thread::{self, JoinHandle};

    const DIFF: &str = "diff --git a/pr-only.txt b/pr-only.txt\nnew file mode 100644\n--- /dev/null\n+++ b/pr-only.txt\n@@ -0,0 +1 @@\n+PR_CONTENT\n";

    #[test]
    fn parses_remote_urls_and_explicit_repositories() {
        for source in [
            "https://github.com/owner/project.git",
            "https://user:secret@github.com/owner/project.git/",
            "git@github.com:owner/project.git",
            "ssh://git@github.com/owner/project.git",
            "ssh://git@github.com:22/owner/project.git",
            "git://github.com/owner/project.git",
            "owner/project",
            "github.com/owner/project",
            "https://GITHUB.COM/owner/project",
        ] {
            let repo = Repository::parse(source, "github.com").unwrap();
            assert_eq!(
                repo.diff_url(NonZeroU64::new(42).unwrap()),
                "https://api.github.com/repos/owner/project/pulls/42",
                "{source}"
            );
        }
        let repo = Repository::parse(
            "git@enterprise.example:owner/project.git",
            "enterprise.example",
        )
        .unwrap();
        assert_eq!(
            repo.diff_url(NonZeroU64::new(1).unwrap()),
            "https://enterprise.example/api/v3/repos/owner/project/pulls/1"
        );
        let repo = Repository::parse(
            "ssh://git@enterprise.example:2222/owner/project",
            "enterprise.example:8443",
        )
        .unwrap();
        assert!(
            repo.diff_url(NonZeroU64::new(1).unwrap())
                .starts_with("https://enterprise.example:8443/api/v3/")
        );
    }

    #[test]
    fn rejects_untrusted_hosts_and_paths_before_authentication() {
        for source in [
            "https://github.com.evil.example/owner/project",
            "git@gitlab.com:owner/project",
            "https://github.com@evil.example/owner/project",
            "https://github.com/owner/../project",
            "owner/..",
            "../project",
            "owner/.",
            "owner/project?query",
            "owner/project#fragment",
            "owner/%2e%2e",
            "owner/project/extra",
            "file:///owner/project",
            "http://github.com/owner/project",
            "https://github.com:443/owner/project",
            "/tmp/repo",
        ] {
            assert!(Repository::parse(source, "github.com").is_err(), "{source}");
        }
        assert!(Repository::parse("owner/repo", "evil.example/path").is_err());
        assert!(Repository::parse("owner/repo", "enterprise.example:0").is_err());
    }

    #[test]
    fn tokens_are_optional_and_scoped_to_the_selected_host() {
        let env = |key: &str| match key {
            "GH_TOKEN" => Some("public-token".into()),
            "GITHUB_TOKEN" => Some("public-fallback".into()),
            "GH_ENTERPRISE_TOKEN" => Some("enterprise-token".into()),
            "GITHUB_ENTERPRISE_TOKEN" => Some("enterprise-fallback".into()),
            _ => None,
        };
        assert_eq!(
            token_for_host("github.com", &env).as_deref(),
            Some("public-token")
        );
        assert_eq!(
            token_for_host("enterprise.example", &env).as_deref(),
            Some("enterprise-token")
        );
        let env = |key: &str| (key == "GITHUB_TOKEN").then(|| "fallback".into());
        assert_eq!(
            token_for_host("github.com", &env).as_deref(),
            Some("fallback")
        );
        assert!(token_for_host("enterprise.example", &env).is_none());
        assert!(token_for_host("github.com", &|_| None).is_none());
    }

    fn git(root: &Path, args: &[&str]) -> String {
        git_output(root, args).unwrap()
    }

    #[test]
    fn remote_selection_uses_upstream_origin_then_a_single_custom_remote() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]);
        assert!(remote_url(root).is_err());
        git(
            root,
            &["remote", "add", "custom", "git@github.com:owner/custom.git"],
        );
        assert_eq!(remote_url(root).unwrap(), "git@github.com:owner/custom.git");
        git(
            root,
            &["remote", "add", "other", "https://github.com/owner/other"],
        );
        assert!(remote_url(root).is_err());
        git(
            root,
            &["remote", "add", "origin", "https://github.com/owner/fork"],
        );
        assert_eq!(remote_url(root).unwrap(), "https://github.com/owner/fork");
        git(
            root,
            &[
                "remote",
                "add",
                "upstream",
                "https://github.com/owner/upstream",
            ],
        );
        assert_eq!(
            remote_url(root).unwrap(),
            "https://github.com/owner/upstream"
        );
    }

    /// A local HTTP peer tests the actual ureq request/response boundary,
    /// without tokens, internet access, fake CLIs, or process-wide env edits.
    fn server(status: u16, headers: &str, body: Vec<u8>) -> (String, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/repos/owner/project/pulls/42",
            listener.local_addr().unwrap()
        );
        listener.set_nonblocking(true).unwrap();
        let headers = headers.to_owned();
        let handle = thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "HTTP request never arrived"
                        );
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            // macOS can inherit the listener's nonblocking mode on accept.
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                assert!(request.len() < 16_384);
            }
            write!(stream, "HTTP/1.1 {status} Test\r\n").unwrap();
            if !headers.contains("Content-Length:") {
                write!(stream, "Content-Length: {}\r\n", body.len()).unwrap();
            }
            write!(stream, "Connection: close\r\n{headers}\r\n").unwrap();
            let _ = stream.write_all(&body);
            String::from_utf8(request).unwrap()
        });
        (url, handle)
    }

    #[test]
    fn public_and_authenticated_requests_fetch_the_aggregate_diff() {
        for token in [None, Some("test-token")] {
            let (url, handle) = server(200, "", DIFF.as_bytes().to_vec());
            assert_eq!(request_diff(&http_agent(), &url, token).unwrap(), DIFF);
            let request = handle.join().unwrap().to_ascii_lowercase();
            assert!(request.starts_with("get /repos/owner/project/pulls/42 http/1.1\r\n"));
            assert!(request.contains("accept: application/vnd.github.diff\r\n"));
            assert!(request.contains("user-agent: katamari/"));
            assert_eq!(
                request.contains("authorization: bearer test-token"),
                token.is_some()
            );
            if token.is_none() {
                assert!(!request.contains("authorization:"));
            }
        }
    }

    #[test]
    fn rejects_http_errors_and_redirects_without_echoing_the_response() {
        for status in [301, 302, 401, 403, 404, 429, 500] {
            let (url, handle) = server(
                status,
                "Location: https://untrusted.example/\r\n",
                b"secret from response".to_vec(),
            );
            let error = request_diff(&http_agent(), &url, Some("test-token"))
                .unwrap_err()
                .to_string();
            assert!(error.contains(&format!("HTTP {status}")), "{error}");
            assert!(
                !error.contains("secret") && !error.contains("test-token"),
                "{error}"
            );
            handle.join().unwrap();
        }
    }

    #[test]
    fn refuses_malformed_or_oversized_diffs_but_accepts_empty_changes() {
        let (url, handle) = server(200, "", Vec::new());
        assert_eq!(request_diff(&http_agent(), &url, None).unwrap(), "");
        handle.join().unwrap();
        for body in [
            vec![0xff],
            b"{\"message\":\"not a diff\"}".to_vec(),
            b"<html>login</html>".to_vec(),
            vec![b'x'; 32 * 1024 * 1024 + 1],
        ] {
            let (url, handle) = server(200, "", body);
            assert!(request_diff(&http_agent(), &url, None).is_err());
            handle.join().unwrap();
        }
        let error = request_diff(&http_agent(), "http://127.0.0.1:1", Some("bad\ntoken"))
            .unwrap_err()
            .to_string();
        assert_eq!(error, "GitHub token contains invalid characters");
    }

    #[test]
    fn incomplete_responses_and_connection_failures_do_not_become_empty_diffs() {
        let (url, handle) = server(200, "Content-Length: 10000\r\n", DIFF.as_bytes().to_vec());
        let error = request_diff(&http_agent(), &url, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("complete GitHub PR diff"), "{error}");
        handle.join().unwrap();
        let error = request_diff(&http_agent(), &url, Some("test-secret"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("network"), "{error}");
        assert!(!error.contains("test-secret"), "{error}");
    }

    /// The downloaded snapshot goes through the actual parser. A
    /// non-main base, missing head ref, and dirty checkout must not affect it.
    #[test]
    fn remote_snapshot_ignores_local_refs_and_working_tree_contents() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q", "-b", "main"]);
        let commit = |message: &str| {
            git(root, &["add", "--all"]);
            git(
                root,
                &[
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.com",
                    "commit",
                    "-qm",
                    message,
                ],
            );
        };
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        commit("main");
        git(root, &["checkout", "-qb", "release"]);
        std::fs::write(root.join("release-only.txt"), "already in base\n").unwrap();
        commit("release base");
        git(root, &["checkout", "-qb", "fork-head"]);
        std::fs::write(root.join("pr-only.txt"), "PR_CONTENT\n").unwrap();
        commit("PR");
        let snapshot = git(
            root,
            &["diff", "--no-ext-diff", "--no-color", "release...HEAD"],
        );
        git(root, &["checkout", "-q", "main"]);
        git(root, &["branch", "-D", "fork-head"]);
        std::fs::write(root.join("pr-only.txt"), "UNRELATED_LOCAL_CONTENT\n").unwrap();
        let before = git(root, &["status", "--porcelain=v1"]);
        let head = git(root, &["rev-parse", "HEAD"]);
        let (url, handle) = server(200, "", snapshot.as_bytes().to_vec());
        let diff = request_diff(&http_agent(), &url, None).unwrap();
        handle.join().unwrap();
        assert_eq!(diff, snapshot);
        assert!(!diff.contains("release-only.txt") && !diff.contains("UNRELATED_LOCAL_CONTENT"));
        let files = crate::diff::parse_unified_diff(&diff);
        assert_eq!(files.len(), 1);
        assert_eq!(git(root, &["status", "--porcelain=v1"]), before);
        assert_eq!(git(root, &["rev-parse", "HEAD"]), head);
    }
}
