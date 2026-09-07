//! Announces new Kotlin releases and blog posts to Telegram destinations.
//!
//! Two feeds, each with its own destination list and its own delivery records:
//! the GitHub releases Atom feed and the Kotlin blog's RSS feed.
//!
//! Everything above the `wasm32` block is pure and compiles natively, which is
//! what `cargo test` exercises. The Worker glue below implements the two traits
//! over KV and `fetch`.

#![allow(async_fn_in_trait)]

use quick_xml::events::Event;
use quick_xml::Reader;

/// The REST list, not `releases.atom`. The Atom feed is ordered by tag creation
/// and mixes in bare tags, so TeamCity's constant `build-*` tags bury a real
/// release long before it is published. This endpoint returns only actual
/// releases, newest published first.
pub const RELEASES_URL: &str =
    "https://api.github.com/repos/JetBrains/kotlin/releases?per_page=10";
pub const BLOG_URL: &str = "https://blog.jetbrains.com/kotlin/feed/";

/// The feed only ever shows the last ten releases, so nothing older can return.
const SEEN_CAP: usize = 50;

/// Longest rate limit the bot will sit out inside a tick. A cron invocation may
/// run for fifteen minutes of wall clock and waiting costs no CPU, so ten
/// entries each waiting this long still finishes well inside the window.
/// Anything longer is left for the next tick.
const MAX_RETRY_WAIT: u64 = 60;

/// One thing worth announcing. `id` is whatever the feed uses to identify it
/// for good: a release tag, or a blog post's `<guid>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: String,
    pub title: String,
    pub link: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feed {
    Releases,
    Blog,
}

