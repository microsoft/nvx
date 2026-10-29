//! The releases of a GitHub repository, from the GitHub REST API, which `curl` reaches.
//!
//! `curl` reads its options, the token among them, from its standard input, so that no command
//! line shows the token, and reads no `.curlrc`. It speaks HTTPS alone and follows no redirect
//! itself: [`GitHub`] follows each, up to [`MAX_REDIRECTS`], and only the requests to the origin
//! of the first carry the token, as with the `credential_safe_opener` of `nvx.py`, so the host
//! that serves a package never receives it. `curl` retries a request that fails transiently, such
//! as one that GitHub answers with HTTP 503, and abandons one that stalls. [`GitHub`] reads the
//! status and the redirect of each response from the headers that `curl` dumps.

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use super::{Asset, Releases, remove_stale, system_program};
use crate::ErrorCode;
use crate::host::cli::{Exit, Result, check_interrupt, io_error};
use crate::host::supervisor::spawn_command;

/// The base URL of the GitHub REST API.
const API: &str = "https://api.github.com";
/// The version of the REST API that requests ask for.
const API_VERSION: &str = "2022-11-28";
/// The media type of the JSON documents of the REST API.
const JSON: &str = "application/vnd.github+json";
/// The media type with which the REST API serves the content of a release asset.
const BINARY: &str = "application/octet-stream";
/// The user agent of the requests.
const USER_AGENT: &str = "nvx-release-downloader";
/// The most redirects that a request follows.
const MAX_REDIRECTS: usize = 10;
/// How many times `curl` retries a request that failed transiently.
const RETRIES: u32 = 3;
/// The seconds that `curl` waits to connect.
const CONNECT_TIMEOUT_SECONDS: u32 = 30;
/// The seconds that a transfer may stall, below a byte per second, before `curl` abandons it.
const STALL_SECONDS: u32 = 60;
/// The file of the scratch directory that receives the body of each response to a query.
const RESPONSE_NAME: &str = "response";
/// The file of the scratch directory that receives the headers of each response.
const HEADERS_NAME: &str = "response.headers";

/// How a query of the REST API fails.
#[derive(Debug)]
pub(super) enum Failure {
    /// GitHub answered with this HTTP error status, which the error reports.
    Http(u16, Exit),
    /// Anything else, such as a request that never reached GitHub.
    Other(Exit),
}

impl From<Exit> for Failure {
    fn from(error: Exit) -> Self {
        Self::Other(error)
    }
}

impl From<Failure> for Exit {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::Http(_, error) | Failure::Other(error) => error,
        }
    }
}

/// The releases of one GitHub repository.
pub(super) struct GitHub {
    /// The repository, `OWNER/REPOSITORY`.
    repository: String,
    /// The REST API URL of the repository.
    url: String,
    /// The token that requests to the origin of the REST API carry, if any.
    token: Option<String>,
    /// The directory in which the responses to queries land.
    scratch: PathBuf,
    /// The `curl` that requests run.
    curl: Curl,
}

impl GitHub {
    /// Returns the releases of `repository`, `OWNER/REPOSITORY`, which requests query with
    /// `token`, if any, and whose responses land in `scratch`.
    pub(super) fn new(repository: &str, token: Option<String>, scratch: &Path) -> Result<Self> {
        Self::at(API, repository, token, scratch, Curl::new())
    }

    /// Returns the releases of `repository` from the REST API at `api`, which `curl` requests.
    fn at(
        api: &str,
        repository: &str,
        token: Option<String>,
        scratch: &Path,
        curl: Curl,
    ) -> Result<Self> {
        let (owner, name) = repository_parts(repository).ok_or_else(|| {
            Exit::error(
                ErrorCode::Unsupported,
                "GitHub repository must be OWNER/REPOSITORY",
            )
        })?;
        Ok(Self {
            repository: repository.to_owned(),
            url: format!(
                "{api}/repos/{}/{}",
                percent_encoded(owner),
                percent_encoded(name)
            ),
            token,
            scratch: scratch.to_path_buf(),
            curl,
        })
    }

