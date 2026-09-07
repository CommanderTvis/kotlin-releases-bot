use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use kotlin_releases_bot::*;
use pollster::block_on;

const RELEASES: &str = include_str!("releases.json");
const MIXED: &str = include_str!("releases-two.json");
const BLOG: &str = include_str!("blog.xml");

type Log = Rc<RefCell<Vec<String>>>;

struct FakeStore {
    map: RefCell<HashMap<String, Vec<String>>>,
    log: Log,
    refuse_put: bool,
}

impl Store for FakeStore {
    async fn get(&self, key: &str) -> Result<Option<Vec<String>>, String> {
        Ok(self.map.borrow().get(key).cloned())
    }

    async fn put(&self, key: &str, tags: &[String]) -> Result<(), String> {
        if self.refuse_put {
            return Err("store is down".to_string());
        }
        self.log.borrow_mut().push(format!("put {key} {}", encode_record(tags)));
        self.map.borrow_mut().insert(key.to_string(), tags.to_vec());
        Ok(())
    }
}

struct FakeSender {
    /// One scripted outcome per send, consumed in order; anything after the
    /// script succeeds.
    script: RefCell<HashMap<String, VecDeque<Outcome>>>,
    log: Log,
    sent: RefCell<Vec<(String, Option<String>, String)>>,
    waits: RefCell<Vec<u64>>,
}

impl Sender for FakeSender {
    async fn wait(&self, seconds: u64) {
        self.log.borrow_mut().push(format!("wait {seconds}"));
        self.waits.borrow_mut().push(seconds);
    }

    async fn send(&self, target: &Target, text: &str) -> Outcome {
        self.log.borrow_mut().push(format!("send {} {}", target.chat_id, version_in(text)));
        let next = self
            .script
            .borrow_mut()
            .get_mut(&target.chat_id)
            .and_then(VecDeque::pop_front)
            .unwrap_or(Outcome::Sent);
        match next {
            Outcome::Sent => {
                self.sent.borrow_mut().push((
                    target.chat_id.clone(),
                    target.thread_id.clone(),
                    text.to_string(),
                ));
                Outcome::Sent
            }
            other => other,
        }
    }
}

/// The anchor text, which is the release title as it was actually sent.
fn version_in(text: &str) -> String {
    let Some(start) = text.find("\">") else {
        return String::new();
    };
    text[start + 2..].split("</a>").next().unwrap_or_default().to_string()
}

struct Harness {
    store: FakeStore,
    sender: FakeSender,
    log: Log,
}

impl Harness {
    fn new(seen: &[(&str, &[&str])], behaviour: &[(&str, Outcome)], refuse_put: bool) -> Self {
        let scripted: Vec<(&str, Vec<Outcome>)> =
            behaviour.iter().map(|(chat, outcome)| (*chat, vec![*outcome])).collect();
        let borrowed: Vec<(&str, &[Outcome])> =
            scripted.iter().map(|(chat, outcomes)| (*chat, outcomes.as_slice())).collect();
        Self::scripted(seen, &borrowed, refuse_put)
    }

    fn scripted(seen: &[(&str, &[&str])], script: &[(&str, &[Outcome])], refuse_put: bool) -> Self {
        let log: Log = Rc::new(RefCell::new(Vec::new()));
        Harness {
            store: FakeStore {
                map: RefCell::new(
                    seen.iter()
                        .map(|(k, v)| (k.to_string(), v.iter().map(|s| s.to_string()).collect()))
                        .collect(),
                ),
                log: log.clone(),
                refuse_put,
            },
            sender: FakeSender {
                script: RefCell::new(
                    script
                        .iter()
                        .map(|(chat, outcomes)| (chat.to_string(), outcomes.iter().copied().collect()))
                        .collect(),
                ),
                log: log.clone(),
                sent: RefCell::new(Vec::new()),
                waits: RefCell::new(Vec::new()),
            },
            log,
        }
    }

    fn run(&self) -> Vec<String> {
        self.run_feed(Feed::Releases, MIXED)
    }

