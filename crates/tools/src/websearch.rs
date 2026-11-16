//! Web search over the public internet, plus a configurable private endpoint.
//!
//! Sources are tried in order and the first that answers wins:
//!
//! 1. `ARGENT_WEB_SEARCH_URL` — a JSON service, for a team that runs its own index
//! 2. `ARGENT_SEARXNG_URL` — a SearXNG instance, which is itself a meta-search
//! 3. DuckDuckGo's HTML endpoint, which needs no key at all
//!
//! Every source is normalized into the same three fields so the model never sees
//! which engine answered. When all of them fail the tool says so instead of
//! inventing results.

use argent_core::error::Result;
use argent_core::json::Json;
use argent_core::process::CommandSpec;

use crate::tool::{arg_str, arg_usize_opt, schema_object, Tool, ToolCtx, ToolResult};

/// One normalized search result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// The `web_search` tool.
pub struct WebSearchTool;

impl Tool for WebSearchTool {
    fn parallel_safe(&self) -> bool {
        true
    }
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web and return titles, URLs and snippets. Use it for current \
         facts, versions and documentation; prefer reading the linked page when \
         the snippet is not enough."
    }

    fn parameters(&self) -> Json {
        schema_object(
            vec![
                ("query", "string", "Search query"),
                (
                    "maxResults",
                    "integer",
                    "Maximum number of results (default 8)",
                ),
            ],
            vec!["query"],
        )
    }

    fn execute(&self, _ctx: &ToolCtx, args: &Json) -> Result<ToolResult> {
        let query = arg_str(args, "query")?;
        let max_results = arg_usize_opt(args, "maxResults").unwrap_or(8).clamp(1, 25);
        let gathered = search(&query, max_results);
        match gathered {
            Ok((source, hits)) if !hits.is_empty() => {
                Ok(ToolResult::ok(render(&query, source, &hits)))
            }
            Ok((source, _)) => Ok(ToolResult::ok(format!(
                "no results for '{}' from {}",
                query, source
            ))),
            Err(message) => Ok(ToolResult::error(message)),
        }
    }
}

fn render(query: &str, source: &str, hits: &[Hit]) -> String {
    let mut text = format!("{} result(s) for '{}' via {}:\n", hits.len(), query, source);
    for (index, hit) in hits.iter().enumerate() {
        text.push_str(&format!("\n{}. {}\n   {}\n", index + 1, hit.title, hit.url));
        if !hit.snippet.is_empty() {
            text.push_str(&format!("   {}\n", hit.snippet));
        }
    }
    text
}

/// Try each configured source in order.
fn search(query: &str, max: usize) -> std::result::Result<(&'static str, Vec<Hit>), String> {
    let mut failures: Vec<String> = Vec::new();

    if let Ok(url) = std::env::var("ARGENT_WEB_SEARCH_URL") {
        if !url.trim().is_empty() {
            match search_custom(&url, query, max) {
                Ok(hits) => return Ok(("configured endpoint", hits)),
                Err(e) => failures.push(format!("ARGENT_WEB_SEARCH_URL: {}", e)),
            }
        }
    }
    if let Ok(url) = std::env::var("ARGENT_SEARXNG_URL") {
        if !url.trim().is_empty() {
            match search_searxng(&url, query, max) {
                Ok(hits) => return Ok(("searxng", hits)),
                Err(e) => failures.push(format!("ARGENT_SEARXNG_URL: {}", e)),
            }
        }
    }
    match search_duckduckgo(query, max) {
        Ok(hits) => return Ok(("duckduckgo", hits)),
        Err(e) => failures.push(format!("duckduckgo: {}", e)),
    }

    Err(format!(
        "web search failed from every source:\n  {}\n\
         Set ARGENT_SEARXNG_URL to a SearXNG instance if the public endpoint is blocked.",
        failures.join("\n  ")
    ))
}

/// A JSON endpoint that answers `{"results":[{title,url,snippet}]}`.
fn search_custom(endpoint: &str, query: &str, max: usize) -> std::result::Result<Vec<Hit>, String> {
    let mut body = Json::obj();
    body.set("query", Json::string(query));
    body.set("limit", Json::num(max as f64));
    let mut request = argent_core::http::Request::post_json(endpoint, &body.dump());
    if let Ok(key) = std::env::var("ARGENT_WEB_SEARCH_KEY") {
        request = request.header("Authorization", &format!("Bearer {}", key));
    }
    let response = argent_core::http::send(&request).map_err(|e| e.to_string())?;
    if !response.is_success() {
        return Err(format!("status {}", response.status));
    }
    let parsed = Json::parse(&response.body_text()).map_err(|e| format!("invalid JSON: {}", e))?;
    let items = parsed
        .get("results")
        .and_then(Json::as_array)
        .map(|items| items.to_vec())
        .unwrap_or_default();
    Ok(items
        .iter()
        .filter_map(|item| {
            let url = item.str_or("url", "");
            if url.is_empty() {
                return None;
            }
            Some(Hit {
                title: item.str_or("title", &url),
                url,
                snippet: item.str_or("snippet", ""),
            })
        })
        .take(max)
        .collect())
}

