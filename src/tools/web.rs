//! `web_search` and `web_fetch`, backed by the Kagi API (`/search` and `/extract`).

use std::{sync::Arc, time::Duration};

use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolOutput, opt_str, opt_u64, schema, str_arg};
use crate::llm::{BoxFuture, ToolSpec};

pub const DEFAULT_BASE_URL: &str = "https://kagi.com/api/v1";
const SNIPPET_CHARS: usize = 400;
/// Extraction has a 10 s budget on Kagi's side; this leaves room for the round trip.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);

/// A Kagi API client shared by both tools.
pub struct Kagi {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl Kagi {
    /// @param base_url API root without a trailing slash (`DEFAULT_BASE_URL` in production).
    pub fn new(api_key: String, base_url: String) -> anyhow::Result<Arc<Self>> {
        let client = reqwest::Client::builder().connect_timeout(Duration::from_secs(5)).build()?;
        Ok(Arc::new(Self { client, base_url: base_url.trim_end_matches('/').to_owned(), api_key }))
    }

    async fn post(&self, path: &str, body: Value, timeout: Duration) -> Result<Value, String> {
        let resp = self
            .client
            .post(format!("{}{path}", self.base_url))
            .bearer_auth(&self.api_key)
            .timeout(timeout)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Kagi request failed: {e}"))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| format!("reading Kagi response: {e}"))?;
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if !status.is_success() {
            return Err(api_error(status.as_u16(), &value, &text));
        }
        if value.is_null() {
            return Err(format!("Kagi returned a response that is not JSON: {}", text.chars().take(200).collect::<String>()));
        }
        Ok(value)
    }
}

/// `Kagi API error 401: Unauthorized (code 2)` from an error envelope, or the raw body.
fn api_error(status: u16, value: &Value, text: &str) -> String {
    let detail = value["error"].as_array().and_then(|e| e.first());
    let what = match detail {
        Some(d) => {
            let message = d["message"].as_str().unwrap_or("no message");
            match d["code"].as_str() {
                Some(code) => format!("{message} (code {code})"),
                None => message.to_owned(),
            }
        }
        None => text.chars().take(200).collect(),
    };
    let hint = match status {
        401 => " — check KAGI_API_KEY",
        429 => " — rate limited or out of API credit",
        _ => "",
    };
    format!("Kagi API error {status}: {what}{hint}")
}

/// Strings from a `string` or `[string]` argument.
fn str_list(args: &Value, key: &str) -> Vec<String> {
    match args.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() => vec![s.trim().to_owned()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect(),
        _ => Vec::new(),
    }
}

/// `web_search`: ranked results with snippets.
pub struct WebSearch(pub Arc<Kagi>);

impl Tool for WebSearch {
    fn guidance(&self) -> Option<&'static str> {
        Some(
            "Use web_search for anything that may have changed since your training or that you are unsure of: current library versions and APIs, error messages, documentation. Snippets are short; web_fetch the most relevant results before relying on details. Mention the URLs you used.",
        )
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_search".into(),
            description: "Search the web (Kagi). Returns numbered results with title, URL, date and snippet, plus \
                          direct answers and related searches when there are any."
                .into(),
            parameters: schema(
                json!({
                    "query": {"type": "string", "description": "Search query; Kagi operators such as site: and quotes work"},
                    "limit": {"type": "integer", "description": "Maximum results (default 10)"},
                    "recency": {"type": "string", "enum": ["day", "week", "month"], "description": "Only pages updated within this period"},
                    "sites": {"type": "array", "items": {"type": "string"}, "description": "Only search these domains"}
                }),
                &["query"],
            ),
        }
    }

    fn parallel_safe(&self) -> bool {
        true
    }

    fn run<'a>(&'a self, args: Value, _ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let query = match str_arg(&args, "query") {
                Ok(q) if !q.trim().is_empty() => q,
                Ok(_) => return ToolOutput::err("`query` is empty"),
                Err(e) => return ToolOutput::err(e),
            };
            let limit = match opt_u64(&args, "limit") {
                Ok(l) => l.unwrap_or(10).clamp(1, 50),
                Err(e) => return ToolOutput::err(e),
            };
            let mut lens = serde_json::Map::new();
            match opt_str(&args, "recency").as_deref() {
                None => {}
                Some(r @ ("day" | "week" | "month")) => drop(lens.insert("time_relative".into(), json!(r))),
                Some(r) => return ToolOutput::err(format!("`recency` must be day, week or month, not {r:?}")),
            }
            let sites = str_list(&args, "sites");
            if !sites.is_empty() {
                lens.insert("sites_included".into(), json!(sites));
            }
            let mut body = json!({"query": query, "limit": limit});
            if !lens.is_empty() {
                body["lens"] = Value::Object(lens);
            }
            ToolOutput::from_result(self.0.post("/search", body, SEARCH_TIMEOUT).await.map(|v| format_search(&v, limit as usize)))
        })
    }
}

