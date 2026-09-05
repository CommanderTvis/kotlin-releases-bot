//! Announces new Kotlin releases to a list of Telegram destinations.
//!
//! Everything above the `wasm32` block is pure and compiles natively, which is
//! what `cargo test` exercises. The Worker glue below implements the two traits
//! over KV and `fetch`.

#![allow(async_fn_in_trait)]

use quick_xml::events::Event;
use quick_xml::Reader;

pub const FEED_URL: &str = "https://github.com/JetBrains/kotlin/releases.atom";

/// The feed only ever shows the last ten releases, so nothing older can return.
const SEEN_CAP: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub tag: String,
    pub title: String,
    pub link: String,
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
}

pub trait Store {
    /// `Ok(None)` means the destination has no record yet, which is different
    /// from a read that failed.
    async fn get(&self, key: &str) -> Result<Option<Vec<String>>, String>;
    async fn put(&self, key: &str, tags: &[String]) -> Result<(), String>;
}

pub trait Sender {
    async fn send(&self, target: &Target, text: &str) -> Outcome;
}

pub fn parse_feed(xml: &str) -> Vec<Release> {
    let mut reader = Reader::from_str(xml);

    let mut releases = Vec::new();
    let mut in_entry = false;
    let mut in_title = false;
    let mut link: Option<String> = None;
    let mut title = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Eof) | Err(_) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match e.name().as_ref() {
                "entry" => {
                    in_entry = true;
                    link = None;
                    title.clear();
                }
                // The feed carries its own <link> elements too, hence in_entry.
                "link" if in_entry && link.is_none() => {
                    link = e
                        .attributes()
                        .flatten()
                        .find(|a| a.key.as_ref() == "href")
                        .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Explicit1_0).ok())
                        .map(|href| href.into_owned());
                }
                "title" if in_entry => in_title = true,
                _ => {}
            },
            Ok(Event::Text(t)) if in_title => title.push_str(&t.xml10_content()),
            // `&amp;` and friends arrive as their own event, not as text.
            Ok(Event::GeneralRef(r)) if in_title => match r.resolve_char_ref() {
                Ok(Some(c)) => title.push(c),
                _ => title.push_str(match r.into_inner().as_ref() {
                    "amp" => "&",
                    "lt" => "<",
                    "gt" => ">",
                    "quot" => "\"",
                    "apos" => "'",
                    _ => "",
                }),
            },
            Ok(Event::End(e)) => match e.name().as_ref() {
                "title" => in_title = false,
                "entry" => {
                    if let Some(link) = link.take() {
                        let title = title.trim();
                        if !title.is_empty() {
                            releases.push(Release {
                                tag: link.rsplit('/').next().unwrap_or_default().to_string(),
                                title: title.to_string(),
                                link,
                            });
                        }
                    }
                    in_entry = false;
                }
                _ => {}
            },
            _ => {}
        }
    }
    releases
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

pub fn format_message(release: &Release) -> String {
    let kind = if release.tag.contains("-RC") {
        "Kotlin release candidate"
    } else {
        "Kotlin release"
    };
    format!(
        "<b>{kind}</b>\n<a href=\"{}\">{}</a>",
        escape_html(&release.link),
        escape_html(&release.title)
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
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(value) if value.get("ok") == Some(&serde_json::Value::Bool(false)) => Outcome::Rejected,
        _ => Outcome::Unknown,
    }
}

/// Walks every destination and returns the lines worth logging.
pub async fn run<S: Store, T: Sender>(
    feed_xml: &str,
    targets: &[Target],
    store: &S,
    sender: &T,
) -> Vec<String> {
    let mut logs = Vec::new();
    let releases: Vec<Release> = parse_feed(feed_xml)
        .into_iter()
        .filter(|r| is_release(&r.tag))
        .collect();
    if releases.is_empty() {
        return logs;
    }

    for target in targets {
        let key = format!("seen:{}", target.key);
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
            let tags: Vec<String> = releases.iter().map(|r| r.tag.clone()).collect();
            if let Err(e) = store.put(&key, &tags).await {
                logs.push(format!("target {}: seeding failed: {e}", target.key));
            }
            continue;
        };

        let mut record = seen.clone();
        let fresh: Vec<&Release> = releases.iter().filter(|r| !seen.contains(&r.tag)).rev().collect();

        for release in fresh {
            // Claim first: a crash, an eviction or a lost acknowledgement after
            // this point costs one missed release, never a duplicate.
            record.push(release.tag.clone());
            if record.len() > SEEN_CAP {
                record.drain(..record.len() - SEEN_CAP);
            }
            if let Err(e) = store.put(&key, &record).await {
                // Nothing was claimed, so nothing may be sent.
                logs.push(format!("target {}: {} not claimed: {e}", target.key, release.tag));
                break;
            }

            match sender.send(target, &format_message(release)).await {
                Outcome::Sent => continue,
                Outcome::Rejected => {
                    record.pop();
                    match store.put(&key, &record).await {
                        Ok(()) => logs.push(format!(
                            "target {}: {} rejected, will retry next tick",
                            target.key, release.tag
                        )),
                        Err(e) => logs.push(format!(
                            "target {}: {} rejected but the claim is stuck: {e}",
                            target.key, release.tag
                        )),
                    }
                    break;
                }
                Outcome::Unknown => {
                    logs.push(format!(
                        "target {}: {} delivery unknown, SKIPPED to avoid a duplicate",
                        target.key, release.tag
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

    async fn fetch_feed() -> Result<String> {
        let headers = Headers::new();
        headers.set("user-agent", "kotlin-releases-bot")?;
        let request = Request::new_with_init(FEED_URL, RequestInit::new().with_headers(headers))?;
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
        let targets = match env.var("TARGETS") {
            Ok(targets) => parse_targets(&targets.to_string()),
            Err(e) => return vec![format!("TARGETS unavailable: {e}")],
        };
        let kv = match env.kv("SEEN") {
            Ok(kv) => Kv(kv),
            Err(e) => return vec![format!("SEEN binding unavailable: {e}")],
        };
        let feed = match fetch_feed().await {
            Ok(feed) => feed,
            Err(e) => return vec![format!("feed unreadable: {e}")],
        };
        run(&feed, &targets, &kv, &Telegram { token }).await
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
