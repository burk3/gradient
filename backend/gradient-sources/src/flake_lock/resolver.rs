/*
 * SPDX-FileCopyrightText: 2026 Wavelens GmbH <info@wavelens.io>
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 */

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;

use super::lock::LockedRef;
use super::narhash::{nar_hash_of_dir, tarball_response_nar_hash, tarball_source_nar_hash};
use super::tarball::{PinnedTarball, immutable_link};

const USER_AGENT: &str = "gradient-flake-lock";
const GITHUB_API: &str = "https://api.github.com";
const GITLAB_API: &str = "https://gitlab.com";
const TARBALL_MAX_REDIRECTS: usize = 10;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedRev {
    pub rev: String,
    pub ref_: Option<String>,
    pub nar_hash: String,
    pub last_modified: i64,
    pub tarball: Option<TarballPin>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TarballPin {
    pub url: String,
    pub rev_count: Option<u64>,
}

#[async_trait]
pub trait RevisionResolver: Send + Sync {
    async fn resolve(&self, reference: &LockedRef) -> Result<ResolvedRev>;
}

pub struct HttpRevisionResolver {
    client: reqwest::Client,
    probe: reqwest::Client,
    github_token: Option<String>,
    gitlab_token: Option<String>,
    ssh_key: Option<String>,
}

impl std::fmt::Debug for HttpRevisionResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redact = |o: &Option<String>| o.as_ref().map(|_| "<redacted>");
        f.debug_struct("HttpRevisionResolver")
            .field("github_token", &redact(&self.github_token))
            .field("gitlab_token", &redact(&self.gitlab_token))
            .field("ssh_key", &redact(&self.ssh_key))
            .finish_non_exhaustive()
    }
}