    fn run_feed(&self, feed: Feed, xml: &str) -> Vec<String> {
        let targets = parse_targets("111,222:7");
        block_on(run(feed, xml, &targets, &self.store, &self.sender))
    }

    fn record(&self, key: &str) -> Option<Vec<String>> {
        self.store.map.borrow().get(key).cloned()
    }

    fn chats(&self) -> Vec<String> {
        self.sender.sent.borrow().iter().map(|(c, _, _)| c.clone()).collect()
    }
}

#[test]
fn the_api_list_holds_real_releases_newest_published_first() {
    let releases = parse_releases(RELEASES);
    assert_eq!(releases.len(), 10);
    // The reason this endpoint replaced releases.atom: no bare build tags, and
    // the newest published release is first even though its tag is older.
    assert!(releases.iter().all(|r| !r.id.contains("build") && !r.id.contains("dev")));
    assert_eq!(releases[0].id, "v2.4.20");
    assert_eq!(releases[0].title, "Kotlin 2.4.20");
    assert_eq!(releases[0].link, "https://github.com/JetBrains/kotlin/releases/tag/v2.4.20");
}

#[test]
fn a_malformed_or_empty_list_yields_nothing_rather_than_panicking() {
    assert!(parse_releases("[]").is_empty());
    assert!(parse_releases("not json").is_empty());
    assert!(parse_releases("{}").is_empty());
}

#[test]
fn keeps_finals_and_candidates_drops_betas() {
    let kept: Vec<Entry> = parse_releases(RELEASES).into_iter().filter(|r| is_release(&r.id)).collect();
    let tags: Vec<&str> = kept.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(
        tags,
        ["v2.4.20", "v2.4.20-RC3", "v2.4.20-RC2", "v2.4.20-RC", "v2.4.10", "v2.4.10-RC2", "v2.4.10-RC", "v2.4.0"]
    );
    assert!(!tags.iter().any(|t| t.contains("Beta")));

    // The orchestration fixture keeps exactly two, with a beta between them.
    let two: Vec<Entry> = parse_releases(MIXED).into_iter().filter(|r| is_release(&r.id)).collect();
    assert_eq!(two.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["v2.4.20-RC3", "v2.4.10"]);
    assert_eq!(two[0].title, "Kotlin 2.4.20-RC3");
}

#[test]
fn the_tag_filter_agrees_with_how_jetbrains_actually_tags() {
    for tag in ["v2.4.10", "v2.4.20-RC", "v2.4.20-RC3", "v10.0.0"] {
        assert!(is_release(tag), "{tag} should be a release");
    }
    for tag in [
        "v2.4.20-Beta1",
        "build-2.5.0-dev-6883",
        "build-2.4.20-377",
        "v2.4",
        "v2.4.20-RC-fix",
    ] {
        assert!(!is_release(tag), "{tag} should not be a release");
    }
}

#[test]
fn formats_a_message_per_kind_and_escapes_the_title() {
    let release = |tag: &str, title: &str| Entry {
        id: tag.to_string(),
        title: title.to_string(),
        link: "https://example.test/r".to_string(),
    };
    assert!(format_message(Feed::Releases, &release("v2.4.20-RC3", "Kotlin 2.4.20-RC3"))
        .starts_with("<b>Kotlin release candidate</b>"));
    assert!(format_message(Feed::Releases, &release("v2.4.10", "Kotlin 2.4.10")).starts_with("<b>Kotlin release</b>"));
    assert!(format_message(Feed::Releases, &release("v1.0.0", "Kotlin <1> & \"2\""))
        .contains("Kotlin &lt;1&gt; &amp; \"2\""));
}

#[test]
fn parses_every_destination_shape() {
    let targets = parse_targets(" -1001234567890, -1009876543210:42 ,123456789 ");
    assert_eq!(targets.len(), 3);
    assert_eq!(targets[0].chat_id, "-1001234567890");
    assert_eq!(targets[0].thread_id, None);
    assert_eq!(targets[1].key, "-1009876543210:42");
    assert_eq!(targets[1].chat_id, "-1009876543210");
    assert_eq!(targets[1].thread_id.as_deref(), Some("42"));
    assert_eq!(targets[2].chat_id, "123456789");
    assert!(parse_targets("").is_empty());
}

