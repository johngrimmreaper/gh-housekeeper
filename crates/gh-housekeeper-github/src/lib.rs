use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gh_housekeeper_core::{
    Account, Artifact, ArtifactProvider, DeleteOutcome, ProviderError, ProviderResult,
    ProviderTelemetry, Repository, RepositoryRef, ScanScope, Visibility, WorkflowRunRef,
};
use reqwest::{Method, Response, StatusCode, header::HeaderMap};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::{
    env, fmt,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::sleep;

const GITHUB_API_VERSION: &str = "2026-03-10";
const DEFAULT_API_URL: &str = "https://api.github.com";
const PER_PAGE: usize = 100;
const RATE_UNKNOWN: u64 = u64::MAX;

pub struct SecretToken(String);

impl SecretToken {
    pub fn discover() -> ProviderResult<Self> {
        if let Ok(output) = Command::new("gh").args(["auth", "token"]).output() {
            if output.status.success() {
                if let Ok(token) = String::from_utf8(output.stdout) {
                    let token = token.trim();
                    if !token.is_empty() {
                        return Ok(Self(token.to_owned()));
                    }
                }
            }
        }

        if let Ok(token) = env::var("GITHUB_TOKEN") {
            let token = token.trim();
            if !token.is_empty() {
                return Ok(Self(token.to_owned()));
            }
        }

        Err(ProviderError::Authentication(
            "no GitHub credential found; authenticate with GitHub CLI or set GITHUB_TOKEN"
                .to_owned(),
        ))
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretToken(<redacted>)")
    }
}

pub struct GithubClient {
    http: reqwest::Client,
    token: SecretToken,
    base_url: String,
    api_requests: AtomicU64,
    rate_limit_remaining: AtomicU64,
}

impl GithubClient {
    pub fn from_environment() -> ProviderResult<Self> {
        Self::new(SecretToken::discover()?)
    }

    pub fn new(token: SecretToken) -> ProviderResult<Self> {
        Self::with_base_url(token, DEFAULT_API_URL)
    }

    pub fn with_base_url(token: SecretToken, base_url: impl Into<String>) -> ProviderResult<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("gh-housekeeper/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| ProviderError::Transport(error.to_string()))?;

        Ok(Self {
            http,
            token,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            api_requests: AtomicU64::new(0),
            rate_limit_remaining: AtomicU64::new(RATE_UNKNOWN),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, self.url(path))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
            .bearer_auth(self.token.expose())
    }

    async fn get_json<T>(&self, path: &str) -> ProviderResult<T>
    where
        T: DeserializeOwned,
    {
        let response = self
            .send_with_policy(Method::GET, path, false)
            .await?
            .expect("successful GET must return a response");
        response
            .json::<T>()
            .await
            .map_err(|error| ProviderError::InvalidResponse(error.to_string()))
    }

    async fn get_optional_json<T>(&self, path: &str) -> ProviderResult<Option<T>>
    where
        T: DeserializeOwned,
    {
        let response = self.send_with_policy(Method::GET, path, true).await?;
        match response {
            Some(response) => response
                .json::<T>()
                .await
                .map(Some)
                .map_err(|error| ProviderError::InvalidResponse(error.to_string())),
            None => Ok(None),
        }
    }

    async fn send_with_policy(
        &self,
        method: Method,
        path: &str,
        allow_not_found: bool,
    ) -> ProviderResult<Option<Response>> {
        let max_attempts = if method == Method::GET { 4 } else { 1 };

        for attempt in 0..max_attempts {
            self.api_requests.fetch_add(1, Ordering::Relaxed);
            let response = self.request(method.clone(), path).send().await;

            let response = match response {
                Ok(response) => response,
                Err(error) if method == Method::GET && attempt + 1 < max_attempts => {
                    sleep(Duration::from_secs(1_u64 << attempt)).await;
                    continue;
                }
                Err(error) => return Err(ProviderError::Transport(error.to_string())),
            };

            self.capture_rate_limit(response.headers());
            let status = response.status();

            if status.is_success() {
                return Ok(Some(response));
            }
            if status == StatusCode::NOT_FOUND && allow_not_found {
                return Ok(None);
            }

            let headers = response.headers().clone();
            let body = response.text().await.unwrap_or_default();
            let message = github_error_message(&body);

            if status == StatusCode::UNAUTHORIZED {
                return Err(ProviderError::Authentication(message));
            }
            if status == StatusCode::NOT_FOUND {
                return Err(ProviderError::NotFound(message));
            }
            if is_rate_limited(status, &headers, &message) {
                let retry_after_seconds = rate_limit_delay(&headers, attempt);
                if method == Method::GET && attempt + 1 < max_attempts {
                    if let Some(delay) = retry_after_seconds.filter(|delay| *delay <= 60) {
                        sleep(Duration::from_secs(delay.max(1))).await;
                        continue;
                    }
                }
                return Err(ProviderError::RateLimited {
                    retry_after_seconds,
                    message: if message.is_empty() {
                        String::new()
                    } else {
                        format!(": {message}")
                    },
                });
            }
            if status.is_server_error() && method == Method::GET && attempt + 1 < max_attempts {
                sleep(Duration::from_secs(1_u64 << attempt)).await;
                continue;
            }
            if status == StatusCode::FORBIDDEN {
                return Err(ProviderError::PermissionDenied(message));
            }

            return Err(ProviderError::HttpStatus {
                status: status.as_u16(),
                message,
            });
        }

        Err(ProviderError::Transport(
            "request retry budget exhausted".to_owned(),
        ))
    }

    fn capture_rate_limit(&self, headers: &HeaderMap) {
        if let Some(remaining) = header_u64(headers, "x-ratelimit-remaining") {
            self.rate_limit_remaining
                .store(remaining, Ordering::Relaxed);
        }
    }

    async fn list_accessible_repositories(&self) -> ProviderResult<Vec<Repository>> {
        let mut repositories = Vec::new();
        let mut page = 1usize;

        loop {
            let path = format!(
                "/user/repos?visibility=all&affiliation=owner%2Ccollaborator%2Corganization_member&sort=full_name&direction=asc&per_page={PER_PAGE}&page={page}"
            );
            let items: Vec<GithubRepository> = self.get_json(&path).await?;
            let item_count = items.len();
            repositories.extend(items.into_iter().map(Repository::from));
            if item_count < PER_PAGE {
                break;
            }
            page += 1;
        }

        Ok(repositories)
    }

    async fn get_repository(&self, full_name: &str) -> ProviderResult<Repository> {
        validate_full_name(full_name)?;
        let repository: GithubRepository = self.get_json(&format!("/repos/{full_name}")).await?;
        Ok(repository.into())
    }
}

#[async_trait]
impl ArtifactProvider for GithubClient {
    async fn account(&self) -> ProviderResult<Account> {
        let user: GithubUser = self.get_json("/user").await?;
        Ok(Account {
            provider: "github".to_owned(),
            login: user.login,
        })
    }

    async fn repositories(&self, scope: &ScanScope) -> ProviderResult<Vec<Repository>> {
        match scope {
            ScanScope::AllAccessible => self.list_accessible_repositories().await,
            ScanScope::Owner(owner) => {
                let mut repositories = self.list_accessible_repositories().await?;
                repositories.retain(|repository| repository.owner.eq_ignore_ascii_case(owner));
                Ok(repositories)
            }
            ScanScope::Repository(full_name) => Ok(vec![self.get_repository(full_name).await?]),
        }
    }

    async fn artifacts(&self, repository: &Repository) -> ProviderResult<Vec<Artifact>> {
        let repository_ref = RepositoryRef::from(repository);
        let mut artifacts = Vec::new();
        let mut page = 1usize;

        loop {
            let path = format!(
                "/repos/{}/actions/artifacts?per_page={PER_PAGE}&page={page}",
                repository.full_name
            );
            let response: GithubArtifactPage = self.get_json(&path).await?;
            let item_count = response.artifacts.len();
            artifacts.extend(
                response
                    .artifacts
                    .into_iter()
                    .map(|artifact| artifact.into_domain(repository_ref.clone())),
            );

            if item_count == 0 || artifacts.len() as u64 >= response.total_count {
                break;
            }
            page += 1;
        }

        Ok(artifacts)
    }

    async fn artifact(
        &self,
        repository: &RepositoryRef,
        artifact_id: u64,
    ) -> ProviderResult<Option<Artifact>> {
        validate_full_name(&repository.full_name)?;
        let path = format!(
            "/repos/{}/actions/artifacts/{artifact_id}",
            repository.full_name
        );
        self.get_optional_json::<GithubArtifact>(&path)
            .await
            .map(|artifact| artifact.map(|artifact| artifact.into_domain(repository.clone())))
    }

    async fn delete_artifact(
        &self,
        repository: &RepositoryRef,
        artifact_id: u64,
    ) -> ProviderResult<DeleteOutcome> {
        validate_full_name(&repository.full_name)?;
        let path = format!(
            "/repos/{}/actions/artifacts/{artifact_id}",
            repository.full_name
        );
        match self.send_with_policy(Method::DELETE, &path, true).await? {
            Some(_) => Ok(DeleteOutcome::Deleted),
            None => Ok(DeleteOutcome::AlreadyAbsent),
        }
    }

    fn telemetry(&self) -> ProviderTelemetry {
        let remaining = self.rate_limit_remaining.load(Ordering::Relaxed);
        ProviderTelemetry {
            api_requests: self.api_requests.load(Ordering::Relaxed),
            rate_limit_remaining: (remaining != RATE_UNKNOWN).then_some(remaining),
        }
    }
}

fn validate_full_name(full_name: &str) -> ProviderResult<()> {
    match full_name.split_once('/') {
        Some((owner, repository)) if !owner.is_empty() && !repository.is_empty() => Ok(()),
        _ => Err(ProviderError::InvalidResponse(format!(
            "repository must be in owner/name form: {full_name}"
        ))),
    }
}

fn header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.parse().ok()
}

fn is_rate_limited(status: StatusCode, headers: &HeaderMap, message: &str) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS
        || header_u64(headers, "x-ratelimit-remaining") == Some(0)
        || headers.contains_key("retry-after")
        || (status == StatusCode::FORBIDDEN
            && message.to_ascii_lowercase().contains("rate limit"))
}