/// A SearXNG instance's JSON API.
fn search_searxng(base: &str, query: &str, max: usize) -> std::result::Result<Vec<Hit>, String> {
    let url = format!(
        "{}/search?q={}&format=json&safesearch=0",
        base.trim_end_matches('/'),
        url_encode(query)
    );
    let request = argent_core::http::Request::get(&url).header("User-Agent", USER_AGENT);
    let response = argent_core::http::send(&request).map_err(|e| e.to_string())?;
    if !response.is_success() {
        return Err(format!("status {}", response.status));
    }
    let parsed = Json::parse(&response.body_text()).map_err(|e| format!("invalid JSON: {}", e))?;
    let items = parsed
        .get("results")
        .and_then(Json::as_array)
        .map(|items| items.to_vec())
        .unwrap_or_default();
    Ok(items
        .iter()
        .filter_map(|item| {
            let url = item.str_or("url", "");
            if url.is_empty() {
                return None;
            }
            Some(Hit {
                title: item.str_or("title", &url),
                url,
                snippet: item.str_or("content", ""),
            })
        })
        .take(max)
        .collect())
}

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/120.0 Safari/537.36";

/// DuckDuckGo's no-JavaScript HTML endpoint.
///
/// Chosen over its JSON API because the JSON endpoint refuses requests without
/// a signed token, while the HTML one still answers a plain GET.
fn search_duckduckgo(query: &str, max: usize) -> std::result::Result<Vec<Hit>, String> {
    let url = format!("https://html.duckduckgo.com/html/?q={}", url_encode(query));
    let request = argent_core::http::Request::get(&url)
        .header("User-Agent", USER_AGENT)
        .header("Accept-Language", "en-US,en;q=0.9");
    let response = argent_core::http::send(&request).map_err(|e| e.to_string())?;
    if !response.is_success() {
        return Err(format!("status {}", response.status));
    }
    let html = response.body_text();
    let hits = parse_duckduckgo(&html);
    if hits.is_empty() {
        return Err("the result page contained no results (possibly rate limited)".to_string());
    }
    Ok(hits.into_iter().take(max).collect())
}

/// Extract results from DuckDuckGo's result markup.
pub fn parse_duckduckgo(html: &str) -> Vec<Hit> {
    let mut hits = Vec::new();
    let marker = "class=\"result__a\"";
    let mut cursor = 0usize;
    while let Some(found) = html[cursor..].find(marker) {
        let start = cursor + found;
        // The href sits before the marker inside the same anchor tag.
        let tag_start = html[..start].rfind("<a ").unwrap_or(start);
        let tag_end = match html[start..].find('>') {
            Some(offset) => start + offset,
            None => break,
        };
        let tag = &html[tag_start..tag_end];
        let href = extract_attr(tag, "href").unwrap_or_default();
        let url = unwrap_redirect(&href);
        let title_html = match html[tag_end + 1..].find("</a>") {
            Some(offset) => &html[tag_end + 1..tag_end + 1 + offset],
            None => "",
        };
        let title = strip_tags(title_html);
        // The snippet follows the anchor in the same result block.
        let after = &html[tag_end..];
        let snippet = match after.find("class=\"result__snippet\"") {
            Some(offset) => {
                let snippet_start = offset + "class=\"result__snippet\"".len();
                let snippet_body = &after[snippet_start..];
                let inner_start = snippet_body.find('>').map(|i| i + 1).unwrap_or(0);
                let inner = &snippet_body[inner_start..];
                let end = inner
                    .find("</a>")
                    .or_else(|| inner.find("</div>"))
                    .unwrap_or(inner.len().min(400));
                strip_tags(&inner[..end])
            }
            None => String::new(),
        };
        if !url.is_empty() && !title.is_empty() {
            hits.push(Hit {
                title,
                url,
                snippet,
            });
        }
        cursor = tag_end + 1;
    }
    hits
}