impl HttpRevisionResolver {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            probe: gradient_util::http::no_redirect_download_client().clone(),
            github_token: None,
            gitlab_token: None,
            ssh_key: None,
        }
    }

    pub fn with_github_token(mut self, token: Option<String>) -> Self {
        self.github_token = token;
        self
    }

    pub fn with_gitlab_token(mut self, token: Option<String>) -> Self {
        self.gitlab_token = token;
        self
    }

    pub fn with_ssh_key(mut self, key: Option<String>) -> Self {
        self.ssh_key = key;
        self
    }

    async fn resolve_github(
        &self,
        owner: &str,
        repo: &str,
        ref_: &Option<String>,
    ) -> Result<ResolvedRev> {
        let r = ref_.clone().unwrap_or_else(|| "HEAD".to_owned());
        let url = format!("{GITHUB_API}/repos/{owner}/{repo}/commits/{r}");
        let commit: GithubCommit = self
            .github_get(&url)
            .send()
            .await
            .context("github commit lookup")?
            .error_for_status()
            .context("github commit lookup failed")?
            .json()
            .await
            .context("parsing github commit")?;

        let last_modified = parse_rfc3339(&commit.commit.committer.date)?;
        let tarball = format!("{GITHUB_API}/repos/{owner}/{repo}/tarball/{}", commit.sha);
        let nar_hash = tarball_source_nar_hash(self.github_get(&tarball))
            .await?
            .nar_hash;

        Ok(ResolvedRev {
            rev: commit.sha,
            ref_: ref_.clone(),
            nar_hash,
            last_modified,
            tarball: None,
        })
    }

    async fn resolve_gitlab(
        &self,
        owner: &str,
        repo: &str,
        ref_: &Option<String>,
    ) -> Result<ResolvedRev> {
        let task = format!("{owner}/{repo}").replace('/', "%2F");
        let r = ref_.clone().unwrap_or_else(|| "HEAD".to_owned());
        let url = format!("{GITLAB_API}/api/v4/tasks/{task}/repository/commits/{r}");
        let commit: GitlabCommit = self
            .gitlab_get(&url)
            .send()
            .await
            .context("gitlab commit lookup")?
            .error_for_status()
            .context("gitlab commit lookup failed")?
            .json()
            .await
            .context("parsing gitlab commit")?;

        let last_modified = parse_rfc3339(&commit.committed_date)?;
        let tarball = format!(
            "{GITLAB_API}/api/v4/tasks/{task}/repository/archive.tar.gz?sha={}",
            commit.id
        );
        let nar_hash = tarball_source_nar_hash(self.gitlab_get(&tarball))
            .await?
            .nar_hash;

        Ok(ResolvedRev {
            rev: commit.id,
            ref_: ref_.clone(),
            nar_hash,
            last_modified,
            tarball: None,
        })
    }

    async fn resolve_git(&self, url: &str, ref_: &Option<String>) -> Result<ResolvedRev> {
        let url = url.to_owned();
        let ref_owned = ref_.clone();
        let ssh_key = self.ssh_key.clone();
        let (rev, last_modified, tmp) = tokio::task::spawn_blocking(move || {
            git_checkout(&url, ref_owned.as_deref(), ssh_key.as_deref())
        })
        .await
        .context("git checkout task panicked")??;
        let nar_hash = nar_hash_of_dir(tmp.path()).await?;

        Ok(ResolvedRev {
            rev,
            ref_: ref_.clone(),
            nar_hash,
            last_modified,
            tarball: None,
        })
    }

    async fn resolve_tarball(&self, url: &str) -> Result<ResolvedRev> {
        let (link, body) = self.walk_to_tarball(url).await?;
        let pinned = PinnedTarball::parse(&link)?;
        let unpacked = tarball_response_nar_hash(body).await?;

        Ok(ResolvedRev {
            rev: pinned.rev,
            ref_: None,
            nar_hash: unpacked.nar_hash,
            last_modified: pinned.last_modified.unwrap_or(unpacked.newest_mtime),
            tarball: Some(TarballPin {
                url: pinned.url,
                rev_count: pinned.rev_count,
            }),
        })
    }

    /// FlakeHub sends its immutable `Link` on an intermediate 307, which auto-redirect would drop.
    async fn walk_to_tarball(&self, url: &str) -> Result<(String, reqwest::Response)> {
        let mut current =
            reqwest::Url::parse(url).with_context(|| format!("parsing tarball url `{url}`"))?;
        let mut link = None;
        for _ in 0..=TARBALL_MAX_REDIRECTS {
            let resp = self
                .probe
                .get(current.clone())
                .send()
                .await
                .with_context(|| format!("requesting {current}"))?;
            if let Some(found) = immutable_link_header(resp.headers()) {
                link = Some(found);
            }

            if !resp.status().is_redirection() {
                let resp = resp
                    .error_for_status()
                    .with_context(|| format!("fetching tarball `{url}`"))?;
                let link = link.with_context(|| {
                    format!(
                        "`{url}` sent no `Link: <…>; rel=\"immutable\"` header, so it has no newer version to pin"
                    )
                })?;
                return Ok((link, resp));
            }

            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .with_context(|| format!("redirect from {current} has no Location"))?
                .to_str()
                .context("non-ASCII Location header")?;
            current = current
                .join(location)
                .with_context(|| format!("resolving redirect `{location}`"))?;
        }

        bail!("`{url}` redirected more than {TARBALL_MAX_REDIRECTS} times")
    }

    fn github_get(&self, url: &str) -> reqwest::RequestBuilder {
        let mut req = self
            .client
            .get(url)
            .header("User-Agent", USER_AGENT)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(token) = &self.github_token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }

        req
    }

    fn gitlab_get(&self, url: &str) -> reqwest::RequestBuilder {
        let mut req = self.client.get(url).header("User-Agent", USER_AGENT);
        if let Some(token) = &self.gitlab_token {
            req = req.header("PRIVATE-TOKEN", token);
        }

        req
    }
}

#[async_trait]
impl RevisionResolver for HttpRevisionResolver {
    async fn resolve(&self, reference: &LockedRef) -> Result<ResolvedRev> {
        match reference {
            LockedRef::Github { owner, repo, ref_ } => self.resolve_github(owner, repo, ref_).await,
            LockedRef::Gitlab { owner, repo, ref_ } => self.resolve_gitlab(owner, repo, ref_).await,
            LockedRef::Git { url, ref_ } => self.resolve_git(url, ref_).await,
            LockedRef::Tarball { url } => self.resolve_tarball(url).await,
            LockedRef::Sourcehut { .. } => bail!("sourcehut inputs are not yet supported"),
            LockedRef::Path { .. } => bail!("path inputs cannot be bumped"),
            LockedRef::Indirect { .. } => bail!("indirect inputs cannot be bumped"),
            LockedRef::Other(ty) => bail!("unsupported flake input type `{ty}`"),
        }
    }
}