fn rate_limit_delay(headers: &HeaderMap, attempt: usize) -> Option<u64> {
    if let Some(delay) = header_u64(headers, "retry-after") {
        return Some(delay);
    }

    if header_u64(headers, "x-ratelimit-remaining") == Some(0) {
        let reset = header_u64(headers, "x-ratelimit-reset")?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        return Some(reset.saturating_sub(now).max(1));
    }

    Some((60_u64.saturating_mul(1_u64 << attempt)).min(300))
}

fn github_error_message(body: &str) -> String {
    #[derive(Deserialize)]
    struct GithubError {
        message: String,
    }

    if let Ok(error) = serde_json::from_str::<GithubError>(body) {
        return error.message.chars().take(512).collect();
    }

    body.chars().take(512).collect()
}

#[derive(Deserialize)]
struct GithubUser {
    login: String,
}

#[derive(Deserialize)]
struct GithubOwner {
    login: String,
}

#[derive(Deserialize)]
struct GithubRepository {
    id: u64,
    owner: GithubOwner,
    name: String,
    full_name: String,
    private: bool,
    visibility: Option<String>,
    default_branch: String,
    archived: bool,
    fork: bool,
}

impl From<GithubRepository> for Repository {
    fn from(value: GithubRepository) -> Self {
        let visibility = match value.visibility.as_deref() {
            Some("public") => Visibility::Public,
            Some("private") => Visibility::Private,
            Some("internal") => Visibility::Internal,
            _ if value.private => Visibility::Private,
            _ => Visibility::Unknown,
        };

        Self {
            id: value.id,
            owner: value.owner.login,
            name: value.name,
            full_name: value.full_name,
            visibility,
            default_branch: value.default_branch,
            archived: value.archived,
            fork: value.fork,
        }
    }
}

