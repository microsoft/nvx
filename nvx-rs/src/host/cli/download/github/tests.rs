//! The tests of the client of the GitHub REST API: its messages, its configuration of `curl`,
//! and its requests, which a real `curl` sends to servers on the loopback interface.

use std::io::{BufRead, BufReader};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::json;

use super::*;

/// A request that a test server received.
#[derive(Debug, Clone)]
struct Received {
    /// The path of the request.
    path: String,
    /// Its headers, by their lowercase names.
    headers: Vec<(String, String)>,
}

impl Received {
    /// Returns the value of the header `name`, if the request had it.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
    }
}

/// A canned response: its status, its other headers, and its body.
type Canned = (u16, Vec<(String, String)>, Vec<u8>);

/// An HTTP server on the loopback interface that answers the requests of a path with the canned
/// responses of that path in turn, the last one again once they run out, or else with 404, and
/// records the requests that it receives.
struct Server {
    /// The requests that it received, in order.
    received: Arc<Mutex<Vec<Received>>>,
}

impl Server {
    /// Returns a listener on a free port of the loopback interface and its origin.
    fn bind() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        (listener, origin)
    }

    /// Serves `routes` on `listener`, from a thread of its own.
    fn serve(listener: TcpListener, routes: Vec<(String, Canned)>) -> Self {
        let received = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&received);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = answer(stream, &routes, &log);
            }
        });
        Self { received }
    }

    /// Returns the requests that it received.
    fn received(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }
}