    /// Downloads the package of `asset` to `destination`, and checks its size. `curl` shows its
    /// progress bar on standard error if that is a terminal.
    pub(super) fn download(&self, asset: &Asset, destination: &Path) -> Result {
        let progress = io::stderr().is_terminal();
        let context = format!("the download of {}", asset.name);
        let response = self.get(&asset.url, BINARY, destination, progress, &context)?;
        if !(200..300).contains(&response.status) {
            let body = fs::read(destination).unwrap_or_default();
            return Err(Exit::error(
                ErrorCode::Unavailable,
                format!(
                    "failed to download {}: HTTP {}{}",
                    asset.name,
                    response.status,
                    detail(&error_message(&body))
                ),
            ));
        }
        let size = match fs::metadata(destination) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
            Err(error) => return Err(io_error(ErrorCode::Io, &error, destination)),
        };
        if size != asset.size {
            return Err(Exit::error(
                ErrorCode::Unavailable,
                format!(
                    "downloaded {} is {size} bytes, expected {}",
                    asset.name, asset.size
                ),
            ));
        }
        Ok(())
    }

    /// Returns the JSON document at `url`, or `None` for a missing one if `missing_ok`, as
    /// `_github_json` of `nvx.py` did.
    fn json(&self, url: &str, missing_ok: bool) -> std::result::Result<Option<Value>, Failure> {
        let body = self.scratch.join(RESPONSE_NAME);
        let response = self.get(url, JSON, &body, false, "GitHub release query")?;
        let bytes = match fs::read(&body) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(io_error(ErrorCode::Io, &error, &body).into()),
        };
        match response.status {
            200..=299 => serde_json::from_slice(&bytes).map(Some).map_err(|_| {
                Exit::error(
                    ErrorCode::Unavailable,
                    "GitHub release query returned an invalid response",
                )
                .into()
            }),
            404 if missing_ok => Ok(None),
            status => {
                let advice = match hint(status, response.rate_limited(), self.token.is_some()) {
                    "" => String::new(),
                    hint => format!("; {hint}"),
                };
                Err(Failure::Http(
                    status,
                    Exit::error(
                        ErrorCode::Unavailable,
                        format!(
                            "GitHub release query failed with HTTP {status}{}{advice}",
                            detail(&error_message(&bytes))
                        ),
                    ),
                ))
            }
        }
    }

    /// Requests `url`, accepting `accept`, with the body of the response in `output`, follows
    /// its redirects, and returns the last response. Only the requests to the origin of `url`
    /// carry the token. `progress` shows the progress bar of `curl` for the requests that a
    /// redirect led to, such as that of a package that GitHub serves from another host. A failure
    /// names `context`.
    fn get(
        &self,
        url: &str,
        accept: &str,
        output: &Path,
        progress: bool,
        context: &str,
    ) -> Result<Response> {
        let first = origin(url);
        let headers = self.scratch.join(HEADERS_NAME);
        let mut current = url.to_owned();
        for redirects in 0..=MAX_REDIRECTS {
            if self.curl.https_only && !current.starts_with("https://") {
                return Err(Exit::error(
                    ErrorCode::Unavailable,
                    format!("{context} failed: {current} is not an HTTPS URL"),
                ));
            }
            let token = self
                .token
                .as_deref()
                .filter(|_| first.is_some() && origin(&current) == first);
            let request = Request {
                url: &current,
                accept,
                token,
                output,
                headers: &headers,
                progress: progress && redirects > 0,
            };
            let response = self.curl.request(&request, context)?;
            match response.location {
                Some(location) if (300..400).contains(&response.status) => current = location,
                _ => return Ok(response),
            }
        }
        Err(Exit::error(
            ErrorCode::Unavailable,
            format!("{context} failed: it redirected more than {MAX_REDIRECTS} times"),
        ))
    }
}

impl Releases for GitHub {
    fn repository(&self) -> &str {
        &self.repository
    }

    fn release(&self, tag: &str) -> std::result::Result<Option<Value>, Failure> {
        self.json(
            &format!("{}/releases/tags/{}", self.url, percent_encoded(tag)),
            true,
        )
    }

    fn check_repository(&self) -> std::result::Result<(), Failure> {
        self.json(&self.url, false).map(drop)
    }
}

/// One request, which follows no redirect.
struct Request<'a> {
    /// The URL.
    url: &'a str,
    /// The media type that it accepts.
    accept: &'a str,
    /// The token that it carries, if any.
    token: Option<&'a str>,
    /// The file that receives the body of the response.
    output: &'a Path,
    /// The file that receives the headers of the response.
    headers: &'a Path,
    /// Whether `curl` shows its progress bar on standard error, which it then also writes its
    /// errors to, rather than to the message of a failure.
    progress: bool,
}