fn git_checkout(
    url: &str,
    ref_: Option<&str>,
    ssh_key: Option<&str>,
) -> Result<(String, i64, tempfile::TempDir)> {
    let tmp = tempfile::tempdir().context("creating git temp dir")?;
    let repo = git2::build::RepoBuilder::new()
        .fetch_options(crate::fetch_options_with_ssh(ssh_key))
        .clone(url, tmp.path())
        .with_context(|| format!("cloning {url}"))?;

    let commit = match ref_ {
        Some(r) => repo
            .revparse_single(&format!("origin/{r}"))
            .or_else(|_| repo.revparse_single(r))
            .with_context(|| format!("resolving git ref `{r}`"))?
            .peel_to_commit()
            .context("ref does not point at a commit")?,
        None => repo
            .head()
            .context("reading HEAD")?
            .peel_to_commit()
            .context("HEAD commit")?,
    };

    let rev = commit.id().to_string();
    let last_modified = commit.time().seconds();
    let tree = commit.tree().context("commit tree")?;
    let mut checkout = git2::build::CheckoutBuilder::new();
    checkout.force();
    repo.checkout_tree(tree.as_object(), Some(&mut checkout))
        .context("git checkout")?;
    std::fs::remove_dir_all(tmp.path().join(".git")).ok();

    Ok((rev, last_modified, tmp))
}

fn immutable_link_header(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get_all(reqwest::header::LINK)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(immutable_link)
        .map(str::to_owned)
}

fn parse_rfc3339(s: &str) -> Result<i64> {
    Ok(chrono::DateTime::parse_from_rfc3339(s)
        .with_context(|| format!("parsing commit date `{s}`"))?
        .timestamp())
}

#[derive(Deserialize)]
struct GithubCommit {
    sha: String,
    commit: GithubCommitInner,
}

#[derive(Deserialize)]
struct GithubCommitInner {
    committer: GithubGitUser,
}

#[derive(Deserialize)]
struct GithubGitUser {
    date: String,
}