/// Reads one request from `stream`, records it in `log`, and answers it from `routes`, with the
/// canned response of its path that follows those of the earlier requests of that path.
fn answer(
    mut stream: TcpStream,
    routes: &[(String, Canned)],
    log: &Mutex<Vec<Received>>,
) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let path = line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    let mut headers = Vec::new();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header.trim_end().is_empty() {
            break;
        }
        if let Some((name, value)) = header.trim_end().split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    let earlier = {
        let mut log = log.lock().unwrap();
        let earlier = log.iter().filter(|request| request.path == path).count();
        log.push(Received {
            path: path.clone(),
            headers,
        });
        earlier
    };
    let answers: Vec<&Canned> = routes
        .iter()
        .filter(|(route, _)| *route == path)
        .map(|(_, canned)| canned)
        .collect();
    let (status, extra, body) = answers.get(earlier).or(answers.last()).map_or_else(
        || (404, Vec::new(), br#"{"message": "Not Found"}"#.to_vec()),
        |&canned| canned.clone(),
    );
    let mut head = format!(
        "HTTP/1.1 {status} Status\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(&body)?;
    stream.flush()
}

#[test]
fn queries_and_downloads_carry_the_token_to_the_origin_of_the_api_alone() {
    let (api_listener, api) = Server::bind();
    let (asset_listener, assets) = Server::bind();
    let asset_url = format!("{api}/repos/octo/nvx/releases/assets/7");
    let release = json!({
        "draft": false,
        "assets": [{"name": "nvx-0.1.0-linux-kvm.tar.gz", "url": asset_url, "size": 5}],
    });
    let api_server = Server::serve(
        api_listener,
        vec![
            (
                "/repos/octo/nvx/releases/tags/v0.1.0-dev.a1b2c3d4e5f6".to_owned(),
                (200, Vec::new(), release.to_string().into_bytes()),
            ),
            (
                "/repos/octo/nvx/releases/assets/7".to_owned(),
                (
                    302,
                    vec![("Location".to_owned(), format!("{assets}/package"))],
                    Vec::new(),
                ),
            ),
            (
                "/repos/octo/nvx".to_owned(),
                (
                    403,
                    vec![("X-RateLimit-Remaining".to_owned(), "0".to_owned())],
                    br#"{"message": "API rate limit exceeded."}"#.to_vec(),
                ),
            ),
        ],
    );
    let asset_server = Server::serve(
        asset_listener,
        vec![("/package".to_owned(), (200, Vec::new(), b"hello".to_vec()))],
    );
    let scratch = tempfile::tempdir().unwrap();
    let github = GitHub::at(
        &api,
        "octo/nvx",
        Some("secret".to_owned()),
        scratch.path(),
        Curl::plain(),
    )
    .unwrap();

    assert_eq!(
        github.release("v0.1.0-dev.a1b2c3d4e5f6").unwrap(),
        Some(release)
    );
    assert_eq!(github.release("v0.1.0-dev.000000000000").unwrap(), None);
    match github.check_repository() {
        Err(Failure::Http(403, error)) => assert_eq!(
            error.message(),
            Some(
                "GitHub release query failed with HTTP 403: API rate limit exceeded; the GitHub \
                 API rate limit is exhausted; retry later"
            )
        ),
        other => panic!("{other:?}"),
    }

    let asset = Asset {
        tag: "v0.1.0-dev.a1b2c3d4e5f6".to_owned(),
        name: "nvx-0.1.0-linux-kvm.tar.gz".to_owned(),
        url: asset_url,
        size: 5,
    };
    let destination = scratch.path().join(&asset.name);
    github.download(&asset, &destination).unwrap();
    assert_eq!(fs::read(&destination).unwrap(), b"hello");
    let larger = Asset {
        size: 6,
        ..asset.clone()
    };
    let error = github.download(&larger, &destination).unwrap_err();
    assert_eq!(
        error.message(),
        Some("downloaded nvx-0.1.0-linux-kvm.tar.gz is 5 bytes, expected 6")
    );
    let missing = Asset {
        url: format!("{api}/repos/octo/nvx/releases/assets/8"),
        ..asset
    };
    let error = github.download(&missing, &destination).unwrap_err();
    assert_eq!(
        error.message(),
        Some("failed to download nvx-0.1.0-linux-kvm.tar.gz: HTTP 404: Not Found")
    );

    let api_requests = api_server.received();
    assert!(
        api_requests
            .iter()
            .all(|request| request.header("authorization") == Some("Bearer secret")),
        "{api_requests:?}"
    );
    for request in &api_requests {
        let expected = if request.path.contains("/assets/") {
            BINARY
        } else {
            JSON
        };
        assert_eq!(request.header("accept"), Some(expected), "{request:?}");
        assert_eq!(request.header("x-github-api-version"), Some(API_VERSION));
        assert_eq!(request.header("user-agent"), Some(USER_AGENT));
    }
    let asset_requests = asset_server.received();
    assert_eq!(asset_requests.len(), 2, "{asset_requests:?}");
    assert!(
        asset_requests
            .iter()
            .all(|request| request.header("authorization").is_none()),
        "{asset_requests:?}"
    );
}

#[test]
fn requests_without_a_token_carry_none() {
    let (listener, api) = Server::bind();
    let server = Server::serve(listener, Vec::new());
    let scratch = tempfile::tempdir().unwrap();
    let github = GitHub::at(&api, "octo/nvx", None, scratch.path(), Curl::plain()).unwrap();
    match github.check_repository() {
        Err(Failure::Http(404, error)) => assert_eq!(
            error.message(),
            Some(
                "GitHub release query failed with HTTP 404: Not Found; set GH_TOKEN to a token \
                 that can read the repository"
            )
        ),
        other => panic!("{other:?}"),
    }
    let requests = server.received();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/repos/octo/nvx");
    assert_eq!(requests[0].header("authorization"), None);
}

#[test]
fn redirects_resolve_relative_locations_against_the_urls_of_their_requests() {
    let (listener, api) = Server::bind();
    let server = Server::serve(
        listener,
        vec![
            (
                "/repos/octo/nvx/releases/assets/7".to_owned(),
                (
                    302,
                    vec![("Location".to_owned(), "../../downloads/./7".to_owned())],
                    Vec::new(),
                ),
            ),
            (
                "/repos/octo/nvx/downloads/7".to_owned(),
                (
                    307,
                    vec![("Location".to_owned(), "/packages/7?part=1".to_owned())],
                    Vec::new(),
                ),
            ),
            (
                "/packages/7?part=1".to_owned(),
                (200, Vec::new(), b"hello".to_vec()),
            ),
        ],
    );
    let scratch = tempfile::tempdir().unwrap();
    let github = GitHub::at(&api, "octo/nvx", None, scratch.path(), Curl::plain()).unwrap();
    let asset = Asset {
        tag: "v0.1.0-dev.a1b2c3d4e5f6".to_owned(),
        name: "nvx-0.1.0-linux-kvm.tar.gz".to_owned(),
        url: format!("{api}/repos/octo/nvx/releases/assets/7"),
        size: 5,
    };
    let destination = scratch.path().join(&asset.name);
    github.download(&asset, &destination).unwrap();
    assert_eq!(fs::read(&destination).unwrap(), b"hello");
    let paths: Vec<String> = server
        .received()
        .into_iter()
        .map(|request| request.path)
        .collect();
    assert_eq!(
        paths,
        [
            "/repos/octo/nvx/releases/assets/7",
            "/repos/octo/nvx/downloads/7",
            "/packages/7?part=1",
        ]
    );
}

#[test]
fn the_last_response_to_a_retried_request_counts() {
    let (listener, api) = Server::bind();
    let path = "/repos/octo/nvx/releases/tags/v0.1.0-dev.a1b2c3d4e5f6";
    // The status, the header, and the message of the first response, which `curl` retries, would
    // each change the error.
    let server = Server::serve(
        listener,
        vec![
            (
                path.to_owned(),
                (
                    503,
                    vec![("X-RateLimit-Remaining".to_owned(), "0".to_owned())],
                    br#"{"message": "Unavailable"}"#.to_vec(),
                ),
            ),
            (
                path.to_owned(),
                (
                    403,
                    Vec::new(),
                    br#"{"message": "Resource not accessible"}"#.to_vec(),
                ),
            ),
        ],
    );
    let scratch = tempfile::tempdir().unwrap();
    let github = GitHub::at(
        &api,
        "octo/nvx",
        Some("secret".to_owned()),
        scratch.path(),
        Curl::plain(),
    )
    .unwrap();
    match github.release("v0.1.0-dev.a1b2c3d4e5f6") {
        Err(Failure::Http(403, error)) => assert_eq!(
            error.message(),
            Some(
                "GitHub release query failed with HTTP 403: Resource not accessible; the \
                 configured GitHub token lacks access; grant it read access to the repository \
                 contents and authorize it for organization single sign-on"
            )
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(server.received().len(), 2);
}

#[test]
fn requests_to_github_speak_https_alone() {
    let scratch = tempfile::tempdir().unwrap();
    let github = GitHub::at(
        "http://127.0.0.1:9",
        "octo/nvx",
        None,
        scratch.path(),
        Curl::new(),
    )
    .unwrap();
    let error = Exit::from(github.release("v1").unwrap_err());
    assert_eq!(error.code(), Some(ErrorCode::Unavailable));
    assert_eq!(
        error.message(),
        Some(
            "GitHub release query failed: http://127.0.0.1:9/repos/octo/nvx/releases/tags/v1 is \
             not an HTTPS URL"
        )
    );
}

#[test]
fn configurations_quote_their_values_and_carry_the_token_when_asked() {
    let curl = Curl {
        program: PathBuf::from("curl"),
        https_only: true,
    };
    let request = Request {
        url: "https://api.github.com/repos/octo/nvx",
        accept: JSON,
        token: Some("se\"cr\\et"),
        output: Path::new(r"C:\scratch\response"),
        headers: Path::new("headers"),
        progress: false,
    };
    let config = curl.config(&request).unwrap();
    for line in [
        "url = \"https://api.github.com/repos/octo/nvx\"",
        "output = \"C:\\\\scratch\\\\response\"",
        "dump-header = \"headers\"",
        "header = \"Accept: application/vnd.github+json\"",
        "header = \"Authorization: Bearer se\\\"cr\\\\et\"",
        "proto = \"=https\"",
        "silent",
        "show-error",
        "retry = 3",
    ] {
        assert!(
            config.lines().any(|candidate| candidate == line),
            "{line}: {config}"
        );
    }
    assert!(!config.contains("progress-bar") && !config.contains("noproxy"));
    // With any `write-out`, the `curl` 8.8.0 of some Windows releases fails every HTTPS request.
    assert!(!config.contains("write-out"), "{config}");
    let anonymous = curl
        .config(&Request {
            token: None,
            progress: true,
            ..request
        })
        .unwrap();
    assert!(!anonymous.contains("Authorization"), "{anonymous}");
    assert!(anonymous.lines().any(|line| line == "progress-bar"));
    assert!(!anonymous.lines().any(|line| line == "silent"));
    let error = curl
        .config(&Request {
            token: Some("line\nbreak"),
            ..request
        })
        .unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::Unsupported));
    assert!(!error.message().unwrap().contains("break"), "{error:?}");
    assert!(
        error.message().unwrap().contains("GitHub token"),
        "{error:?}"
    );
}

#[test]
fn responses_are_the_last_whose_headers_curl_dumped() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("headers");
    let url = "https://api.github.com/repos/octo/nvx/releases/assets/7";
    let read = |headers: &str| {
        fs::write(&path, headers).unwrap();
        read_response(&path, url).unwrap()
    };
    // A response that `curl` retried, an interim one, and the redirect that counts, whose status
    // line is that of HTTP/2.
    assert_eq!(
        read(
            "HTTP/1.1 503 Busy\r\nX-RateLimit-Remaining: 0\r\nLocation: https://example.com/\r\n\
             \r\nHTTP/2 103 \r\nlink: </style.css>\r\n\r\nHTTP/2 302 \r\nlocation: \
             ../packages/7?sig=a%2Fb\r\ncontent-type: text/plain\r\n\r\n"
        ),
        Some(Response {
            status: 302,
            location: Some(
                "https://api.github.com/repos/octo/nvx/releases/packages/7?sig=a%2Fb".to_owned()
            ),
            headers: vec![
                ("location".to_owned(), "../packages/7?sig=a%2Fb".to_owned()),
                ("content-type".to_owned(), "text/plain".to_owned()),
            ],
        })
    );
    // Only a redirect redirects, to its first location, if that is not empty.
    for (headers, status, location) in [
        ("HTTP/1.1 201 Created\r\nLocation: /x\r\n\r\n", 201, None),
        ("HTTP/1.1 304 Not Modified\r\n\r\n", 304, None),
        ("HTTP/1.1 302 Found\r\nLocation: \r\n\r\n", 302, None),
        (
            "HTTP/1.1 301 Moved\r\nLocation: /x\r\nLocation: /y\r\n\r\n",
            301,
            Some("https://api.github.com/x"),
        ),
    ] {
        let response = read(headers).unwrap();
        assert_eq!(
            (response.status, response.location.as_deref()),
            (status, location),
            "{headers:?}"
        );
    }
    // Without a valid last status line, there is no response.
    for headers in [
        "",
        "Content-Type: text/plain\r\n\r\n",
        "HTTP/1.1 200 OK\r\n\r\nHTTP/1.1 2000 OK\r\n\r\n",
        "HTTP/1.1 099 Early\r\n\r\n",
        "HTTP/1.1\r\n\r\n",
    ] {
        assert_eq!(read(headers), None, "{headers:?}");
    }
    assert_eq!(
        read_response(&directory.path().join("missing"), url).unwrap(),
        None
    );
}