/// The response to a [`Request`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct Response {
    /// The HTTP status.
    status: u16,
    /// The URL to which it redirects, if it does: that of its `location` header, resolved against
    /// the URL of its request, if its status is 3xx.
    location: Option<String>,
    /// Its headers, by their lowercase names, in order.
    headers: Vec<(String, String)>,
}

impl Response {
    /// Returns whether it reports that the rate limit of the REST API is exhausted.
    fn rate_limited(&self) -> bool {
        self.headers
            .iter()
            .any(|(name, value)| name == "x-ratelimit-remaining" && value == "0")
    }
}

/// The `curl` that requests run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Curl {
    /// The program.
    program: PathBuf,
    /// Whether requests speak HTTPS alone, as every request to GitHub does; tests serve HTTP.
    https_only: bool,
}

impl Curl {
    /// Returns the `curl` of the host, which speaks HTTPS alone.
    pub(super) fn new() -> Self {
        Self {
            program: system_program("curl"),
            https_only: true,
        }
    }

    /// Returns a `curl` that also speaks plain HTTP, to the servers of tests.
    #[cfg(test)]
    pub(super) fn plain() -> Self {
        Self {
            https_only: false,
            ..Self::new()
        }
    }

    /// Runs `request`, and returns its response; a failure names `context`.
    fn request(&self, request: &Request<'_>, context: &str) -> Result<Response> {
        remove_stale(request.output)?;
        remove_stale(request.headers)?;
        let config = self.config(request)?;
        let mut command = Command::new(&self.program);
        command
            .args(["--disable", "--config", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(if request.progress {
                Stdio::inherit()
            } else {
                Stdio::piped()
            });
        let mut child = spawn_command(&mut command).map_err(|error| {
            Exit::error(
                ErrorCode::Unavailable,
                format!(
                    "{context} needs curl, which did not run: {}: {error}",
                    self.program.display()
                ),
            )
        })?;
        if let Some(mut stdin) = child.stdin.take() {
            // `curl` reports a configuration that it did not receive whole.
            let _ = stdin.write_all(config.as_bytes());
        }
        let output = child.wait_with_output().map_err(Exit::from_os_error)?;
        check_interrupt()?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = match stderr.trim() {
                "" => format!("curl exited with {}", output.status),
                text => text.to_owned(),
            };
            return Err(Exit::error(
                ErrorCode::Unavailable,
                format!("{context} failed: {detail}"),
            ));
        }
        read_response(request.headers, request.url)?.ok_or_else(|| {
            Exit::error(
                ErrorCode::Unavailable,
                format!("{context} failed: curl reported no HTTP status"),
            )
        })
    }

    /// Returns the configuration of `request`, which `curl` reads from its standard input.
    fn config(&self, request: &Request<'_>) -> Result<String> {
        let mut lines = vec![
            format!("url = {}", quoted(request.url, "URL")?),
            format!("output = {}", quoted(&path_text(request.output)?, "path")?),
            // The headers give the status and the redirect of the response, which no `write-out`
            // reports: with one, the `curl` 8.8.0 of some Windows releases fails every HTTPS
            // request with error 43 (curl/curl#13845).
            format!(
                "dump-header = {}",
                quoted(&path_text(request.headers)?, "path")?
            ),
            format!("header = \"Accept: {}\"", request.accept),
            format!("header = \"X-GitHub-Api-Version: {API_VERSION}\""),
            format!("user-agent = \"{USER_AGENT}\""),
            "globoff".to_owned(),
            format!("connect-timeout = {CONNECT_TIMEOUT_SECONDS}"),
            "speed-limit = 1".to_owned(),
            format!("speed-time = {STALL_SECONDS}"),
            format!("retry = {RETRIES}"),
            "show-error".to_owned(),
            if request.progress {
                "progress-bar".to_owned()
            } else {
                "silent".to_owned()
            },
        ];
        if let Some(token) = request.token {
            lines.push(format!(
                "header = {}",
                quoted(&format!("Authorization: Bearer {token}"), "GitHub token")?
            ));
        }
        if self.https_only {
            lines.push("proto = \"=https\"".to_owned());
        } else {
            // The servers of tests listen on the loopback interface, which no proxy reaches.
            lines.push("noproxy = \"*\"".to_owned());
        }
        let mut config = lines.join("\n");
        config.push('\n');
        Ok(config)
    }
}

/// Returns the response to a request of `url`, the last one whose headers `curl` wrote to `path`,
/// or `None` without a valid status line to start them.
fn read_response(path: &Path, url: &str) -> Result<Option<Response>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(io_error(ErrorCode::Io, &error, path)),
    };
    let mut status = None;
    let mut headers = Vec::new();
    for line in String::from_utf8_lossy(&bytes).lines() {
        if line.starts_with("HTTP/") {
            // `curl` writes the headers of each response to a request that it retries, and those
            // of an interim response, such as HTTP 103, or of a proxy's tunnel before the final
            // response's; the last one counts.
            status = status_code(line);
            headers.clear();
        } else if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    let Some(status) = status else {
        return Ok(None);
    };
    let location = headers
        .iter()
        .find(|(name, _)| name == "location")
        .map(|(_, location)| location.as_str())
        .filter(|location| (300..400).contains(&status) && !location.is_empty())
        .map(|location| resolved(url, location));
    Ok(Some(Response {
        status,
        location,
        headers,
    }))
}