#[test]
fn only_telegrams_own_envelope_counts_as_proof_of_non_delivery() {
    assert_eq!(classify(200, "{\"ok\":true}"), Outcome::Sent);
    assert_eq!(
        classify(403, "{\"ok\":false,\"error_code\":403,\"description\":\"kicked\"}"),
        Outcome::Rejected
    );
    assert_eq!(classify(429, "{\"ok\":false,\"error_code\":429}"), Outcome::Rejected);
    assert_eq!(classify(502, "<html>Bad Gateway</html>"), Outcome::Unknown);
    assert_eq!(classify(500, ""), Outcome::Unknown);
}

#[test]
fn a_rate_limit_carries_telegrams_own_retry_hint() {
    let body = "{\"ok\":false,\"error_code\":429,\"description\":\"Too Many Requests: retry after 10\",\
                \"parameters\":{\"retry_after\":10}}";
    assert_eq!(classify(429, body), Outcome::RetryAfter(10));
    // A 429 without the hint is still a plain refusal.
    assert_eq!(classify(429, "{\"ok\":false,\"error_code\":429}"), Outcome::Rejected);
}

#[test]
fn slow_mode_is_waited_out_inside_the_tick() {
    let h = Harness::scripted(
        &[("seen:release:111", &[]), ("seen:release:222:7", &["v2.4.10", "v2.4.20-RC3"])],
        &[("111", &[Outcome::RetryAfter(10)])],
        false,
    );
    h.run();
    assert_eq!(*h.sender.waits.borrow(), [10]);
    let log = h.log.borrow();
    let for_111: Vec<&String> = log.iter().filter(|l| l.contains("111") || l.starts_with("wait")).collect();
    assert_eq!(
        for_111,
        [
            "put seen:release:111 [\"v2.4.10\"]",
            "send 111 Kotlin 2.4.10",
            "wait 10",
            "send 111 Kotlin 2.4.10",
            "put seen:release:111 [\"v2.4.10\",\"v2.4.20-RC3\"]",
            "send 111 Kotlin 2.4.20-RC3",
        ]
    );
    // Both releases still land, in one tick, exactly once each.
    assert_eq!(
        h.record("seen:release:111"),
        Some(vec!["v2.4.10".to_string(), "v2.4.20-RC3".to_string()])
    );
}

#[test]
fn the_waiting_budget_is_spent_once_and_not_again() {
    // Two destinations, each rate limited for 60s. The first wait fits the
    // budget; the second would push the pass past it, so it is refused and left
    // for the next tick rather than running into it.
    let h = Harness::scripted(
        &[("seen:release:111", &["v2.4.10"]), ("seen:release:222:7", &["v2.4.10"])],
        &[
            ("111", &[Outcome::RetryAfter(60)]),
            ("222", &[Outcome::RetryAfter(60), Outcome::RetryAfter(60)]),
        ],
        false,
    );
    h.run();
    assert_eq!(*h.sender.waits.borrow(), [60], "only one wait fits the budget");
    // The one that waited went out; the one that did not keeps its claim released.
    assert_eq!(h.chats(), ["111"]);
    assert_eq!(h.record("seen:release:222:7"), Some(vec!["v2.4.10".to_string()]));
}

#[test]
fn a_rate_limit_longer_than_the_budget_is_left_for_the_next_tick() {
    let h = Harness::new(
        &[("seen:release:111", &[]), ("seen:release:222:7", &["v2.4.10", "v2.4.20-RC3"])],
        &[("111", Outcome::RetryAfter(3600))],
        false,
    );
    h.run();
    assert!(h.sender.waits.borrow().is_empty(), "an hour must not be waited out");
    assert_eq!(h.record("seen:release:111"), Some(Vec::new()), "the claim is given back");
}

#[test]
fn a_record_round_trips_through_the_stored_form() {
    let tags = vec!["v2.4.10".to_string(), "v2.4.20-RC3".to_string()];
    assert_eq!(encode_record(&tags), "[\"v2.4.10\",\"v2.4.20-RC3\"]");
    assert_eq!(decode_record("[\"v2.4.10\"]").unwrap(), vec!["v2.4.10".to_string()]);
    assert_eq!(decode_record("[]").unwrap(), Vec::<String>::new());
    assert!(decode_record("not json").is_err());
}