#[derive(Deserialize)]
struct GitlabCommit {
    id: String,
    committed_date: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn make_git_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("r");
        let rd = repo.to_str().unwrap();
        Command::new("git")
            .args(["init", rd, "-b", "main"])
            .output()
            .unwrap();
        std::fs::write(repo.join("f"), "x").unwrap();
        Command::new("git")
            .args(["-C", rd, "add", "."])
            .output()
            .unwrap();
        Command::new("git")
            .args([
                "-C",
                rd,
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-m",
                "i",
            ])
            .output()
            .unwrap();
        (tmp, repo)
    }

    // `git_checkout` is called directly because a reqwest client cannot be built in the sandboxed
    // CI without system CA certificates.
    #[test]
    fn git_checkout_clones_file_url_via_repobuilder() {
        let (_tmp, repo) = make_git_repo();
        let url = format!("file://{}", repo.display());
        let out = git_checkout(&url, None, None);
        assert!(out.is_ok(), "file:// clone failed: {:?}", out.err());
    }

    #[test]
    fn git_checkout_with_ssh_key_still_clones_file_url() {
        let (_tmp, repo) = make_git_repo();
        let url = format!("file://{}", repo.display());
        let out = git_checkout(&url, None, Some("-----BEGIN OPENSSH PRIVATE KEY-----\n"));
        assert!(
            out.is_ok(),
            "file:// clone with an ssh key failed: {:?}",
            out.err()
        );
    }

    use super::super::narhash::{tarball_bytes_nar_hash, test_tarball::targz};

    const SOURCE: &[(&str, &[u8], u64)] = &[("source/flake.nix", b"{ }", 1234)];

    fn serve(respond: impl Fn(&str, &str) -> Vec<u8> + Send + 'static) -> String {
        use std::io::{BufRead as _, BufReader, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("local addr"));
        let base_in_thread = base.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                let mut request_line = String::new();
                let _ = reader.read_line(&mut request_line);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) <= 2 {
                        break;
                    }
                }
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_owned();
                let _ = stream.write_all(&respond(&path, &base_in_thread));
            }
        });

        base
    }

    fn redirect(status: &str, location: &str, link: Option<&str>) -> Vec<u8> {
        let link = link
            .map(|l| format!("link: <{l}>; rel=\"immutable\"\r\n"))
            .unwrap_or_default();
        format!(
            "HTTP/1.1 {status}\r\nlocation: {location}\r\n{link}content-length: 0\r\nconnection: close\r\n\r\n"
        )
        .into_bytes()
    }

    fn ok(body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    fn not_found() -> Vec<u8> {
        b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec()
    }

    fn resolver() -> HttpRevisionResolver {
        HttpRevisionResolver::new(gradient_util::http::download_client().clone())
    }

    #[tokio::test]
    async fn flakehub_style_link_on_an_intermediate_redirect_pins_the_input() {
        let body = targz(SOURCE);
        let served = body.clone();
        let base = serve(move |path, base| match path {
            "/f/o/r/0.1" => redirect(
                "307 Temporary Redirect",
                "/f/pinned/o/r/0.1.20+rev-2222/u/source.tar.gz",
                None,
            ),
            "/f/pinned/o/r/0.1.20+rev-2222/u/source.tar.gz" => redirect(
                "307 Temporary Redirect",
                "/s3/object",
                Some(&format!(
                    "{base}/f/pinned/o/r/0.1.20+rev-2222/u/source.tar.gz?rev=2222&revCount=20"
                )),
            ),
            "/s3/object" => ok(&served),
            _ => not_found(),
        });

        let got = resolver()
            .resolve(&LockedRef::Tarball {
                url: format!("{base}/f/o/r/0.1"),
            })
            .await
            .unwrap();

        assert_eq!(got.rev, "2222");
        assert_eq!(got.ref_, None);
        assert_eq!(
            got.nar_hash,
            tarball_bytes_nar_hash(body).await.unwrap().nar_hash
        );
        assert_eq!(
            got.last_modified, 1234,
            "no lastModified param: newest mtime wins"
        );
        assert_eq!(
            got.tarball,
            Some(TarballPin {
                url: format!("{base}/f/pinned/o/r/0.1.20%2Brev-2222/u/source.tar.gz"),
                rev_count: Some(20),
            })
        );
    }

    #[tokio::test]
    async fn channel_style_link_last_modified_wins_over_mtime() {
        let body = targz(SOURCE);
        let base = serve(move |path, base| match path {
            "/nixos-unstable/nixexprs.tar.xz" => redirect(
                "302 Found",
                "/rel/nixexprs.tar.xz",
                Some(&format!(
                    "{base}/rel/nixexprs.tar.xz?rev=3333&lastModified=1790000000"
                )),
            ),
            "/rel/nixexprs.tar.xz" => ok(&body),
            _ => not_found(),
        });

        let got = resolver()
            .resolve(&LockedRef::Tarball {
                url: format!("{base}/nixos-unstable/nixexprs.tar.xz"),
            })
            .await
            .unwrap();

        assert_eq!(got.rev, "3333");
        assert_eq!(got.last_modified, 1790000000);
        assert_eq!(
            got.tarball,
            Some(TarballPin {
                url: format!("{base}/rel/nixexprs.tar.xz"),
                rev_count: None,
            })
        );
    }

    #[tokio::test]
    async fn tarball_without_an_immutable_link_is_rejected() {
        let body = targz(SOURCE);
        let base = serve(move |_, _| ok(&body));

        let err = resolver()
            .resolve(&LockedRef::Tarball {
                url: format!("{base}/plain.tar.gz"),
            })
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("no newer version to pin"),
            "unexpected error: {err:#}"
        );
    }

    #[tokio::test]
    async fn failing_final_response_reports_the_http_error() {
        let base = serve(|_, _| not_found());

        let err = resolver()
            .resolve(&LockedRef::Tarball {
                url: format!("{base}/missing.tar.gz"),
            })
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("404"),
            "unexpected error: {err:#}"
        );
    }

    #[tokio::test]
    #[ignore = "network: compares against a lock written by nix in ~/src/all-the-nix/flake.lock"]
    async fn matches_nix_for_a_flakehub_pinned_tarball() {
        let url = "https://api.flakehub.com/f/pinned/NixOS/nixpkgs/0.2605.1012930%2Brev-c5c4a43b0e8056328ec4529f735cabdb8f1942bb/01a053c0-7061-7382-b001-102e9148a5b5/source.tar.gz";

        let got = resolver()
            .resolve(&LockedRef::Tarball { url: url.into() })
            .await
            .unwrap();

        assert_eq!(got.rev, "c5c4a43b0e8056328ec4529f735cabdb8f1942bb");
        assert_eq!(
            got.nar_hash,
            "sha256-u6z9VTZA4Kf3RkHQo9sQI7NI4Ei/uiU9vrMqOiwWP1Y="
        );
        assert_eq!(got.last_modified, 1787962033);
        assert_eq!(
            got.tarball,
            Some(TarballPin {
                url: url.into(),
                rev_count: Some(1012930),
            })
        );
    }
}