/// Returns the status of the status line `line`, such as `HTTP/1.1 200 OK` or `HTTP/2 200`, if it
/// holds one.
fn status_code(line: &str) -> Option<u16> {
    let code = line.split_whitespace().nth(1)?;
    if code.len() != 3 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    code.parse().ok().filter(|status| *status >= 100)
}

/// Returns `value`, the `what` of a request, quoted for the configuration of `curl`, or fails for
/// a control character, which the configuration cannot hold. The message does not show the value,
/// which may be a token.
fn quoted(value: &str, what: &str) -> Result<String> {
    if value.chars().any(char::is_control) {
        return Err(Exit::error(
            ErrorCode::Unsupported,
            format!("the {what} of a download holds a control character, which curl cannot take"),
        ));
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

/// Returns `path` as text, which the configuration of `curl` holds.
fn path_text(path: &Path) -> Result<String> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        Exit::error(
            ErrorCode::Unsupported,
            format!("path is not valid Unicode: {}", path.display()),
        )
    })
}

/// Returns the origin of `url`, its scheme and authority in lowercase, if it has them.
fn origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    Some(format!(
        "{}://{}",
        scheme.to_ascii_lowercase(),
        authority.to_ascii_lowercase()
    ))
}

/// The components of a URI reference, as RFC 3986 splits it (appendix B).
struct Reference<'a> {
    /// The scheme, if it has one.
    scheme: Option<&'a str>,
    /// The authority, if it has one, which may be empty.
    authority: Option<&'a str>,
    /// The path, which may be empty.
    path: &'a str,
    /// The query, if it has one, which may be empty.
    query: Option<&'a str>,
    /// The fragment, if it has one, which may be empty.
    fragment: Option<&'a str>,
}

impl<'a> Reference<'a> {
    /// Returns the components of `text`.
    fn of(text: &'a str) -> Self {
        let (text, fragment) = split_at_first(text, '#');
        let (text, query) = split_at_first(text, '?');
        let (scheme, text) = match text.split_once(':') {
            Some((scheme, rest)) if is_scheme(scheme) => (Some(scheme), rest),
            _ => (None, text),
        };
        let (authority, path) = match text.strip_prefix("//") {
            Some(rest) => {
                let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
                (Some(authority), path)
            }
            None => (None, text),
        };
        Self {
            scheme,
            authority,
            path,
            query,
            fragment,
        }
    }
}

/// Returns `text` up to its first `delimiter`, and what follows that, if it has one.
fn split_at_first(text: &str, delimiter: char) -> (&str, Option<&str>) {
    text.split_once(delimiter)
        .map_or((text, None), |(head, tail)| (head, Some(tail)))
}

/// Returns whether `text` is a scheme: a letter, then letters, digits, `+`, `-`, and `.`.
fn is_scheme(text: &str) -> bool {
    let mut bytes = text.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || b"+-.".contains(&byte))
}

/// Returns `reference`, a URI reference such as the `location` header of a redirect, resolved
/// against the absolute URL `base` as RFC 3986 resolves it (section 5.2), with its scheme in
/// lowercase, the canonical form in which [`GitHub::get`] expects HTTPS URLs.
fn resolved(base: &str, reference: &str) -> String {
    let base = Reference::of(base);
    let reference = Reference::of(reference);
    let (authority, path, query) = if reference.scheme.is_some() || reference.authority.is_some() {
        (
            reference.authority,
            without_dot_segments(reference.path),
            reference.query,
        )
    } else if reference.path.is_empty() {
        (
            base.authority,
            base.path.to_owned(),
            reference.query.or(base.query),
        )
    } else if reference.path.starts_with('/') {
        (
            base.authority,
            without_dot_segments(reference.path),
            reference.query,
        )
    } else {
        (
            base.authority,
            without_dot_segments(&merged(&base, reference.path)),
            reference.query,
        )
    };
    let mut target = String::new();
    if let Some(scheme) = reference.scheme.or(base.scheme) {
        target.push_str(&scheme.to_ascii_lowercase());
        target.push(':');
    }
    if let Some(authority) = authority {
        target.push_str("//");
        target.push_str(authority);
    }
    target.push_str(&path);
    for (delimiter, component) in [('?', query), ('#', reference.fragment)] {
        if let Some(component) = component {
            target.push(delimiter);
            target.push_str(component);
        }
    }
    target
}