#[test]
fn a_destination_with_no_record_is_seeded_silently() {
    let h = Harness::new(&[], &[], false);
    h.run();
    assert!(h.chats().is_empty());
    let expected = vec!["v2.4.20-RC3".to_string(), "v2.4.10".to_string()];
    assert_eq!(h.record("seen:release:111"), Some(expected.clone()));
    assert_eq!(h.record("seen:release:222:7"), Some(expected));
}

#[test]
fn a_new_release_reaches_every_destination_oldest_first_with_the_topic_id() {
    let h = Harness::new(&[("seen:release:111", &["v2.4.10"]), ("seen:release:222:7", &[])], &[], false);
    h.run();
    let sent = h.sender.sent.borrow();
    let addressed: Vec<(&str, Option<&str>)> = sent
        .iter()
        .map(|(c, t, _)| (c.as_str(), t.as_deref()))
        .collect();
    assert_eq!(addressed, [("111", None), ("222", Some("7")), ("222", Some("7"))]);
    assert!(sent[1].2.contains("Kotlin 2.4.10"));
    assert!(sent[2].2.contains("Kotlin 2.4.20-RC3"));
    assert_eq!(
        h.record("seen:release:111"),
        Some(vec!["v2.4.10".to_string(), "v2.4.20-RC3".to_string()])
    );
}

#[test]
fn nothing_is_resent_once_every_destination_is_up_to_date() {
    let seen: &[&str] = &["v2.4.10", "v2.4.20-RC3"];
    let h = Harness::new(&[("seen:release:111", seen), ("seen:release:222:7", seen)], &[], false);
    h.run();
    assert!(h.chats().is_empty());
}

#[test]
fn every_release_is_recorded_before_it_is_sent_never_after() {
    let h = Harness::new(
        &[("seen:release:111", &[]), ("seen:release:222:7", &["v2.4.10", "v2.4.20-RC3"])],
        &[],
        false,
    );
    h.run();
    let log = h.log.borrow();
    let for_111: Vec<&String> = log.iter().filter(|l| l.contains("111")).collect();
    assert_eq!(
        for_111,
        [
            "put seen:release:111 [\"v2.4.10\"]",
            "send 111 Kotlin 2.4.10",
            "put seen:release:111 [\"v2.4.10\",\"v2.4.20-RC3\"]",
            "send 111 Kotlin 2.4.20-RC3",
        ]
    );
}

#[test]
fn a_release_that_cannot_be_claimed_is_never_sent() {
    let h = Harness::new(&[("seen:release:111", &[]), ("seen:release:222:7", &[])], &[], true);
    let logs = h.run();
    assert!(h.chats().is_empty(), "a failed claim must stop the send");
    assert!(logs.iter().all(|l| l.contains("not claimed")), "{logs:?}");
}

#[test]
fn an_unknown_delivery_outcome_is_never_retried() {
    let h = Harness::new(
        &[("seen:release:111", &[]), ("seen:release:222:7", &[])],
        &[("111", Outcome::Unknown)],
        false,
    );
    let logs = h.run();
    // The claim stands even though the send outcome is unknown.
    assert_eq!(h.record("seen:release:111"), Some(vec!["v2.4.10".to_string()]));
    assert!(logs.iter().any(|l| l.contains("SKIPPED")), "{logs:?}");

    // Next tick: the ambiguous release is not sent again.
    let next = Harness::new(&[("seen:release:111", &["v2.4.10"]), ("seen:release:222:7", &[])], &[], false);
    next.run();
    let resent: Vec<String> = next
        .sender
        .sent
        .borrow()
        .iter()
        .filter(|(c, _, _)| c == "111")
        .map(|(_, _, text)| version_in(text))
        .collect();
    assert_eq!(resent, ["Kotlin 2.4.20-RC3"]);
}