#[test]
fn locations_resolve_as_rfc_3986_resolves_references() {
    // The examples of RFC 3986, section 5.4, with `http:g` as a strict parser resolves it.
    let base = "http://a/b/c/d;p?q";
    for (reference, expected) in [
        ("g:h", "g:h"),
        ("g", "http://a/b/c/g"),
        ("./g", "http://a/b/c/g"),
        ("g/", "http://a/b/c/g/"),
        ("/g", "http://a/g"),
        ("//g", "http://g"),
        ("?y", "http://a/b/c/d;p?y"),
        ("g?y", "http://a/b/c/g?y"),
        ("#s", "http://a/b/c/d;p?q#s"),
        ("g#s", "http://a/b/c/g#s"),
        ("g?y#s", "http://a/b/c/g?y#s"),
        (";x", "http://a/b/c/;x"),
        ("g;x", "http://a/b/c/g;x"),
        ("g;x?y#s", "http://a/b/c/g;x?y#s"),
        ("", "http://a/b/c/d;p?q"),
        (".", "http://a/b/c/"),
        ("./", "http://a/b/c/"),
        ("..", "http://a/b/"),
        ("../", "http://a/b/"),
        ("../g", "http://a/b/g"),
        ("../..", "http://a/"),
        ("../../", "http://a/"),
        ("../../g", "http://a/g"),
        ("../../../g", "http://a/g"),
        ("../../../../g", "http://a/g"),
        ("/./g", "http://a/g"),
        ("/../g", "http://a/g"),
        ("g.", "http://a/b/c/g."),
        (".g", "http://a/b/c/.g"),
        ("g..", "http://a/b/c/g.."),
        ("..g", "http://a/b/c/..g"),
        ("./../g", "http://a/b/g"),
        ("./g/.", "http://a/b/c/g/"),
        ("g/./h", "http://a/b/c/g/h"),
        ("g/../h", "http://a/b/c/h"),
        ("g;x=1/./y", "http://a/b/c/g;x=1/y"),
        ("g;x=1/../y", "http://a/b/c/y"),
        ("g?y/./x", "http://a/b/c/g?y/./x"),
        ("g?y/../x", "http://a/b/c/g?y/../x"),
        ("g#s/./x", "http://a/b/c/g#s/./x"),
        ("g#s/../x", "http://a/b/c/g#s/../x"),
        ("http:g", "http:g"),
    ] {
        assert_eq!(resolved(base, reference), expected, "{reference}");
    }
    // The scheme turns lowercase, and the rest stays as it is.
    assert_eq!(
        resolved(
            "https://api.github.com/repos/octo/nvx/releases/assets/7",
            "HTTPS://Objects.Example.com:443/a/../b?sig=x%2Fy%3D#top"
        ),
        "https://Objects.Example.com:443/b?sig=x%2Fy%3D#top"
    );
    assert_eq!(resolved("http://[::1]:8080", "x"), "http://[::1]:8080/x");
}