/// Returns the relative path `path` merged with the path of `base`, as RFC 3986 merges them
/// (section 5.2.3).
fn merged(base: &Reference<'_>, path: &str) -> String {
    if base.authority.is_some() && base.path.is_empty() {
        return format!("/{path}");
    }
    let directory = &base.path[..base.path.rfind('/').map_or(0, |slash| slash + 1)];
    format!("{directory}{path}")
}

/// Returns `path` without its `.` and `..` segments, as RFC 3986 removes them (section 5.2.4).
fn without_dot_segments(path: &str) -> String {
    let mut input = path;
    let mut output = String::with_capacity(path.len());
    while !input.is_empty() {
        if let Some(rest) = input
            .strip_prefix("../")
            .or_else(|| input.strip_prefix("./"))
        {
            input = rest;
        } else if let Some(rest) = after_segment(input, ".") {
            input = rest;
        } else if let Some(rest) = after_segment(input, "..") {
            input = rest;
            output.truncate(output.rfind('/').unwrap_or(0));
        } else if input == "." || input == ".." {
            input = "";
        } else {
            let start = usize::from(input.starts_with('/'));
            let end = input[start..]
                .find('/')
                .map_or(input.len(), |slash| start + slash);
            output.push_str(&input[..end]);
            input = &input[end..];
        }
    }
    output
}

/// Returns the rest of `path` after `/` and the whole segment `segment` that start it, as a path
/// that starts with `/`, if they start it.
fn after_segment<'a>(path: &'a str, segment: &str) -> Option<&'a str> {
    let rest = path.strip_prefix('/')?.strip_prefix(segment)?;
    if rest.is_empty() {
        Some("/")
    } else {
        rest.starts_with('/').then_some(rest)
    }
}

/// Returns `text` percent-encoded for one segment of a URL path, as `urllib.parse.quote(text,
/// safe="")` encodes it: every byte but ASCII letters, digits, and `-._~`.
fn percent_encoded(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Returns the owner and the name of `repository`, if it is `OWNER/REPOSITORY`.
pub(super) fn repository_parts(repository: &str) -> Option<(&str, &str)> {
    let (owner, name) = repository.split_once('/')?;
    (!owner.is_empty() && !name.is_empty() && !name.contains('/')).then_some((owner, name))
}

/// Returns what the error response `body` of GitHub says, as `_github_error_message` of `nvx.py`
/// read it: the `message` of a JSON document, or else at most 200 characters of a body that is
/// not JSON, with its whitespace collapsed.
fn error_message(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let payload = text.trim();
    if payload.is_empty() {
        return String::new();
    }
    match serde_json::from_str::<Value>(payload) {
        Ok(document) => document
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        Err(_) => payload
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(200)
            .collect(),
    }
}

/// Returns `: MESSAGE` without the periods that end `message`, or nothing without a message.
fn detail(message: &str) -> String {
    if message.is_empty() {
        String::new()
    } else {
        format!(": {}", message.trim_end_matches('.'))
    }
}

/// Returns the advice for the HTTP error `status` of a query, as `_github_error_hint` of `nvx.py`
/// gave it, given whether the rate limit is exhausted and whether the query carried a token.
fn hint(status: u16, rate_limited: bool, authenticated: bool) -> &'static str {
    match status {
        401 | 403 | 404 if !authenticated => "set GH_TOKEN to a token that can read the repository",
        401 => "the configured GitHub token was rejected; refresh or replace it",
        403 if rate_limited => "the GitHub API rate limit is exhausted; retry later",
        403 => {
            "the configured GitHub token lacks access; grant it read access to the repository \
             contents and authorize it for organization single sign-on"
        }
        404 => "verify --repository and that the configured GitHub token can read that repository",
        _ => "",
    }
}

#[cfg(test)]
mod tests;
