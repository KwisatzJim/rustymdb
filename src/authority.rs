use axum::{
    extract::{Request, State},
    http::{StatusCode, header::HOST, uri::Authority},
    middleware::Next,
    response::Response,
};
use std::{collections::HashSet, net::SocketAddr};

#[derive(Clone)]
pub struct AllowedHosts(HashSet<String>);

// Parse authorities only, never URLs, user information or forwarded headers.
fn normalize(value: &str) -> Option<String> {
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b))
    {
        return None;
    }
    let authority: Authority = value.parse().ok()?;
    let host = authority.host();
    let normalized_host = if let Some(ip) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']'))
    {
        format!("[{}]", ip.parse::<std::net::Ipv6Addr>().ok()?)
    } else if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
        ip.to_string()
    } else {
        if host.len() > 253
            || !host.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && label.as_bytes()[0].is_ascii_alphanumeric()
                    && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
        {
            return None;
        }
        host.to_ascii_lowercase()
    };
    // Reject empty, overflowing and noncanonical ports instead of discarding them.
    let suffix = &value[host.len()..];
    let port = if suffix.is_empty() {
        String::new()
    } else {
        let port = authority.port_u16()?;
        if port == 0 || suffix != format!(":{port}") {
            return None;
        }
        format!(":{port}")
    };
    Some(format!("{normalized_host}{port}"))
}

impl AllowedHosts {
    pub fn new(bound: SocketAddr, additional: Option<&str>) -> Result<Self, String> {
        let mut hosts = HashSet::new();
        for host in ["localhost".to_owned(), "127.0.0.1".into(), "[::1]".into()] {
            hosts.insert(format!("{host}:{}", bound.port()));
            if bound.port() == 80 {
                hosts.insert(host);
            }
        }
        if !bound.ip().is_unspecified() {
            let host = match bound.ip() {
                std::net::IpAddr::V4(ip) => ip.to_string(),
                std::net::IpAddr::V6(ip) => format!("[{ip}]"),
            };
            hosts.insert(format!("{host}:{}", bound.port()));
            if bound.port() == 80 {
                hosts.insert(host);
            }
        }
        if let Some(additional) = additional {
            for entry in additional.split(',') {
                let host = normalize(entry.trim()).ok_or_else(|| {
                    "RUSTYMDB_ALLOWED_HOSTS must contain comma-separated host authorities, without schemes, paths or wildcards".to_owned()
                })?;
                hosts.insert(host);
            }
        }
        Ok(Self(hosts))
    }

    fn accepts(&self, request: &Request) -> bool {
        let mut values = request.headers().get_all(HOST).iter();
        let Some(host) = values
            .next()
            .and_then(|v| v.to_str().ok())
            .and_then(normalize)
        else {
            return false;
        };
        if values.next().is_some() || !self.0.contains(&host) {
            return false;
        }
        request
            .uri()
            .authority()
            .is_none_or(|a| normalize(a.as_str()).as_ref() == Some(&host))
    }
}

pub async fn enforce(
    State(allowed): State<AllowedHosts>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if !allowed.accepts(&request) {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, middleware, routing::get};

    #[test]
    fn authority_defaults_follow_concrete_and_wildcard_binds() {
        for (bound, host) in [
            ("192.168.1.20:80", "192.168.1.20"),
            ("[2001:db8::1]:3000", "[2001:0db8::1]:3000"),
        ] {
            let allowed = AllowedHosts::new(bound.parse().unwrap(), None).unwrap();
            assert!(
                allowed.accepts(
                    &Request::builder()
                        .header(HOST, host)
                        .body(Body::empty())
                        .unwrap()
                )
            );
        }
        let allowed = AllowedHosts::new("0.0.0.0:3000".parse().unwrap(), None).unwrap();
        let request = Request::builder()
            .header(HOST, "192.168.1.20:3000")
            .header("x-forwarded-host", "localhost:3000")
            .body(Body::empty())
            .unwrap();
        assert!(!allowed.accepts(&request));
    }

    #[test]
    fn authority_rejects_ambiguous_headers_and_targets() {
        let allowed = AllowedHosts::new(
            "127.0.0.1:3000".parse().unwrap(),
            Some("movies.home.arpa,[2001:db8::1]:3000"),
        )
        .unwrap();
        for host in [
            "localhost:3000",
            "LOCALHOST:3000",
            "127.0.0.1:3000",
            "[::1]:3000",
            "movies.home.arpa",
            "[2001:0db8::1]:3000",
        ] {
            assert!(
                allowed.accepts(
                    &Request::builder()
                        .header(HOST, host)
                        .body(Body::empty())
                        .unwrap()
                ),
                "{host}"
            );
        }
        for host in [
            "evil.example:3000",
            "localhost.evil:3000",
            "localhost:3001",
            "localhost",
            "localhost:03000",
            "localhost:65536",
            "localhost:",
            "localhost:0",
            "localhost:3000,evil.example",
            "user@localhost:3000",
            "localhost.:3000",
            "http://localhost:3000",
            "::1:3000",
        ] {
            assert!(
                !allowed.accepts(
                    &Request::builder()
                        .header(HOST, host)
                        .body(Body::empty())
                        .unwrap()
                ),
                "{host}"
            );
        }
        assert!(!allowed.accepts(&Request::new(Body::empty())));
        let duplicate = Request::builder()
            .header(HOST, "localhost:3000")
            .header(HOST, "localhost:3000")
            .body(Body::empty())
            .unwrap();
        assert!(!allowed.accepts(&duplicate));
        let absolute = Request::builder()
            .uri("http://evil.example:3000/options")
            .header(HOST, "localhost:3000")
            .body(Body::empty())
            .unwrap();
        assert!(!allowed.accepts(&absolute));
        assert!(AllowedHosts::new("127.0.0.1:3000".parse().unwrap(), Some("*")).is_err());
    }

    #[tokio::test]
    async fn authority_middleware_blocks_reads_and_writes_before_handlers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bound = listener.local_addr().unwrap();
        let allowed = AllowedHosts::new(bound, Some("movies.home.arpa")).unwrap();
        let app = Router::new()
            .route(
                "/options",
                get(|| async { "private-form-token" }).post(|| async { "changed" }),
            )
            .layer(middleware::from_fn_with_state(allowed, enforce));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let url = format!("http://{bound}/options");
        for method in [reqwest::Method::GET, reqwest::Method::POST] {
            let response = client
                .request(method.clone(), &url)
                .header(HOST, "evil.example:3000")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert!(
                !response
                    .text()
                    .await
                    .unwrap()
                    .contains("private-form-token")
            );
            let response = client.request(method, &url).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        let response = client
            .get(&url)
            .header(HOST, "movies.home.arpa")
            .send()
            .await
            .unwrap();
        assert_eq!(response.text().await.unwrap(), "private-form-token");
        server.abort();
    }
}