#[derive(Deserialize)]
struct GithubArtifactPage {
    total_count: u64,
    artifacts: Vec<GithubArtifact>,
}

#[derive(Deserialize)]
struct GithubArtifact {
    id: u64,
    name: String,
    size_in_bytes: u64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    expires_at: Option<DateTime<Utc>>,
    expired: bool,
    digest: Option<String>,
    workflow_run: Option<GithubWorkflowRunRef>,
}

impl GithubArtifact {
    fn into_domain(self, repository: RepositoryRef) -> Artifact {
        Artifact {
            id: self.id,
            repository,
            name: self.name,
            size_in_bytes: self.size_in_bytes,
            created_at: self.created_at,
            updated_at: self.updated_at,
            expires_at: self.expires_at,
            expired: self.expired,
            digest: self.digest,
            workflow_run: self.workflow_run.map(WorkflowRunRef::from),
        }
    }
}

#[derive(Deserialize)]
struct GithubWorkflowRunRef {
    id: u64,
    head_branch: Option<String>,
    head_sha: Option<String>,
}

impl From<GithubWorkflowRunRef> for WorkflowRunRef {
    fn from(value: GithubWorkflowRunRef) -> Self {
        Self {
            id: value.id,
            head_branch: value.head_branch,
            head_sha: value.head_sha,
            workflow_id: None,
            workflow_name: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_debug_output_is_redacted() {
        let token = SecretToken("fictional-secret-token".to_owned());
        let debug = format!("{token:?}");
        assert_eq!(debug, "SecretToken(<redacted>)");
        assert!(!debug.contains("fictional-secret-token"));
    }

    #[test]
    fn validates_repository_full_names_without_repo_specific_knowledge() {
        assert!(validate_full_name("example-user/project-alpha").is_ok());
        assert!(validate_full_name("project-alpha").is_err());
    }

    #[test]
    fn extracts_github_error_message_without_echoing_arbitrary_json() {
        let message =
            github_error_message(r#"{"message":"rate limit exceeded","extra":"ignored"}"#);
        assert_eq!(message, "rate limit exceeded");
    }

    #[test]
    fn distinguishes_plain_forbidden_from_rate_limit_responses() {
        let headers = HeaderMap::new();
        assert!(!is_rate_limited(
            StatusCode::FORBIDDEN,
            &headers,
            "Resource not accessible by integration"
        ));
        assert!(is_rate_limited(
            StatusCode::FORBIDDEN,
            &headers,
            "You have exceeded a secondary rate limit"
        ));
        assert!(is_rate_limited(
            StatusCode::TOO_MANY_REQUESTS,
            &headers,
            ""
        ));
    }
}
