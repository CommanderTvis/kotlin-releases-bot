use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use kotlin_releases_bot::*;
use pollster::block_on;

const DEV_ONLY: &str = include_str!("releases.atom");
const MIXED: &str = include_str!("releases-mixed.atom");

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
    behaviour: HashMap<String, Outcome>,
    log: Log,
    sent: RefCell<Vec<(String, Option<String>, String)>>,
}

impl Sender for FakeSender {
    async fn send(&self, target: &Target, text: &str) -> Outcome {
        self.log.borrow_mut().push(format!("send {} {}", target.chat_id, version_in(text)));
        match self.behaviour.get(&target.chat_id).copied().unwrap_or(Outcome::Sent) {
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
                behaviour: behaviour.iter().map(|(k, o)| (k.to_string(), *o)).collect(),
                log: log.clone(),
                sent: RefCell::new(Vec::new()),
            },
            log,
        }
    }

    fn run(&self) -> Vec<String> {
        let targets = parse_targets("111,222:7");
        block_on(run(MIXED, &targets, &self.store, &self.sender))
    }

    fn record(&self, key: &str) -> Option<Vec<String>> {
        self.store.map.borrow().get(key).cloned()
    }

    fn chats(&self) -> Vec<String> {
        self.sender.sent.borrow().iter().map(|(c, _, _)| c.clone()).collect()
    }
}

#[test]
fn parses_the_live_feed_which_is_nothing_but_dev_builds() {
    let releases = parse_feed(DEV_ONLY);
    assert_eq!(releases.len(), 10);
    assert_eq!(releases[0].tag, "build-2.5.0-dev-6883");
    assert!(releases.iter().all(|r| !is_release(&r.tag)));
}

#[test]
fn keeps_finals_and_candidates_drops_betas_and_dev_builds() {
    let kept: Vec<Release> = parse_feed(MIXED).into_iter().filter(|r| is_release(&r.tag)).collect();
    let tags: Vec<&str> = kept.iter().map(|r| r.tag.as_str()).collect();
    assert_eq!(tags, ["v2.4.20-RC3", "v2.4.10"]);
    assert_eq!(kept[0].title, "Kotlin 2.4.20-RC3");
    assert_eq!(kept[0].link, "https://github.com/JetBrains/kotlin/releases/tag/v2.4.20-RC3");
}

#[test]
fn a_feed_holding_a_single_entry_parses_as_one_release() {
    let start = MIXED.find("<entry>").unwrap();
    let end = MIXED.find("</entry>").unwrap() + "</entry>".len();
    let one = format!("{}{}</feed>", &MIXED[..start], &MIXED[start..end]);
    assert_eq!(parse_feed(&one).len(), 1);
}

#[test]
fn entity_encoded_titles_survive_parsing_and_are_escaped_again() {
    let feed = MIXED.replace(
        "<title>Kotlin 2.4.10</title>",
        "<title>Kotlin 2.4.10 &amp; friends</title>",
    );
    let release = parse_feed(&feed).into_iter().find(|r| r.tag == "v2.4.10").unwrap();
    assert_eq!(release.title, "Kotlin 2.4.10 & friends");
    assert!(format_message(&release).contains("Kotlin 2.4.10 &amp; friends"));
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
    let release = |tag: &str, title: &str| Release {
        tag: tag.to_string(),
        title: title.to_string(),
        link: "https://example.test/r".to_string(),
    };
    assert!(format_message(&release("v2.4.20-RC3", "Kotlin 2.4.20-RC3"))
        .starts_with("<b>Kotlin release candidate</b>"));
    assert!(format_message(&release("v2.4.10", "Kotlin 2.4.10")).starts_with("<b>Kotlin release</b>"));
    assert!(format_message(&release("v1.0.0", "Kotlin <1> & \"2\""))
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
    assert_eq!(h.record("seen:111"), Some(expected.clone()));
    assert_eq!(h.record("seen:222:7"), Some(expected));
}

#[test]
fn a_new_release_reaches_every_destination_oldest_first_with_the_topic_id() {
    let h = Harness::new(&[("seen:111", &["v2.4.10"]), ("seen:222:7", &[])], &[], false);
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
        h.record("seen:111"),
        Some(vec!["v2.4.10".to_string(), "v2.4.20-RC3".to_string()])
    );
}

#[test]
fn nothing_is_resent_once_every_destination_is_up_to_date() {
    let seen: &[&str] = &["v2.4.10", "v2.4.20-RC3"];
    let h = Harness::new(&[("seen:111", seen), ("seen:222:7", seen)], &[], false);
    h.run();
    assert!(h.chats().is_empty());
}

#[test]
fn every_release_is_recorded_before_it_is_sent_never_after() {
    let h = Harness::new(
        &[("seen:111", &[]), ("seen:222:7", &["v2.4.10", "v2.4.20-RC3"])],
        &[],
        false,
    );
    h.run();
    let log = h.log.borrow();
    let for_111: Vec<&String> = log.iter().filter(|l| l.contains("111")).collect();
    assert_eq!(
        for_111,
        [
            "put seen:111 [\"v2.4.10\"]",
            "send 111 Kotlin 2.4.10",
            "put seen:111 [\"v2.4.10\",\"v2.4.20-RC3\"]",
            "send 111 Kotlin 2.4.20-RC3",
        ]
    );
}

#[test]
fn a_release_that_cannot_be_claimed_is_never_sent() {
    let h = Harness::new(&[("seen:111", &[]), ("seen:222:7", &[])], &[], true);
    let logs = h.run();
    assert!(h.chats().is_empty(), "a failed claim must stop the send");
    assert!(logs.iter().all(|l| l.contains("not claimed")), "{logs:?}");
}

#[test]
fn an_unknown_delivery_outcome_is_never_retried() {
    let h = Harness::new(
        &[("seen:111", &[]), ("seen:222:7", &[])],
        &[("111", Outcome::Unknown)],
        false,
    );
    let logs = h.run();
    // The claim stands even though the send outcome is unknown.
    assert_eq!(h.record("seen:111"), Some(vec!["v2.4.10".to_string()]));
    assert!(logs.iter().any(|l| l.contains("SKIPPED")), "{logs:?}");

    // Next tick: the ambiguous release is not sent again.
    let next = Harness::new(&[("seen:111", &["v2.4.10"]), ("seen:222:7", &[])], &[], false);
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
        &[("seen:111", &[]), ("seen:222:7", &[])],
        &[("111", Outcome::Rejected)],
        false,
    );
    h.run();
    assert_eq!(h.chats(), ["222", "222"]);
    assert_eq!(h.record("seen:111"), Some(Vec::new()));

    let next = Harness::new(
        &[("seen:111", &[]), ("seen:222:7", &["v2.4.10", "v2.4.20-RC3"])],
        &[],
        false,
    );
    next.run();
    assert_eq!(next.chats(), ["111", "111"]);
}

#[test]
fn a_failing_destination_neither_blocks_nor_duplicates_the_healthy_one() {
    let h = Harness::new(
        &[("seen:111", &[]), ("seen:222:7", &[])],
        &[("111", Outcome::Rejected)],
        false,
    );
    h.run();
    assert_eq!(h.chats(), ["222", "222"]);
    assert_eq!(
        h.record("seen:222:7"),
        Some(vec!["v2.4.10".to_string(), "v2.4.20-RC3".to_string()])
    );
}