/// Renders a `/search` response for the model.
pub fn format_search(v: &Value, limit: usize) -> String {
    let data = &v["data"];
    let list = |key: &str| data[key].as_array().cloned().unwrap_or_default();
    let mut out = String::new();
    for answer in list("direct_answer").iter().chain(list("infobox").iter()) {
        let text = clean(answer["snippet"].as_str().unwrap_or(""));
        let title = clean(answer["title"].as_str().unwrap_or(""));
        if !text.is_empty() || !title.is_empty() {
            out.push_str(&format!("Answer: {}\n", [title, text].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" — ")));
        }
    }
    let results = list("search");
    for (i, r) in results.iter().take(limit).enumerate() {
        let title = clean(r["title"].as_str().unwrap_or("(untitled)"));
        let url = r["url"].as_str().unwrap_or("");
        out.push_str(&format!("{}. {title}\n   {url}\n", i + 1));
        let date = r["time"].as_str().and_then(|t| t.get(..10));
        let snippet = shorten(&clean(r["snippet"].as_str().unwrap_or("")), SNIPPET_CHARS);
        match (date, snippet.is_empty()) {
            (Some(d), false) => out.push_str(&format!("   {d} · {snippet}\n")),
            (Some(d), true) => out.push_str(&format!("   {d}\n")),
            (None, false) => out.push_str(&format!("   {snippet}\n")),
            (None, true) => {}
        }
    }
    if results.is_empty() && out.is_empty() {
        out.push_str("No results.\n");
    }
    let related: Vec<String> = list("related_search")
        .iter()
        .filter_map(|r| r["title"].as_str().filter(|t| !t.trim().is_empty()).or(r["props"]["question"].as_str()).map(clean))
        .filter(|s| !s.is_empty())
        .take(8)
        .collect();
    if !related.is_empty() {
        out.push_str(&format!("Related searches: {}\n", related.join(" · ")));
    }
    out.trim_end().to_owned()
}

/// `web_fetch`: pages as markdown.
pub struct WebFetch(pub Arc<Kagi>);

impl Tool for WebFetch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_fetch".into(),
            description: "Fetch up to 10 https web pages and return each one's main content as markdown.".into(),
            parameters: schema(
                json!({
                    "urls": {"type": "array", "items": {"type": "string"}, "description": "1-10 https URLs"}
                }),
                &["urls"],
            ),
        }
    }

    fn parallel_safe(&self) -> bool {
        true
    }

    fn run<'a>(&'a self, args: Value, _ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            // Models sometimes pass a single `url`; accept it.
            let mut urls = str_list(&args, "urls");
            urls.extend(str_list(&args, "url"));
            match urls.len() {
                0 => return ToolOutput::err("missing required argument `urls`"),
                11.. => return ToolOutput::err(format!("at most 10 URLs per call, got {}", urls.len())),
                _ => {}
            }
            // Kagi extracts https only; most http pages redirect there anyway.
            let urls: Vec<String> =
                urls.into_iter().map(|u| u.strip_prefix("http://").map_or(u.clone(), |rest| format!("https://{rest}"))).collect();
            let body = json!({"pages": urls.iter().map(|u| json!({"url": u})).collect::<Vec<_>>()});
            match self.0.post("/extract", body, FETCH_TIMEOUT).await {
                Ok(v) => format_extract(&v),
                Err(e) => ToolOutput::err(e),
            }
        })
    }
}