impl Feed {
    /// Each feed keeps its own records, so the two never collide and a
    /// destination can subscribe to one without the other.
    pub fn key_prefix(self) -> &'static str {
        match self {
            Feed::Releases => "seen:release",
            Feed::Blog => "seen:blog",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub key: String,
    pub chat_id: String,
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Sent,
    /// Telegram's own error envelope came back, which proves nothing was posted.
    Rejected,
    /// The send may have landed with only the acknowledgement lost.
    Unknown,
    /// Refused with a rate limit and Telegram's own retry hint, in seconds.
    /// Still proof of non-delivery, so sending again cannot duplicate.
    RetryAfter(u64),
}

pub trait Store {
    /// `Ok(None)` means the destination has no record yet, which is different
    /// from a read that failed.
    async fn get(&self, key: &str) -> Result<Option<Vec<String>>, String>;
    async fn put(&self, key: &str, tags: &[String]) -> Result<(), String>;
}

pub trait Sender {
    async fn send(&self, target: &Target, text: &str) -> Outcome;
    /// Sits out a rate limit. Spends wall clock, not CPU.
    async fn wait(&self, seconds: u64);
}

/// Resolves one `&...;` reference to its text.
fn entity(reference: quick_xml::events::BytesRef) -> String {
    if let Ok(Some(c)) = reference.resolve_char_ref() {
        return c.to_string();
    }
    match reference.into_inner().as_ref() {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        _ => "",
    }
    .to_string()
}

/// GitHub's release list. Only the three fields used are declared, so the
/// release bodies and asset lists — most of the payload — are walked but never
/// allocated, which keeps the parse well inside the CPU budget.
#[derive(serde::Deserialize)]
struct ApiRelease {
    tag_name: String,
    name: Option<String>,
    html_url: String,
}

pub fn parse_releases(json: &str) -> Vec<Entry> {
    serde_json::from_str::<Vec<ApiRelease>>(json)
        .unwrap_or_default()
        .into_iter()
        .map(|release| Entry {
            title: release.name.filter(|n| !n.is_empty()).unwrap_or_else(|| release.tag_name.clone()),
            id: release.tag_name,
            link: release.html_url,
        })
        .collect()
}

/// The blog feed: RSS `<item>`, link as element text, id taken from `<guid>`,
/// which WordPress keeps stable even when a post is renamed or its URL changes.
/// Post bodies arrive as CDATA, so their markup can never be mistaken for feed
/// structure.
pub fn parse_rss(xml: &str) -> Vec<Entry> {
    #[derive(Clone, Copy, PartialEq)]
    enum Field {
        Title,
        Link,
        Guid,
    }

    fn append(field: Option<Field>, text: &str, title: &mut String, link: &mut String, guid: &mut String) {
        match field {
            Some(Field::Title) => title.push_str(text),
            Some(Field::Link) => link.push_str(text),
            Some(Field::Guid) => guid.push_str(text),
            None => {}
        }
    }

    let mut reader = Reader::from_str(xml);
    let mut entries = Vec::new();
    let mut in_item = false;
    // Only set for the three fields collected, so a post body or a
    // channel-level element can never leak into an entry.
    let mut field: Option<Field> = None;
    let (mut title, mut link, mut guid) = (String::new(), String::new(), String::new());

    loop {
        match reader.read_event() {
            Ok(Event::Eof) | Err(_) => break,
            Ok(Event::Start(e)) => match e.name().as_ref() {
                "item" => {
                    in_item = true;
                    title.clear();
                    link.clear();
                    guid.clear();
                }
                "title" if in_item => field = Some(Field::Title),
                "link" if in_item => field = Some(Field::Link),
                "guid" if in_item => field = Some(Field::Guid),
                _ => {}
            },
            Ok(Event::Text(t)) => {
                append(field, &t.xml10_content(), &mut title, &mut link, &mut guid)
            }
            Ok(Event::CData(c)) => {
                append(field, &c.into_inner(), &mut title, &mut link, &mut guid)
            }
            Ok(Event::GeneralRef(r)) => {
                append(field, &entity(r), &mut title, &mut link, &mut guid)
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                "title" | "link" | "guid" => field = None,
                "item" => {
                    let (t, l) = (title.trim(), link.trim());
                    if !t.is_empty() && !l.is_empty() {
                        let id = if guid.trim().is_empty() { l } else { guid.trim() };
                        entries.push(Entry {
                            id: id.to_string(),
                            title: t.to_string(),
                            link: l.to_string(),
                        });
                    }
                    in_item = false;
                }
                _ => {}
            },
            _ => {}
        }
    }
    entries
}

/// Finals (`v2.4.10`) and release candidates (`v2.4.20-RC`, `v2.4.20-RC3`).
/// Rejects betas and every `build-2.5.0-dev-6883` tag TeamCity pushes.
pub fn is_release(tag: &str) -> bool {
    let Some(rest) = tag.strip_prefix('v') else {
        return false;
    };
    let version = match rest.split_once("-RC") {
        Some((version, suffix)) if suffix.chars().all(|c| c.is_ascii_digit()) => version,
        Some(_) => return false,
        None => rest,
    };
    let mut parts = version.split('.');
    let numeric = |p: Option<&str>| p.is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    numeric(parts.next()) && numeric(parts.next()) && numeric(parts.next()) && parts.next().is_none()
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn format_message(feed: Feed, entry: &Entry) -> String {
    let kind = match feed {
        Feed::Releases if entry.id.contains("-RC") => "Kotlin release candidate",
        Feed::Releases => "Kotlin release",
        Feed::Blog => "Kotlin blog",
    };
    format!(
        "<b>{kind}</b>\n<a href=\"{}\">{}</a>",
        escape_html(&entry.link),
        escape_html(&entry.title)
    )
}

pub fn parse_targets(raw: &str) -> Vec<Target> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|key| match key.split_once(':') {
            Some((chat_id, thread_id)) => Target {
                key: key.to_string(),
                chat_id: chat_id.to_string(),
                thread_id: Some(thread_id.to_string()),
            },
            None => Target {
                key: key.to_string(),
                chat_id: key.to_string(),
                thread_id: None,
            },
        })
        .collect()
}

pub fn encode_record(tags: &[String]) -> String {
    serde_json::to_string(tags).unwrap_or_else(|_| "[]".to_string())
}

pub fn decode_record(raw: &str) -> Result<Vec<String>, String> {
    serde_json::from_str(raw).map_err(|e| e.to_string())
}

/// Only Telegram's own error envelope proves the message was never posted. A
/// 5xx, a proxy error page or a dropped connection are all `Unknown`.
pub fn classify(status: u16, body: &str) -> Outcome {
    if (200..300).contains(&status) {
        return Outcome::Sent;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Outcome::Unknown;
    };
    if value.get("ok") != Some(&serde_json::Value::Bool(false)) {
        return Outcome::Unknown;
    }
    match value.pointer("/parameters/retry_after").and_then(serde_json::Value::as_u64) {
        Some(seconds) => Outcome::RetryAfter(seconds),
        None => Outcome::Rejected,
    }
}

