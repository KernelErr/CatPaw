//! What a page shows of dates and numbers comes from its configuration,
//! never from the host's time zone or locale: a run prints the same
//! wherever it runs, and sites learn nothing of the machine's settings.

use std::process::Command;
use std::rc::Rc;

use catpaw_bindings_boa::BoaPage;
use catpaw_web::{PageConfig, PageState};
use url::Url;

/// What the page shows of dates, numbers and its locale.
const PROBE: &str = r#"[
  new Date(0).toString(),
  new Date(0).getTimezoneOffset(),
  new Date(2026, 0, 1).getTime(),
  new Intl.DateTimeFormat().resolvedOptions().locale,
  new Intl.NumberFormat().resolvedOptions().locale,
  new Intl.Collator().resolvedOptions().locale,
  (1234567.891).toLocaleString(),
  navigator.language,
].join(" | ")"#;

fn probe(config: PageConfig) -> String {
    let state = Rc::new(PageState::new(
        Url::parse("https://example.test/").unwrap(),
        config,
    ));
    let mut page = BoaPage::new(state).expect("page setup");
    page.eval_to_string(PROBE)
        .unwrap_or_else(|e| format!("THROWN {e}"))
}

fn config() -> PageConfig {
    PageConfig {
        time_origin_unix_ms: Some(1_790_000_000_000.0),
        ..PageConfig::default()
    }
}

/// The variable that marks this test's run in a child process.
const CHILD: &str = "CATPAW_HOST_INDEPENDENCE_CHILD";

#[test]
fn dates_and_numbers_ignore_the_host_time_zone_and_locale() {
    assert_eq!(
        probe(config()),
        "Thu Jan 01 1970 00:00:00 GMT+0000 | 0 | 1767225600000 | en-US | en-US | en-US | 1,234,567.891 | en-US"
    );
    if std::env::var_os(CHILD).is_some() {
        return;
    }
    // Again in a process set to a time zone and a locale far from UTC and
    // American English.
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "dates_and_numbers_ignore_the_host_time_zone_and_locale",
            "--test-threads=1",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("TZ", "Asia/Shanghai")
        .env("LANG", "zh_TW.UTF-8")
        .env("LC_ALL", "zh_TW.UTF-8")
        .env("LANGUAGE", "zh_TW")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "the child ran the test"
    );
}

#[test]
fn the_page_configuration_sets_the_time_zone_and_locale() {
    let shown = probe(PageConfig {
        timezone_offset_minutes: 8 * 60,
        languages: vec!["de-DE".to_string(), "de".to_string()],
        ..config()
    });
    // (Boa resolves `de-DE` to the German data it has.)
    assert_eq!(
        shown,
        "Thu Jan 01 1970 08:00:00 GMT+0800 | -480 | 1767196800000 | de | de | de | 1.234.567,891 | de-DE"
    );
    let shown = probe(PageConfig {
        timezone_offset_minutes: -(4 * 60 + 30),
        ..config()
    });
    assert!(
        shown.starts_with("Wed Dec 31 1969 19:30:00 GMT-0430 | 270 | "),
        "{shown}"
    );
}
