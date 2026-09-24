use async_trait::async_trait;
use chrono::{DateTime, Utc};
use gh_housekeeper_core::{
    Account, ActionsCache, Artifact, ArtifactProvider, CacheProvider, CachePurgeProvider,
    DeleteOutcome, ProviderError, ProviderResult, ProviderTelemetry, Repository, RepositoryRef,
    ScanScope, Visibility, WorkflowRun, WorkflowRunProvider, WorkflowRunPurgeProvider,
    WorkflowRunRef,
};
use reqwest::{Method, Response, StatusCode, header::HeaderMap};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::{
    env, fmt,
    process::Command,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::time::sleep;

const GITHUB_API_VERSION: &str = "2026-03-10";
const DEFAULT_API_URL: &str = "https://api.github.com";
const PER_PAGE: usize = 100;
const RATE_UNKNOWN: u64 = u64::MAX;
const MUTATION_PACING: Duration = Duration::from_secs(2);

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
    last_mutation_slot: Mutex<Option<Instant>>,
}

impl GithubClient {
    pub fn normalized_api_url(base_url: Option<&str>) -> ProviderResult<String> {
        let url = reqwest::Url::parse(base_url.unwrap_or(DEFAULT_API_URL))
            .map_err(|error| ProviderError::InvalidResponse(format!("invalid API URL: {error}")))?;
        Ok(url.to_string().trim_end_matches('/').to_owned())
    }

    pub fn from_environment() -> ProviderResult<Self> {
        Self::new(SecretToken::discover()?)
    }

    pub fn new(token: SecretToken) -> ProviderResult<Self> {
        Self::with_base_url(token, DEFAULT_API_URL)
    }