/// Walks every destination and returns the lines worth logging.
pub async fn run<S: Store, T: Sender>(
    feed: Feed,
    feed_xml: &str,
    targets: &[Target],
    store: &S,
    sender: &T,
) -> Vec<String> {
    let mut logs = Vec::new();
    let entries: Vec<Entry> = match feed {
        // Only the releases feed is filtered; every blog post is worth sending.
        Feed::Releases => parse_releases(feed_xml).into_iter().filter(|e| is_release(&e.id)).collect(),
        Feed::Blog => parse_rss(feed_xml),
    };
    if entries.is_empty() {
        return logs;
    }

    for target in targets {
        let key = format!("{}:{}", feed.key_prefix(), target.key);
        let seen = match store.get(&key).await {
            Ok(seen) => seen,
            Err(e) => {
                logs.push(format!("target {}: record unreadable: {e}", target.key));
                continue;
            }
        };

        // A destination the bot has never sent to is seeded silently, so neither
        // a fresh deploy nor a newly added chat replays the whole feed.
        let Some(seen) = seen else {
            let tags: Vec<String> = entries.iter().map(|e| e.id.clone()).collect();
            if let Err(e) = store.put(&key, &tags).await {
                logs.push(format!("target {}: seeding failed: {e}", target.key));
            }
            continue;
        };

        let mut record = seen.clone();
        let fresh: Vec<&Entry> = entries.iter().filter(|e| !seen.contains(&e.id)).rev().collect();

        for entry in fresh {
            // Claim first: a crash, an eviction or a lost acknowledgement after
            // this point costs one missed release, never a duplicate.
            record.push(entry.id.clone());
            if record.len() > SEEN_CAP {
                record.drain(..record.len() - SEEN_CAP);
            }
            if let Err(e) = store.put(&key, &record).await {
                // Nothing was claimed, so nothing may be sent.
                logs.push(format!("target {}: {} not claimed: {e}", target.key, entry.id));
                break;
            }

            let text = format_message(feed, entry);
            let mut outcome = sender.send(target, &text).await;

            // Slow mode is permanent in some groups, so a rate limit is the
            // normal case rather than an incident. The 429 carries Telegram's
            // envelope, which proves nothing was posted, so waiting and sending
            // again cannot duplicate the message.
            if let Outcome::RetryAfter(seconds) = outcome {
                if seconds <= MAX_RETRY_WAIT {
                    logs.push(format!(
                        "target {}: rate limited, waiting {seconds}s for {}",
                        target.key, entry.id
                    ));
                    sender.wait(seconds).await;
                    outcome = sender.send(target, &text).await;
                }
            }

            match outcome {
                Outcome::Sent => continue,
                // A rate limit still standing is left for the next tick.
                Outcome::Rejected | Outcome::RetryAfter(_) => {
                    record.pop();
                    match store.put(&key, &record).await {
                        Ok(()) => logs.push(format!(
                            "target {}: {} rejected, will retry next tick",
                            target.key, entry.id
                        )),
                        Err(e) => logs.push(format!(
                            "target {}: {} rejected but the claim is stuck: {e}",
                            target.key, entry.id
                        )),
                    }
                    break;
                }
                Outcome::Unknown => {
                    logs.push(format!(
                        "target {}: {} delivery unknown, SKIPPED to avoid a duplicate",
                        target.key, entry.id
                    ));
                    break;
                }
            }
        }
    }
    logs
}

#[cfg(target_arch = "wasm32")]
mod glue {
    use super::*;
    use std::time::Duration;
    use worker::*;

    struct Kv(kv::KvStore);

    impl Store for Kv {
        async fn get(&self, key: &str) -> Result<Option<Vec<String>>, String> {
            match self.0.get(key).text().await.map_err(|e| e.to_string())? {
                Some(raw) => decode_record(&raw).map(Some),
                None => Ok(None),
            }
        }

        async fn put(&self, key: &str, tags: &[String]) -> Result<(), String> {
            self.0
                .put(key, encode_record(tags))
                .map_err(|e| e.to_string())?
                .execute()
                .await
                .map_err(|e| e.to_string())
        }
    }

    struct Telegram {
        token: String,
    }

    impl Sender for Telegram {
        async fn wait(&self, seconds: u64) {
            Delay::from(Duration::from_secs(seconds)).await
        }