/// Renders an `/extract` response; an error only when no page came back.
pub fn format_extract(v: &Value) -> ToolOutput {
    let pages = v["data"].as_array().cloned().unwrap_or_default();
    let mut out = Vec::new();
    let mut any = false;
    for p in &pages {
        let url = p["url"].as_str().unwrap_or("?");
        match p["markdown"].as_str().filter(|m| !m.trim().is_empty()) {
            Some(md) => {
                any = true;
                out.push(format!("# {url}\n\n{}", md.trim()));
            }
            None => out.push(format!("# {url}\n\n(failed: {})", p["error"].as_str().unwrap_or("no content extracted"))),
        }
    }
    for e in v["errors"].as_array().into_iter().flatten() {
        let at = e["location"].as_str().map(|l| format!(" at {l}")).unwrap_or_default();
        out.push(format!("(error{at}: {})", e["message"].as_str().or(e["code"].as_str()).unwrap_or("unknown")));
    }
    let text = if out.is_empty() { "No pages returned.".to_owned() } else { out.join("\n\n") };
    ToolOutput { content: text, is_error: !any }
}

/// Decodes the HTML entities Kagi leaves in titles and snippets and collapses whitespace.
fn clean(s: &str) -> String {
    let s = s
        .replace("&#39;", "'")
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&");
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let cut: String = s.chars().take(max).collect();
    let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head);
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_results_are_numbered_with_dates_answers_and_related_searches() {
        let v = json!({"meta": {"trace": "t"}, "data": {
            "direct_answer": [{"url": "", "title": "2 + 2", "snippet": "4"}],
            "search": [
                {"url": "https://a.example/x", "title": "A &amp; B", "snippet": "It&#39;s   here", "time": "2024-11-29T03:54:26Z"},
                {"url": "https://b.example", "title": "B"},
                {"url": "https://c.example", "title": "C", "snippet": "cut"}
            ],
            "related_search": [{"url": "", "title": "a b c"}, {"url": "", "title": "", "props": {"question": "why?"}}]
        }});
        assert_eq!(
            format_search(&v, 2),
            "Answer: 2 + 2 — 4\n1. A & B\n   https://a.example/x\n   2024-11-29 · It's here\n2. B\n   https://b.example\nRelated searches: a b c · why?"
        );
        assert_eq!(format_search(&json!({"data": {}}), 5), "No results.");
    }

    #[test]
    fn extract_keeps_pages_that_worked_and_notes_the_rest() {
        let v = json!({"data": [
            {"url": "https://a.example", "markdown": "# A\nbody"},
            {"url": "https://b.example", "error": "timeout"}
        ]});
        let out = format_extract(&v);
        assert!(!out.is_error);
        assert_eq!(out.content, "# https://a.example\n\n# A\nbody\n\n# https://b.example\n\n(failed: timeout)");
        assert!(format_extract(&json!({"data": [{"url": "https://b.example"}]})).is_error);
    }

    #[test]
    fn api_errors_name_the_cause() {
        let v = json!({"meta": {}, "data": null, "error": [{"code": "2", "url": "u", "message": "Unauthorized", "location": null}]});
        assert_eq!(api_error(401, &v, ""), "Kagi API error 401: Unauthorized (code 2) — check KAGI_API_KEY");
        assert_eq!(api_error(502, &Value::Null, "bad gateway"), "Kagi API error 502: bad gateway");
    }

    #[test]
    fn long_snippets_end_at_a_word() {
        assert_eq!(shorten("one two three", 9), "one two…");
        assert_eq!(shorten("short", 9), "short");
    }
}
