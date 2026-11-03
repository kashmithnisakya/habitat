//! URLs and redirects from an agent or a server. What's accepted has to be exactly what was checked.
#![no_main]

use habitat_broker::gateway::{CheckedUrl, check_url, follow};
use habitat_core::manifest::is_host_name;
use libfuzzer_sys::fuzz_target;

fn check(url: &CheckedUrl, text: &str) {
    assert!(is_host_name(&url.host), "{text:?}");
    assert!(url.path.starts_with('/'), "{text:?}");
    assert!(url.path.is_ascii() && !url.path.chars().any(|c| c.is_control() || c == ' ' || c == '#' || c == '\\'), "{text:?}");
    // It says what it said it would, and reading it back changes nothing.
    let shown = url.to_string();
    assert!(shown.starts_with(&format!("https://{}/", url.host)), "{text:?}");
    let again = check_url(&shown).unwrap();
    assert_eq!(&again, url, "{text:?}");
}

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(url) = check_url(text) {
        check(&url, text);
        assert!(text.starts_with("https://"), "{text:?}");
    }
    let base = check_url("https://a.example/one/two?q=1").unwrap();
    if let Ok(next) = follow(&base, text) {
        check(&next, text);
        // Only an absolute https:// URL can lead to another host.
        if next.host != base.host {
            assert!(text.starts_with("https://"), "{text:?}");
        }
    }
});