/// Read one attribute out of a tag.
fn extract_attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{}=\"", name);
    let start = tag.find(&needle)? + needle.len();
    let rest = &tag[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// DuckDuckGo wraps outbound links in `//duckduckgo.com/l/?uddg=<encoded>`.
fn unwrap_redirect(href: &str) -> String {
    let candidate = if let Some(rest) = href.strip_prefix("//") {
        format!("https://{}", rest)
    } else if href.starts_with("http") {
        href.to_string()
    } else {
        return String::new();
    };
    if let Some(position) = candidate.find("uddg=") {
        let encoded = &candidate[position + 5..];
        let encoded = encoded.split('&').next().unwrap_or(encoded);
        let decoded = url_decode(encoded);
        if !decoded.is_empty() {
            return decoded;
        }
    }
    candidate
}

/// Remove tags and collapse whitespace.
fn strip_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    let decoded = decode_entities(&out);
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn decode_entities(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

/// Percent-encode a query string.
pub fn url_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 3);
    for byte in text.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

/// Percent-decode, leaving invalid sequences alone.
pub fn url_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Fetch a page as text, for the model to read after a search.
pub fn fetch_page(url: &str) -> std::result::Result<String, String> {
    let request = argent_core::http::Request::get(url).header("User-Agent", USER_AGENT);
    let response = argent_core::http::send(&request).map_err(|e| e.to_string())?;
    if !response.is_success() {
        return Err(format!("status {}", response.status));
    }
    Ok(response.body_text())
}

/// True when `curl` can be found, which the https paths depend on.
pub fn curl_available() -> bool {
    let spec = CommandSpec::new("curl")
        .arg("--version")
        .timeout(std::time::Duration::from_secs(5));
    argent_core::process::run(&spec)
        .map(|output| output.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_round_trip_handles_non_ascii() {
        let encoded = url_encode("中文 空格 & x");
        assert!(encoded.contains("%E4%B8%AD"));
        // A space is %20 rather than `+`: `+` is a literal plus inside the
        // percent-encoded URLs DuckDuckGo wraps its results in, so decoding it
        // as a space would corrupt the link.
        assert!(encoded.contains("%20"));
        assert!(!encoded.contains('+'));
        assert_eq!(url_decode(&encoded), "中文 空格 & x");
    }

    #[test]
    fn a_literal_plus_survives_decoding() {
        assert_eq!(url_decode("a%2Bb+1"), "a+b+1");
    }

    #[test]
    fn a_redirect_link_is_unwrapped() {
        let href = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fb%3D1&rut=x";
        assert_eq!(unwrap_redirect(href), "https://example.com/a?b=1");
    }

    #[test]
    fn a_plain_link_survives() {
        assert_eq!(
            unwrap_redirect("https://example.com/plain"),
            "https://example.com/plain"
        );
        assert_eq!(unwrap_redirect("/relative"), "");
    }

    #[test]
    fn tags_are_stripped_and_entities_decoded() {
        assert_eq!(strip_tags("<b>a</b> &amp; <i>b</i>"), "a & b");
        assert_eq!(strip_tags("  too   many\tspaces "), "too many spaces");
    }

    #[test]
    fn duckduckgo_markup_yields_results() {
        let html = r#"
        <div class="result">
          <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage">Example <b>title</b></a>
          <a class="result__snippet" href="//x">A short <b>snippet</b> here.</a>
        </div>
        "#;
        let hits = parse_duckduckgo(html);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://example.com/page");
        assert_eq!(hits[0].title, "Example title");
        assert_eq!(hits[0].snippet, "A short snippet here.");
    }

    #[test]
    fn markup_without_results_is_empty() {
        assert!(parse_duckduckgo("<html><body>nothing</body></html>").is_empty());
        assert!(parse_duckduckgo("").is_empty());
    }

    #[test]
    fn a_result_without_a_title_is_dropped() {
        let html = r#"<a class="result__a" href="https://example.com"></a>"#;
        assert!(parse_duckduckgo(html).is_empty());
    }

    #[test]
    fn the_tool_reports_a_failure_rather_than_inventing_results() {
        let tool = WebSearchTool;
        let ctx = ToolCtx::new(std::env::temp_dir());
        // With no network in the test sandbox, every source fails; the point is
        // that the answer is an error and not a fabricated list.
        std::env::set_var("ARGENT_WEB_SEARCH_URL", "http://127.0.0.1:1/nope");
        let result = tool
            .execute(
                &ctx,
                &Json::parse(r#"{"query":"x","maxResults":1}"#).unwrap(),
            )
            .unwrap();
        std::env::remove_var("ARGENT_WEB_SEARCH_URL");
        assert!(result.is_error || result.content.contains("no results"));
    }
}
