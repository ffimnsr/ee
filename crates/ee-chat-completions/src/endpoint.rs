//! Credential-safe Chat Completions endpoints.

use url::{Host, Url};

/// An endpoint a bearer credential may be sent to.
///
/// The transport rule is that no provider may hand an API key to an arbitrary
/// origin: HTTPS is required, and plain `http` is accepted only for loopback
/// hosts so a local development proxy keeps working. URLs carrying userinfo
/// (`user:password@`) are rejected because the bearer token is the only
/// credential form this transport sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedEndpoint(String);

/// Why an endpoint cannot be trusted with a credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointError {
    /// The value is not a URL.
    Unparseable {
        /// Parser diagnostic.
        detail: String,
    },
    /// The URL has no host.
    MissingHost,
    /// The URL embedded userinfo credentials.
    EmbeddedCredentials,
    /// Plain `http` was used for a host that is not loopback.
    InsecureOrigin {
        /// Offending host.
        host: String,
    },
    /// The scheme is not HTTP(S) at all.
    UnsupportedScheme {
        /// Offending scheme.
        scheme: String,
    },
}

impl std::fmt::Display for EndpointError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unparseable { detail } => write!(formatter, "endpoint is not a URL: {detail}"),
            Self::MissingHost => formatter.write_str("endpoint has no host"),
            Self::EmbeddedCredentials => formatter.write_str(
                "endpoint embeds credentials; the bearer token is the only credential sent",
            ),
            Self::InsecureOrigin { host } => write!(
                formatter,
                "endpoint uses plain http for {host}; only https or loopback http may receive a \
                 credential"
            ),
            Self::UnsupportedScheme { scheme } => {
                write!(formatter, "endpoint scheme {scheme:?} is not http or https")
            }
        }
    }
}

impl std::error::Error for EndpointError {}

impl TrustedEndpoint {
    /// Validates one endpoint value.
    ///
    /// The original string is preserved so the request targets exactly the
    /// documented URL.
    ///
    /// # Errors
    ///
    /// Returns [`EndpointError`] when the value is unparseable, has no host,
    /// carries userinfo, uses a non-HTTP scheme, or uses plain `http` for a
    /// non-loopback host.
    pub fn parse(value: &str) -> Result<Self, EndpointError> {
        let url = Url::parse(value)
            .map_err(|error| EndpointError::Unparseable { detail: error.to_string() })?;
        match url.scheme() {
            "https" | "http" => {}
            scheme => return Err(EndpointError::UnsupportedScheme { scheme: scheme.to_string() }),
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(EndpointError::EmbeddedCredentials);
        }
        let Some(host) = url.host() else {
            return Err(EndpointError::MissingHost);
        };
        if url.scheme() == "http" && !host_is_loopback(&host) {
            return Err(EndpointError::InsecureOrigin { host: host.to_string() });
        }
        Ok(Self(value.to_string()))
    }

    /// Returns the validated endpoint exactly as supplied.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn host_is_loopback(host: &Host<&str>) -> bool {
    match host {
        Host::Domain(name) => name.eq_ignore_ascii_case("localhost"),
        Host::Ipv4(address) => address.is_loopback(),
        Host::Ipv6(address) => address.is_loopback(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_endpoints_are_accepted_verbatim() {
        let endpoint = TrustedEndpoint::parse("https://openrouter.ai/api/v1/chat/completions")
            .expect("https accepted");

        assert_eq!(endpoint.as_str(), "https://openrouter.ai/api/v1/chat/completions");
    }

    #[test]
    fn loopback_http_is_accepted_for_local_proxies() {
        for value in [
            "http://localhost:8080/v1/chat/completions",
            "http://127.0.0.1:1/v1",
            "http://[::1]:1/v1",
        ] {
            assert!(TrustedEndpoint::parse(value).is_ok(), "{value} should be accepted");
        }
    }

    #[test]
    fn remote_plain_http_is_rejected() {
        assert_eq!(
            TrustedEndpoint::parse("http://gateway.example/v1/chat/completions").unwrap_err(),
            EndpointError::InsecureOrigin { host: String::from("gateway.example") }
        );
    }

    #[test]
    fn non_http_schemes_are_rejected() {
        assert_eq!(
            TrustedEndpoint::parse("file:///etc/passwd").unwrap_err(),
            EndpointError::UnsupportedScheme { scheme: String::from("file") }
        );
        assert!(matches!(
            TrustedEndpoint::parse("ftp://example.com/v1").unwrap_err(),
            EndpointError::UnsupportedScheme { .. }
        ));
    }

    #[test]
    fn embedded_credentials_are_rejected() {
        assert_eq!(
            TrustedEndpoint::parse("https://user:secret@example.com/v1").unwrap_err(),
            EndpointError::EmbeddedCredentials
        );
    }

    #[test]
    fn unparseable_and_hostless_values_are_rejected() {
        assert!(matches!(
            TrustedEndpoint::parse("not a url").unwrap_err(),
            EndpointError::Unparseable { .. }
        ));
        assert!(TrustedEndpoint::parse("https://").is_err(), "a hostless endpoint is rejected");
    }

    #[test]
    fn error_messages_are_deterministic() {
        assert_eq!(
            EndpointError::InsecureOrigin { host: String::from("h") }.to_string(),
            "endpoint uses plain http for h; only https or loopback http may receive a credential"
        );
        assert_eq!(
            EndpointError::EmbeddedCredentials.to_string(),
            "endpoint embeds credentials; the bearer token is the only credential sent"
        );
    }
}