#[test]
fn origins_compare_schemes_and_authorities_in_lowercase() {
    assert_eq!(
        origin("HTTPS://API.GitHub.com/repos?x#y").as_deref(),
        Some("https://api.github.com")
    );
    assert_eq!(
        origin("https://api.github.com:443/x").as_deref(),
        Some("https://api.github.com:443")
    );
    assert_ne!(
        origin("https://api.github.com/x"),
        origin("https://objects.githubusercontent.com/x")
    );
    assert_ne!(
        origin("https://api.github.com/x"),
        origin("http://api.github.com/x")
    );
    assert_eq!(origin("api.github.com/x"), None);
}

#[test]
fn path_segments_are_encoded_as_python_quotes_them() {
    assert_eq!(percent_encoded("v0.1.0-dev.abc_~"), "v0.1.0-dev.abc_~");
    assert_eq!(percent_encoded("a b/c?d#e%"), "a%20b%2Fc%3Fd%23e%25");
    assert_eq!(percent_encoded("ü"), "%C3%BC");
}

#[test]
fn repositories_have_an_owner_and_a_name() {
    assert_eq!(repository_parts("octo/nvx"), Some(("octo", "nvx")));
    for invalid in ["octo", "/nvx", "octo/", "octo/nvx/dev", ""] {
        assert_eq!(repository_parts(invalid), None, "{invalid}");
    }
    let scratch = tempfile::tempdir().unwrap();
    let error = GitHub::new("octo", None, scratch.path()).err().unwrap();
    assert_eq!(
        error.message(),
        Some("GitHub repository must be OWNER/REPOSITORY")
    );
}

