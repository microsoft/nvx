//! The HTTP or HTTPS proxy on host IPv4 loopback that is the one way out of a proxy-only network.

use std::net::Ipv4Addr;

use super::rule::parse_port;
use crate::error::{Error, Result};

/// The scheme of a [`NetworkProxy`]'s URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProxyScheme {
    /// `http`.
    Http,
    /// `https`.
    Https,
}

impl ProxyScheme {
    /// Returns the scheme's name: `http` or `https`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }

    /// Returns the scheme that `name` names: `http` or `https`, in lowercase.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for any other name.
    pub fn from_name(name: &str) -> Result<Self> {
        match name {
            "http" => Ok(Self::Http),
            "https" => Ok(Self::Https),
            _ => Err(Error::unsupported(format!(
                "network proxy scheme {name:?} must be http or https"
            ))),
        }
    }
}

/// An HTTP or HTTPS proxy that listens on host IPv4 loopback: the one way out of a proxy-only
/// network, which its guest reaches at the same port of its gateway.
///
/// OpenVMM relays the guest's TCP connections to `port` of its gateway to `127.0.0.1:port` on the
/// host, whatever they carry. The scheme only names the URL that [`url`](Self::url) gives: an
/// HTTPS proxy's certificate must therefore be valid for the gateway's address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NetworkProxy {
    /// TCP port where the proxy listens on `127.0.0.1`, and where the guest reaches it at its
    /// gateway; nonzero, and not 53, where OpenVMM answers DNS at the gateway, over TCP too,
    /// before it relays a connection to a proxy.
    pub port: u16,
    /// Scheme of the proxy's URL, if any.
    pub scheme: Option<ProxyScheme>,
}

impl NetworkProxy {
    /// Creates a proxy at `port` of host loopback, whose URL has no scheme.
    pub const fn new(port: u16) -> Self {
        Self { port, scheme: None }
    }

    /// Names the scheme of the proxy's URL.
    #[must_use]
    pub const fn with_scheme(mut self, scheme: ProxyScheme) -> Self {
        self.scheme = Some(scheme);
        self
    }

    /// Parses the URL of a proxy on host loopback, as the host names it: `http` or `https`,
    /// `127.0.0.1` or `localhost`, and an explicit port, such as `http://127.0.0.1:3128`, with at
    /// most a `/` after the port.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) for another scheme, a
    /// host other than IPv4 loopback, a missing or zero port, or a path, query, or fragment.
    pub fn parse_url(url: &str) -> Result<Self> {
        let refused = |reason: &str| {
            Err(Error::unsupported(format!(
                "network proxy URL {url}: {reason}"
            )))
        };
        let Some((scheme, rest)) = url.split_once("://") else {
            return refused("expected http://HOST:PORT or https://HOST:PORT");
        };
        let scheme = match ProxyScheme::from_name(scheme) {
            Ok(scheme) => scheme,
            Err(_) => return refused("the scheme must be http or https"),
        };
        let authority = rest.strip_suffix('/').unwrap_or(rest);
        if authority.contains(['/', '?', '#']) {
            return refused("nothing but a / may follow the port");
        }
        let Some((host, port)) = authority.rsplit_once(':') else {
            return refused("the port must be explicit");
        };
        if !matches!(host, "127.0.0.1" | "localhost") {
            return refused("the host must be 127.0.0.1 or localhost");
        }
        let port = Some(port)
            .filter(|port| port.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(parse_port);
        let Some(port) = port else {
            return refused("the port must be between 1 and 65535");
        };
        Ok(Self::new(port).with_scheme(scheme))
    }

    /// Parses OpenVMM's `--network-proxy` value, `IPV4:TCP-PORT`, for a guest whose gateway is
    /// `gateway`, which the address must be. The proxy's URL has no scheme.
    ///
    /// Fails with [`ErrorKind::Unsupported`](crate::ErrorKind::Unsupported) and a message that
    /// starts with `endpoint`.
    #[cfg(feature = "cli")]
    pub(crate) fn parse_endpoint(endpoint: &str, gateway: Ipv4Addr) -> Result<Self> {
        let refused = |reason: String| Err(Error::unsupported(format!("{endpoint}: {reason}")));
        let Some((address, port)) = endpoint
            .rsplit_once(':')
            .and_then(|(address, port)| Some((address.parse::<Ipv4Addr>().ok()?, port)))
        else {
            return refused("expected IPV4:TCP-PORT".to_owned());
        };
        let Some(port) = parse_port(port) else {
            return refused("the port must be between 1 and 65535".to_owned());
        };
        if address != gateway {
            return refused(format!(
                "the address must be the guest's gateway, {gateway}"
            ));
        }
        Ok(Self::new(port))
    }

    /// Returns the proxy's URL as the guest sees it, at `gateway`: `SCHEME://GATEWAY:PORT`, or
    /// `GATEWAY:PORT` without a scheme.
    pub fn url(&self, gateway: Ipv4Addr) -> String {
        match self.scheme {
            Some(scheme) => format!("{}://{gateway}:{}", scheme.as_str(), self.port),
            None => format!("{gateway}:{}", self.port),
        }
    }
}