#[test]
fn a_release_telegram_provably_refused_is_released_and_retried_next_tick() {
    let h = Harness::new(
        &[("seen:release:111", &[]), ("seen:release:222:7", &[])],
        &[("111", Outcome::Rejected)],
        false,
    );
    h.run();
    assert_eq!(h.chats(), ["222", "222"]);
    assert_eq!(h.record("seen:release:111"), Some(Vec::new()));

    let next = Harness::new(
        &[("seen:release:111", &[]), ("seen:release:222:7", &["v2.4.10", "v2.4.20-RC3"])],
        &[],
        false,
    );
    next.run();
    assert_eq!(next.chats(), ["111", "111"]);
}

#[test]
fn a_failing_destination_neither_blocks_nor_duplicates_the_healthy_one() {
    let h = Harness::new(
        &[("seen:release:111", &[]), ("seen:release:222:7", &[])],
        &[("111", Outcome::Rejected)],
        false,
    );
    h.run();
    assert_eq!(h.chats(), ["222", "222"]);
    assert_eq!(
        h.record("seen:release:222:7"),
        Some(vec!["v2.4.10".to_string(), "v2.4.20-RC3".to_string()])
    );
}

#[test]
fn parses_the_blog_feed_including_cdata_bodies_and_encoded_guids() {
    let entries = parse_rss(BLOG);
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].title, "Kotlin Toolchain 0.12: Multiplatform Library Publishing, Wasm Apps, and More");
    assert!(entries[0].link.starts_with("https://blog.jetbrains.com/kotlin/2026/09/"));
    // The guid is the id, and `&#038;` in it is decoded.
    assert_eq!(entries[0].id, "https://blog.jetbrains.com/?post_type=kotlin&p=736282");
    // Post bodies are CDATA; none of their markup leaks into a title or link.
    assert!(entries.iter().all(|e| !e.title.contains('<') && !e.link.contains(' ')));
    assert_eq!(entries[1].title, "Compose Multiplatform 1.12.0 Released");
}

#[test]
fn a_blog_post_is_labelled_as_a_blog_post() {
    let entry = &parse_rss(BLOG)[1];
    let message = format_message(Feed::Blog, entry);
    assert!(message.starts_with("<b>Kotlin blog</b>"), "{message}");
    assert!(message.contains("Compose Multiplatform 1.12.0 Released"));
    // The release wording never appears for a blog post.
    assert!(!message.contains("Kotlin release"));
}

#[test]
fn the_blog_feed_keeps_records_apart_from_the_releases_feed() {
    // Same destinations, already up to date on releases, brand new to the blog.
    let h = Harness::new(
        &[
            ("seen:release:111", &["v2.4.10", "v2.4.20-RC3"]),
            ("seen:release:222:7", &["v2.4.10", "v2.4.20-RC3"]),
        ],
        &[],
        false,
    );
    h.run_feed(Feed::Blog, BLOG);

    // The blog is seeded silently under its own keys...
    assert!(h.chats().is_empty(), "a first blog tick must not backfill");
    let seeded = h.record("seen:blog:111").expect("blog record written");
    assert_eq!(seeded.len(), 3);
    assert!(seeded[0].starts_with("https://blog.jetbrains.com/?post_type=kotlin"));

    // ...and the releases records are untouched.
    assert_eq!(
        h.record("seen:release:111"),
        Some(vec!["v2.4.10".to_string(), "v2.4.20-RC3".to_string()])
    );
}

#[test]
fn a_new_blog_post_is_sent_to_the_blog_destinations() {
    let older: Vec<String> = parse_rss(BLOG).into_iter().skip(1).map(|e| e.id).collect();
    let seen: Vec<&str> = older.iter().map(String::as_str).collect();
    let h = Harness::new(&[("seen:blog:111", &seen), ("seen:blog:222:7", &seen)], &[], false);
    h.run_feed(Feed::Blog, BLOG);

    // Only the one post missing from each record goes out.
    assert_eq!(h.chats(), ["111", "222"]);
    let sent = h.sender.sent.borrow();
    assert!(sent[0].2.contains("Kotlin Toolchain 0.12"), "{}", sent[0].2);
    assert!(sent[0].2.starts_with("<b>Kotlin blog</b>"));
}