    pub fn with_base_url(token: SecretToken, base_url: impl Into<String>) -> ProviderResult<Self> {
        let input = base_url.into();
        let base_url = Self::normalized_api_url(Some(&input))?;
        let http = reqwest::Client::builder()
            .user_agent(concat!("gh-housekeeper/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| ProviderError::Transport(error.to_string()))?;

        Ok(Self {
            http,
            token,
            base_url,
            api_requests: AtomicU64::new(0),
            rate_limit_remaining: AtomicU64::new(RATE_UNKNOWN),
            last_mutation_slot: Mutex::new(None),
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

    fn reserve_mutation_delay(&self, method: &Method) -> Duration {
        if method != Method::POST
            && method != Method::PATCH
            && method != Method::PUT
            && method != Method::DELETE
        {
            return Duration::ZERO;
        }

        let now = Instant::now();
        let mut slot = self
            .last_mutation_slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let reserved_at = slot
            .map(|last| last + MUTATION_PACING)
            .filter(|candidate| *candidate > now)
            .unwrap_or(now);
        *slot = Some(reserved_at);
        reserved_at.saturating_duration_since(now)
    }

    async fn send_with_policy(
        &self,
        method: Method,
        path: &str,
        allow_not_found: bool,
    ) -> ProviderResult<Option<Response>> {
        let max_attempts = if method == Method::GET { 4 } else { 1 };

        for attempt in 0..max_attempts {
            let mutation_delay = self.reserve_mutation_delay(&method);
            if !mutation_delay.is_zero() {
                sleep(mutation_delay).await;
            }

            self.api_requests.fetch_add(1, Ordering::Relaxed);
            let response = self.request(method.clone(), path).send().await;

            let response = match response {
                Ok(response) => response,
                Err(_) if method == Method::GET && attempt + 1 < max_attempts => {
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

#[async_trait]
impl CacheProvider for GithubClient {
    async fn caches(&self, repository: &Repository) -> ProviderResult<Vec<ActionsCache>> {
        let repository_ref = RepositoryRef::from(repository);
        let mut caches = Vec::new();
        let mut page = 1usize;

        loop {
            let path = format!(
                "/repos/{}/actions/caches?per_page={PER_PAGE}&page={page}",
                repository.full_name
            );
            let response: GithubCachePage = self.get_json(&path).await?;
            let item_count = response.actions_caches.len();
            caches.extend(
                response
                    .actions_caches
                    .into_iter()
                    .map(|cache| cache.into_domain(repository_ref.clone())),
            );

            if item_count == 0 || caches.len() as u64 >= response.total_count {
                break;
            }
            page += 1;
        }

        Ok(caches)
    }

    async fn cache(
        &self,
        repository: &RepositoryRef,
        cache_id: u64,
    ) -> ProviderResult<Option<ActionsCache>> {
        validate_full_name(&repository.full_name)?;
        let mut page = 1usize;
        let mut inspected = 0_u64;

        loop {
            let path = format!(
                "/repos/{}/actions/caches?per_page={PER_PAGE}&page={page}",
                repository.full_name
            );
            let response: GithubCachePage = self.get_json(&path).await?;
            let item_count = response.actions_caches.len();
            let total_count = response.total_count;
            inspected = inspected.saturating_add(item_count as u64);

            if let Some(cache) = response
                .actions_caches
                .into_iter()
                .find(|cache| cache.id == cache_id)
            {
                return Ok(Some(cache.into_domain(repository.clone())));
            }

            if item_count == 0 || inspected >= total_count {
                return Ok(None);
            }
            page += 1;
        }
    }
}

#[async_trait]
impl CachePurgeProvider for GithubClient {
    async fn delete_cache(
        &self,
        repository: &RepositoryRef,
        cache_id: u64,
    ) -> ProviderResult<DeleteOutcome> {
        validate_full_name(&repository.full_name)?;
        let path = format!("/repos/{}/actions/caches/{cache_id}", repository.full_name);
        match self.send_with_policy(Method::DELETE, &path, true).await? {
            Some(_) => Ok(DeleteOutcome::Deleted),
            None => Ok(DeleteOutcome::AlreadyAbsent),
        }
    }
}

#[async_trait]
impl WorkflowRunProvider for GithubClient {
    fn provider_instance(&self) -> String {
        self.base_url.clone()
    }

    async fn workflow_runs(&self, repository: &Repository) -> ProviderResult<Vec<WorkflowRun>> {
        validate_full_name(&repository.full_name)?;
        let repository_ref = RepositoryRef::from(repository);
        let mut runs = Vec::new();
        let mut page = 1usize;

        loop {
            let path = format!(
                "/repos/{}/actions/runs?per_page={PER_PAGE}&page={page}",
                repository.full_name
            );
            let response: GithubWorkflowRunPage = self.get_json(&path).await?;
            let item_count = response.workflow_runs.len();
            runs.extend(
                response
                    .workflow_runs
                    .into_iter()
                    .map(|run| run.into_domain(repository_ref.clone())),
            );

            if item_count == 0 || runs.len() as u64 >= response.total_count {
                break;
            }
            page += 1;
        }

        Ok(runs)
    }

    async fn workflow_run(
        &self,
        repository: &RepositoryRef,
        run_id: u64,
    ) -> ProviderResult<Option<WorkflowRun>> {
        validate_full_name(&repository.full_name)?;
        let path = format!("/repos/{}/actions/runs/{run_id}", repository.full_name);
        let Some(run) = self.get_optional_json::<GithubWorkflowRun>(&path).await? else {
            return Ok(None);
        };
        if let Some(remote) = run.repository.as_ref() {
            if remote.id != repository.id || remote.full_name != repository.full_name {
                return Err(ProviderError::InvalidResponse(format!(
                    "workflow run {run_id} belongs to {} (repository ID {}), not {} (ID {})",
                    remote.full_name, remote.id, repository.full_name, repository.id
                )));
            }
        } else {
            // Older provider responses may omit the nested repository. Confirm the
            // canonical repository identity instead of trusting the caller's ref.
            let remote = self.get_repository(&repository.full_name).await?;
            if remote.id != repository.id || remote.full_name != repository.full_name {
                return Err(ProviderError::InvalidResponse(format!(
                    "workflow run {run_id} repository lookup changed identity"
                )));
            }
        }
        Ok(Some(run.into_domain(repository.clone())))
    }

    async fn workflow_run_artifacts(
        &self,
        repository: &RepositoryRef,
        run_id: u64,
    ) -> ProviderResult<Vec<Artifact>> {
        validate_full_name(&repository.full_name)?;
        let mut artifacts = Vec::new();
        let mut page = 1usize;

        loop {
            let path = format!(
                "/repos/{}/actions/runs/{run_id}/artifacts?per_page={PER_PAGE}&page={page}",
                repository.full_name
            );
            let response: GithubArtifactPage = self.get_json(&path).await?;
            let item_count = response.artifacts.len();
            artifacts.extend(
                response
                    .artifacts
                    .into_iter()
                    .map(|artifact| artifact.into_domain(repository.clone())),
            );

            if item_count == 0 || artifacts.len() as u64 >= response.total_count {
                break;
            }
            page += 1;
        }

        Ok(artifacts)
    }
}

#[async_trait]
impl WorkflowRunPurgeProvider for GithubClient {
    async fn workflow_run_artifact(
        &self,
        repository: &RepositoryRef,
        artifact_id: u64,
    ) -> ProviderResult<Option<Artifact>> {
        ArtifactProvider::artifact(self, repository, artifact_id).await
    }

    async fn delete_workflow_run_artifact(
        &self,
        repository: &RepositoryRef,
        artifact_id: u64,
    ) -> ProviderResult<DeleteOutcome> {
        ArtifactProvider::delete_artifact(self, repository, artifact_id).await
    }

    async fn delete_workflow_run_logs(
        &self,
        repository: &RepositoryRef,
        run_id: u64,
    ) -> ProviderResult<DeleteOutcome> {
        validate_full_name(&repository.full_name)?;
        let path = format!("/repos/{}/actions/runs/{run_id}/logs", repository.full_name);
        match self.send_with_policy(Method::DELETE, &path, true).await? {
            Some(_) => Ok(DeleteOutcome::Deleted),
            None => Ok(DeleteOutcome::AlreadyAbsent),
        }
    }

    async fn delete_workflow_run(
        &self,
        repository: &RepositoryRef,
        run_id: u64,
    ) -> ProviderResult<DeleteOutcome> {
        validate_full_name(&repository.full_name)?;
        let path = format!("/repos/{}/actions/runs/{run_id}", repository.full_name);
        match self.send_with_policy(Method::DELETE, &path, true).await? {
            Some(_) => Ok(DeleteOutcome::Deleted),
            None => Ok(DeleteOutcome::AlreadyAbsent),
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
        || (status == StatusCode::FORBIDDEN && message.to_ascii_lowercase().contains("rate limit"))
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
struct GithubWorkflowRunPage {
    total_count: u64,
    workflow_runs: Vec<GithubWorkflowRun>,
}

#[derive(Deserialize)]
struct GithubWorkflowRun {
    id: u64,
    name: Option<String>,
    display_title: String,
    event: String,
    status: String,
    conclusion: Option<String>,
    workflow_id: u64,
    head_branch: Option<String>,
    head_sha: String,
    run_number: u64,
    run_attempt: u64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    repository: Option<GithubRunRepository>,
}

#[derive(Deserialize)]
struct GithubRunRepository {
    id: u64,
    full_name: String,
}

impl GithubWorkflowRun {
    fn into_domain(self, repository: RepositoryRef) -> WorkflowRun {
        WorkflowRun {
            id: self.id,
            repository,
            workflow_id: self.workflow_id,
            workflow_name: self.name,
            display_title: self.display_title,
            event: self.event,
            status: self.status,
            conclusion: self.conclusion,
            head_branch: self.head_branch,
            head_sha: self.head_sha,
            run_number: self.run_number,
            run_attempt: self.run_attempt,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

#[derive(Deserialize)]
struct GithubCachePage {
    total_count: u64,
    actions_caches: Vec<GithubCache>,
}

#[derive(Deserialize)]
struct GithubCache {
    id: u64,
    key: String,
    version: String,
    #[serde(rename = "ref")]
    git_ref: String,
    created_at: DateTime<Utc>,
    last_accessed_at: DateTime<Utc>,
    size_in_bytes: u64,
}

impl GithubCache {
    fn into_domain(self, repository: RepositoryRef) -> ActionsCache {
        ActionsCache {
            id: self.id,
            repository,
            key: self.key,
            version: self.version,
            git_ref: self.git_ref,
            created_at: self.created_at,
            last_accessed_at: self.last_accessed_at,
            size_in_bytes: self.size_in_bytes,
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
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::mpsc,
        thread,
    };

    fn spawn_http_fixture(
        bodies: Vec<String>,
    ) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            for body in bodies {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 4096];
                let read = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..read]);
                sender
                    .send(request.lines().next().unwrap_or_default().to_owned())
                    .unwrap();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
        });
        (format!("http://{address}"), receiver, handle)
    }

    #[tokio::test]
    async fn lists_caches_with_pagination_and_maps_metadata() {
        let page_one = r#"{"total_count":2,"actions_caches":[{"id":505,"ref":"refs/heads/main","key":"linux-build","version":"version-a","last_accessed_at":"2026-09-10T12:00:00Z","created_at":"2026-09-01T12:00:00Z","size_in_bytes":1024}]}"#.to_owned();
        let page_two = r#"{"total_count":2,"actions_caches":[{"id":506,"ref":"refs/pull/42/merge","key":"linux-test","version":"version-b","last_accessed_at":"2026-09-11T12:00:00Z","created_at":"2026-09-02T12:00:00Z","size_in_bytes":2048}]}"#.to_owned();
        let (base_url, requests, server) = spawn_http_fixture(vec![page_one, page_two]);
        let client =
            GithubClient::with_base_url(SecretToken("fictional-token".to_owned()), base_url)
                .unwrap();
        let repository = Repository {
            id: 1,
            owner: "example-user".to_owned(),
            name: "project-alpha".to_owned(),
            full_name: "example-user/project-alpha".to_owned(),
            visibility: Visibility::Public,
            default_branch: "main".to_owned(),
            archived: false,
            fork: false,
        };

        let caches = CacheProvider::caches(&client, &repository).await.unwrap();
        server.join().unwrap();

        assert_eq!(caches.len(), 2);
        assert_eq!(caches[0].id, 505);
        assert_eq!(caches[0].key, "linux-build");
        assert_eq!(caches[0].git_ref, "refs/heads/main");
        assert_eq!(caches[0].size_in_bytes, 1024);
        assert_eq!(caches[1].version, "version-b");
        assert_eq!(client.telemetry().api_requests, 2);

        let requests = requests.try_iter().collect::<Vec<_>>();
        assert_eq!(
            requests,
            vec![
                "GET /repos/example-user/project-alpha/actions/caches?per_page=100&page=1 HTTP/1.1",
                "GET /repos/example-user/project-alpha/actions/caches?per_page=100&page=2 HTTP/1.1",
            ]
        );
    }

    #[tokio::test]
    async fn finds_one_cache_by_exact_id_without_mutation() {
        let page_one = r#"{"total_count":2,"actions_caches":[{"id":505,"ref":"refs/heads/main","key":"linux-build","version":"version-a","last_accessed_at":"2026-09-10T12:00:00Z","created_at":"2026-09-01T12:00:00Z","size_in_bytes":1024}]}"#.to_owned();
        let page_two = r#"{"total_count":2,"actions_caches":[{"id":506,"ref":"refs/heads/main","key":"linux-build-extra","version":"version-b","last_accessed_at":"2026-09-11T12:00:00Z","created_at":"2026-09-02T12:00:00Z","size_in_bytes":2048}]}"#.to_owned();
        let (base_url, requests, server) = spawn_http_fixture(vec![page_one, page_two]);
        let client =
            GithubClient::with_base_url(SecretToken("fictional-token".to_owned()), base_url)
                .unwrap();
        let repository = RepositoryRef {
            id: 1,
            full_name: "example-user/project-alpha".to_owned(),
        };

        let cache = CacheProvider::cache(&client, &repository, 506)
            .await
            .unwrap()
            .unwrap();
        server.join().unwrap();

        assert_eq!(cache.id, 506);
        assert_eq!(cache.key, "linux-build-extra");
        assert_eq!(cache.repository, repository);
        assert_eq!(client.telemetry().api_requests, 2);

        let requests = requests.try_iter().collect::<Vec<_>>();
        assert_eq!(
            requests,
            vec![
                "GET /repos/example-user/project-alpha/actions/caches?per_page=100&page=1 HTTP/1.1",
                "GET /repos/example-user/project-alpha/actions/caches?per_page=100&page=2 HTTP/1.1",
            ]
        );
    }

    #[tokio::test]
    async fn lists_workflow_runs_with_pagination_and_maps_metadata() {
        let page_one = r#"{"total_count":2,"workflow_runs":[{"id":7001,"name":"Rust CI","display_title":"first","event":"push","status":"completed","conclusion":"success","workflow_id":88,"head_branch":"main","head_sha":"abc","run_number":10,"run_attempt":1,"created_at":"2026-09-01T12:00:00Z","updated_at":"2026-09-01T12:05:00Z"}]}"#.to_owned();
        let page_two = r#"{"total_count":2,"workflow_runs":[{"id":7002,"name":"Rust CI","display_title":"second","event":"pull_request","status":"completed","conclusion":"failure","workflow_id":88,"head_branch":"work/example","head_sha":"def","run_number":11,"run_attempt":2,"created_at":"2026-09-02T12:00:00Z","updated_at":"2026-09-02T12:06:00Z"}]}"#.to_owned();
        let (base_url, requests, server) = spawn_http_fixture(vec![page_one, page_two]);
        let client =
            GithubClient::with_base_url(SecretToken("fictional-token".to_owned()), base_url)
                .unwrap();
        let repository = Repository {
            id: 1,
            owner: "example-user".to_owned(),
            name: "project-alpha".to_owned(),
            full_name: "example-user/project-alpha".to_owned(),
            visibility: Visibility::Public,
            default_branch: "main".to_owned(),
            archived: false,
            fork: false,
        };

        let runs = WorkflowRunProvider::workflow_runs(&client, &repository)
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].id, 7001);
        assert_eq!(runs[0].workflow_name.as_deref(), Some("Rust CI"));
        assert_eq!(runs[0].head_branch.as_deref(), Some("main"));
        assert_eq!(runs[1].run_attempt, 2);
        assert_eq!(runs[1].conclusion.as_deref(), Some("failure"));

        let requests = requests.try_iter().collect::<Vec<_>>();
        assert_eq!(
            requests,
            vec![
                "GET /repos/example-user/project-alpha/actions/runs?per_page=100&page=1 HTTP/1.1",
                "GET /repos/example-user/project-alpha/actions/runs?per_page=100&page=2 HTTP/1.1",
            ]
        );
    }

    #[tokio::test]
    async fn deletes_exact_actions_cache_id_through_cache_endpoint_only() {
        let (base_url, requests, server) = spawn_http_fixture(vec!["{}".to_owned()]);
        let client =
            GithubClient::with_base_url(SecretToken("fictional-token".to_owned()), base_url)
                .unwrap();
        let repository = RepositoryRef {
            id: 1,
            full_name: "example-user/project-alpha".to_owned(),
        };

        let outcome = CachePurgeProvider::delete_cache(&client, &repository, 4242)
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(outcome, DeleteOutcome::Deleted);
        let requests = requests.try_iter().collect::<Vec<_>>();
        assert_eq!(
            requests,
            vec!["DELETE /repos/example-user/project-alpha/actions/caches/4242 HTTP/1.1"]
        );
        assert!(
            requests
                .iter()
                .all(|request| !request.contains("/releases"))
        );
    }

    #[tokio::test]
    async fn resolves_exact_run_and_snapshots_its_artifacts_without_mutation() {
        let run = r#"{"id":7001,"name":"Rust CI","display_title":"first","event":"push","status":"completed","conclusion":"success","workflow_id":88,"head_branch":"main","head_sha":"abc","run_number":10,"run_attempt":1,"created_at":"2026-09-01T12:00:00Z","updated_at":"2026-09-01T12:05:00Z","repository":{"id":1,"full_name":"example-user/project-alpha"}}"#.to_owned();
        let artifacts = r#"{"total_count":1,"artifacts":[{"id":9001,"name":"build-output","size_in_bytes":4096,"created_at":"2026-09-01T12:04:00Z","updated_at":"2026-09-01T12:04:00Z","expires_at":"2026-12-01T12:04:00Z","expired":false,"digest":"sha256:fictional","workflow_run":{"id":7001,"head_branch":"main","head_sha":"abc"}}]}"#.to_owned();
        let (base_url, requests, server) = spawn_http_fixture(vec![run, artifacts]);
        let client =
            GithubClient::with_base_url(SecretToken("fictional-token".to_owned()), base_url)
                .unwrap();
        let repository = RepositoryRef {
            id: 1,
            full_name: "example-user/project-alpha".to_owned(),
        };

        let run = WorkflowRunProvider::workflow_run(&client, &repository, 7001)
            .await
            .unwrap()
            .unwrap();
        let artifacts = WorkflowRunProvider::workflow_run_artifacts(&client, &repository, 7001)
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(run.id, 7001);
        assert!(run.is_completed());
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].id, 9001);
        assert_eq!(
            artifacts[0].workflow_run.as_ref().map(|run| run.id),
            Some(7001)
        );

        let requests = requests.try_iter().collect::<Vec<_>>();
        assert_eq!(
            requests,
            vec![
                "GET /repos/example-user/project-alpha/actions/runs/7001 HTTP/1.1",
                "GET /repos/example-user/project-alpha/actions/runs/7001/artifacts?per_page=100&page=1 HTTP/1.1",
            ]
        );
    }

    #[tokio::test]
    async fn rejects_a_run_returned_with_another_repository_identity() {
        let run = r#"{"id":7001,"name":"Rust CI","display_title":"first","event":"push","status":"completed","conclusion":"success","workflow_id":88,"head_branch":"main","head_sha":"abc","run_number":10,"run_attempt":1,"created_at":"2026-09-01T12:00:00Z","updated_at":"2026-09-01T12:05:00Z","repository":{"id":2,"full_name":"other/project"}}"#.to_owned();
        let (base_url, requests, server) = spawn_http_fixture(vec![run]);
        let client =
            GithubClient::with_base_url(SecretToken("fictional-token".to_owned()), base_url)
                .unwrap();
        let repository = RepositoryRef {
            id: 1,
            full_name: "example-user/project-alpha".to_owned(),
        };

        let result = WorkflowRunProvider::workflow_run(&client, &repository, 7001).await;
        server.join().unwrap();
        assert!(matches!(result, Err(ProviderError::InvalidResponse(_))));
        assert_eq!(requests.try_iter().count(), 1);
    }

    #[tokio::test]
    async fn deletes_run_logs_before_the_run_through_distinct_endpoints() {
        let (base_url, requests, server) =
            spawn_http_fixture(vec!["{}".to_owned(), "{}".to_owned()]);
        let client =
            GithubClient::with_base_url(SecretToken("fictional-token".to_owned()), base_url)
                .unwrap();
        let repository = RepositoryRef {
            id: 1,
            full_name: "example-user/project-alpha".to_owned(),
        };

        let logs = WorkflowRunPurgeProvider::delete_workflow_run_logs(&client, &repository, 7001)
            .await
            .unwrap();
        let run = WorkflowRunPurgeProvider::delete_workflow_run(&client, &repository, 7001)
            .await
            .unwrap();
        server.join().unwrap();

        assert_eq!(logs, DeleteOutcome::Deleted);
        assert_eq!(run, DeleteOutcome::Deleted);

        let requests = requests.try_iter().collect::<Vec<_>>();
        assert_eq!(
            requests,
            vec![
                "DELETE /repos/example-user/project-alpha/actions/runs/7001/logs HTTP/1.1",
                "DELETE /repos/example-user/project-alpha/actions/runs/7001 HTTP/1.1",
            ]
        );
    }

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
    fn mutation_slots_are_paced_without_delaying_reads() {
        let client = GithubClient::with_base_url(
            SecretToken("fictional-token".to_owned()),
            "http://127.0.0.1:9",
        )
        .unwrap();

        assert_eq!(client.reserve_mutation_delay(&Method::GET), Duration::ZERO);
        assert_eq!(
            client.reserve_mutation_delay(&Method::DELETE),
            Duration::ZERO
        );

        let second_delete = client.reserve_mutation_delay(&Method::DELETE);
        assert!(
            second_delete >= Duration::from_millis(1900),
            "second mutation should be reserved about two seconds later, got {second_delete:?}"
        );
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
        assert!(is_rate_limited(StatusCode::TOO_MANY_REQUESTS, &headers, ""));
    }
}