        async fn send(&self, target: &Target, text: &str) -> Outcome {
            let mut body = serde_json::json!({
                "chat_id": target.chat_id,
                "text": text,
                "parse_mode": "HTML",
            });
            if let Some(thread) = &target.thread_id {
                body["message_thread_id"] = serde_json::Value::String(thread.clone());
            }

            let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);
            let Ok(mut response) = post(&url, body.to_string()).await else {
                return Outcome::Unknown;
            };
            let status = response.status_code();
            if (200..300).contains(&status) {
                return Outcome::Sent;
            }
            classify(status, &response.text().await.unwrap_or_default())
        }
    }

    async fn post(url: &str, body: String) -> Result<Response> {
        let headers = Headers::new();
        headers.set("content-type", "application/json")?;
        let request = Request::new_with_init(
            url,
            RequestInit::new()
                .with_method(Method::Post)
                .with_headers(headers)
                .with_body(Some(body.into())),
        )?;
        Fetch::Request(request).send().await
    }

    async fn fetch_feed(url: &str) -> Result<String> {
        let headers = Headers::new();
        // GitHub's API rejects requests without a user agent.
        headers.set("user-agent", "kotlin-releases-bot")?;
        headers.set("accept", "application/vnd.github+json")?;
        let request = Request::new_with_init(url, RequestInit::new().with_headers(headers))?;
        let mut response = Fetch::Request(request).send().await?;
        if !(200..300).contains(&response.status_code()) {
            return Err(Error::RustError(format!("feed {}", response.status_code())));
        }
        response.text().await
    }

    async fn tick(env: Env) -> Vec<String> {
        let token = match env.secret("BOT_TOKEN") {
            Ok(token) => token.to_string(),
            Err(e) => return vec![format!("BOT_TOKEN unavailable: {e}")],
        };
        let kv = match env.kv("SEEN") {
            Ok(kv) => Kv(kv),
            Err(e) => return vec![format!("SEEN binding unavailable: {e}")],
        };
        let sender = Telegram { token };
        let mut logs = Vec::new();

        for (feed, url, var) in [
            (Feed::Releases, RELEASES_URL, "TARGETS"),
            (Feed::Blog, BLOG_URL, "BLOG_TARGETS"),
        ] {
            // An unset or empty list means that feed is simply not subscribed
            // to, which is not worth a log line on every tick.
            let targets = match env.var(var) {
                Ok(raw) => parse_targets(&raw.to_string()),
                Err(_) => continue,
            };
            if targets.is_empty() {
                continue;
            }
            match fetch_feed(url).await {
                Ok(xml) => logs.extend(run(feed, &xml, &targets, &kv, &sender).await),
                Err(e) => logs.push(format!("{var}: feed unreadable: {e}")),
            }
        }
        logs
    }

    // The workers.dev URL is public and always assigned, so it answers with a
    // fixed description. It reads no binding: the KV keys are named after the
    // destinations, and this page must not leak them.
    #[event(fetch)]
    pub async fn fetch(_req: Request, _env: Env, _ctx: Context) -> Result<Response> {
        // Stamped by build.rs at compile time.
        let commit = env!("GIT_COMMIT");
        Response::ok(format!(
            "kotlin-releases-bot\n\n\
             Announces Kotlin releases and blog posts to Telegram.\n\n\
             releases  github api, newest published first\n\
             \x20         keeps vX.Y.Z and vX.Y.Z-RCn, drops betas and build-*-dev-*\n\
             blog      {BLOG_URL}\n\
             \x20         every post\n\n\
             Each feed has its own destination list and its own records.\n\
             schedule  every 15 minutes\n\
             commit    {commit}\n\
             code      https://github.com/CommanderTvis/kotlin-releases-bot/commit/{commit}\n\n\
             There is no API here. The bot runs on a cron trigger and only sends.\n"
        ))
        .map(|mut response| {
            let _ = response.headers_mut().set("content-type", "text/plain; charset=utf-8");
            response
        })
    }

    // Returns unit and must never panic: workers-rs exposes no no_retry, so a
    // failed tick would be replayed. The claim-before-send ordering makes that
    // replay harmless, and returning normally means it is never requested.
    #[event(scheduled)]
    pub async fn scheduled(_event: ScheduledEvent, env: Env, _ctx: ScheduleContext) {
        for line in tick(env).await {
            console_error!("{line}");
        }
    }
}