#[test]
fn error_responses_read_as_nvx_py_read_them() {
    assert_eq!(
        error_message(br#"{"message": "Bad credentials"}"#),
        "Bad credentials"
    );
    assert_eq!(error_message(br#"{"documentation_url": "x"}"#), "");
    assert_eq!(error_message(br#"["message"]"#), "");
    assert_eq!(error_message(b"  \n "), "");
    assert_eq!(
        error_message(b"<html>\n  Bad   gateway\n</html>"),
        "<html> Bad gateway </html>"
    );
    assert_eq!(error_message(&[b'x'; 300]).len(), 200);
    assert_eq!(detail("Not Found."), ": Not Found");
    assert_eq!(detail(""), "");
}

#[test]
fn hints_advise_as_nvx_py_advised() {
    let set_token = "set GH_TOKEN to a token that can read the repository";
    for (status, rate_limited, authenticated, expected) in [
        (401, false, false, set_token),
        (403, true, false, set_token),
        (404, false, false, set_token),
        (
            401,
            false,
            true,
            "the configured GitHub token was rejected; refresh or replace it",
        ),
        (
            403,
            true,
            true,
            "the GitHub API rate limit is exhausted; retry later",
        ),
        (
            403,
            false,
            true,
            "the configured GitHub token lacks access; grant it read access to the repository \
             contents and authorize it for organization single sign-on",
        ),
        (
            404,
            false,
            true,
            "verify --repository and that the configured GitHub token can read that repository",
        ),
        (500, false, false, ""),
        (502, false, true, ""),
    ] {
        assert_eq!(
            hint(status, rate_limited, authenticated),
            expected,
            "{status} {rate_limited} {authenticated}"
        );
    }
}
