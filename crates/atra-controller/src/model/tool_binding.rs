use std::{
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use reqwest::{Client, StatusCode, Url, redirect::Policy};
use serde_json::{Value, json};

const MAX_FETCH_BYTES: usize = 5 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Binding {
    Hosted,
    Function(Executor),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Executor {
    Ollama,
    Exa,
    DirectFetch,
}

impl Binding {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Hosted => "hosted",
            Self::Function(Executor::Ollama) => "ollama",
            Self::Function(Executor::Exa) => "exa",
            Self::Function(Executor::DirectFetch) => "direct",
        }
    }
}

pub(super) fn codex(name: &str) -> Option<Binding> {
    (name == "web_search").then_some(Binding::Hosted)
}

pub(super) async fn execute(
    binding: Binding,
    client: &Client,
    api_key: Option<&str>,
    name: &str,
    arguments: &Value,
) -> Result<Option<Value>> {
    match binding {
        Binding::Hosted => Ok(None),
        Binding::Function(Executor::Exa) => exa(client, arguments).await.map(Some),
        Binding::Function(Executor::DirectFetch) => direct_fetch(arguments).await.map(Some),
        Binding::Function(Executor::Ollama) => {
            let api_key = api_key.context("Ollama API key is unavailable")?;
            ollama(client, api_key, name, arguments).await.map(Some)
        }
    }
}

async fn exa(client: &Client, arguments: &Value) -> Result<Value> {
    let query = arguments["query"]
        .as_str()
        .context("web_search requires string property `query`")?;
    let count = arguments["max_results"].as_u64().unwrap_or(5).clamp(1, 10);
    let mut url = Url::parse("https://mcp.exa.ai/mcp")?;
    if let Ok(key) = std::env::var("EXA_API_KEY")
        && !key.trim().is_empty()
    {
        url.query_pairs_mut().append_pair("exaApiKey", key.trim());
    }
    let body = client
        .post(url)
        .header("accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "web_search_exa",
                "arguments": {
                    "query": query,
                    "type": "auto",
                    "numResults": count,
                    "livecrawl": "fallback",
                }
            }
        }))
        .send()
        .await
        .context("Exa search request failed")?
        .error_for_status()
        .context("Exa search failed")?
        .text()
        .await?;
    for payload in std::iter::once(body.trim())
        .chain(body.lines().filter_map(|line| line.strip_prefix("data: ")))
    {
        if !payload.starts_with('{') {
            continue;
        }
        let value: Value = serde_json::from_str(payload)?;
        if let Some(text) = value
            .pointer("/result/content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find_map(|item| item["text"].as_str())
        {
            return Ok(Value::String(text.to_owned()));
        }
    }
    bail!("Exa returned no textual search result")
}

async fn ollama(client: &Client, key: &str, name: &str, arguments: &Value) -> Result<Value> {
    let path = match name {
        "web_search" => "web_search",
        "web_fetch" => "web_fetch",
        _ => bail!("unsupported Ollama tool {name}"),
    };
    client
        .post(format!("https://ollama.com/api/{path}"))
        .bearer_auth(key)
        .json(arguments)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .with_context(|| format!("failed to decode Ollama {name} result"))
}

async fn direct_fetch(arguments: &Value) -> Result<Value> {
    let mut url = Url::parse(
        arguments["url"]
            .as_str()
            .context("web_fetch requires string property `url`")?,
    )?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "web_fetch only supports HTTP and HTTPS"
    );
    let deadline = Instant::now() + FETCH_TIMEOUT;
    for _ in 0..=MAX_REDIRECTS {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .context("web_fetch timed out")?;
        let (host, addresses) = resolve_public(&url, remaining).await?;
        let client = Client::builder()
            .redirect(Policy::none())
            .no_proxy()
            .connect_timeout(remaining.min(Duration::from_secs(5)))
            .timeout(remaining)
            .resolve_to_addrs(&host, &addresses)
            .build()
            .context("failed to build the web_fetch HTTP client")?;
        let response = client
            .get(url.clone())
            .header("user-agent", "Atra/1")
            .header("accept", "text/markdown, text/plain;q=0.9, text/html;q=0.8")
            .send()
            .await
            .with_context(|| format!("failed to fetch {url}"))?;
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .context("redirect response omitted Location")?
                .to_str()?;
            url = url.join(location)?;
            continue;
        }
        if response.status() == StatusCode::REQUEST_TIMEOUT {
            bail!("web_fetch timed out");
        }
        let response = response.error_for_status()?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_FETCH_BYTES as u64)
        {
            bail!("web_fetch response exceeds {MAX_FETCH_BYTES} bytes");
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let bytes = response.bytes().await?;
        ensure!(
            bytes.len() <= MAX_FETCH_BYTES,
            "web_fetch response exceeds {MAX_FETCH_BYTES} bytes"
        );
        let text = String::from_utf8_lossy(&bytes);
        let output = if content_type.contains("text/html") {
            htmd::convert(&text).context("failed to convert fetched HTML to Markdown")?
        } else {
            text.into_owned()
        };
        return Ok(json!({
            "url": url.as_str(),
            "content_type": content_type,
            "markdown": output,
        }));
    }
    bail!("web_fetch exceeded {MAX_REDIRECTS} redirects")
}

async fn resolve_public(url: &Url, timeout: Duration) -> Result<(String, Vec<SocketAddr>)> {
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "web_fetch only supports HTTP and HTTPS"
    );
    let host = url.host_str().context("web_fetch URL has no host")?;
    let port = url
        .port_or_known_default()
        .context("URL has no known port")?;
    let addresses = tokio::time::timeout(timeout, tokio::net::lookup_host((host, port)))
        .await
        .context("web_fetch DNS lookup timed out")?
        .with_context(|| format!("failed to resolve {host}"))?
        .collect::<Vec<_>>();
    ensure!(
        !addresses.is_empty(),
        "web_fetch host resolved to no addresses"
    );
    for address in &addresses {
        ensure!(
            public_ip(address.ip()),
            "web_fetch refuses non-public address {}",
            address.ip()
        );
    }
    Ok((host.to_owned(), addresses))
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_unspecified()
                || ip.is_multicast())
        }
        IpAddr::V6(ip) => {
            if let Some(ip) = ip.to_ipv4_mapped() {
                return public_ip(IpAddr::V4(ip));
            }
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_unique_local()
                || ip.is_unicast_link_local())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_public_addresses() {
        for address in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.1.1",
            "192.168.1.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
        ] {
            assert!(!public_ip(address.parse().unwrap()), "{address}");
        }
        assert!(public_ip("8.8.8.8".parse().unwrap()));
        assert!(public_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    #[tokio::test]
    async fn rejects_private_fetch_targets() {
        assert!(
            resolve_public(
                &Url::parse("http://127.0.0.1/").unwrap(),
                Duration::from_secs(1),
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn returns_the_validated_public_socket_addresses() {
        let (host, addresses) = resolve_public(
            &Url::parse("https://1.1.1.1/example").unwrap(),
            Duration::from_secs(1),
        )
        .await
        .unwrap();

        assert_eq!(host, "1.1.1.1");
        assert_eq!(addresses, vec!["1.1.1.1:443".parse().unwrap()]);
    }

    #[test]
    fn converts_html_to_markdown() {
        assert_eq!(
            htmd::convert("<h1>Hello</h1><p>world</p>").unwrap(),
            "# Hello\n\nworld"
        );
    }
}
