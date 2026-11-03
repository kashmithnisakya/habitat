//! An agent trying to get out of its home.

use std::fs;

use habitat_attacks::{Arena, Limits, OTHER_AGENT_CANARY, OUTSIDE_CANARY, no_breach};
use habitat_core::backend::RunOutcome;
use habitat_core::run_log::RunEnding;

/// Ways of naming something outside the home. Each is one word, as the agent's commands split on
/// spaces.
fn escaping_paths(arena: &Arena) -> Vec<String> {
    let outside = arena.outside().join("secret.txt").display().to_string();
    let mut paths: Vec<String> = [
        "..",
        "../",
        "../..",
        "../grants.db",
        "../audit.db",
        "../audit.head",
        "../package.sha256",
        "../package/agent.wasm",
        "../package/agent.toml",
        "../runs",
        "../tmp",
        "../../bystander/home/secret.txt",
        "../../../secrets.toml",
        "../../../locks/hostile.lock",
        "a/../../grants.db",
        "./../grants.db",
        "a/./b/../../../grants.db",
        "/etc/passwd",
        "/etc/hosts",
        "//etc/passwd",
        "/",
        "\\",
        "..\\grants.db",
        "..\\..\\bystander\\home\\secret.txt",
        "C:\\Windows\\win.ini",
        "C:/Windows/win.ini",
        "\\\\?\\C:\\Windows\\win.ini",
        "\\\\.\\NUL",
        "NUL",
        "CON",
        "com1.txt",
        "file.txt:stream",
        "file.txt::$DATA",
        "PROGRA~1",
        "name.",
        "%2e%2e/grants.db",
        "..%2fgrants.db",
        "%2e%2e%2fgrants.db",
        // Lookalikes of a dot and a slash, and things that some systems ignore or change.
        "\u{ff0e}\u{ff0e}/grants.db",
        "\u{2024}\u{2024}/grants.db",
        "..\u{2215}grants.db",
        "..\u{2044}grants.db",
        "..\u{ff0f}grants.db",
        "..\u{200b}/grants.db",
        ".\u{200d}./grants.db",
        "\u{202e}bd.stnarg/..",
        "..\u{0}/grants.db",
        "a\u{0}b",
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    paths.push(outside);
    paths
}

#[test]
fn no_way_of_naming_something_outside_the_home_can_read_it() {
    let arena = Arena::new();
    let before = arena.snapshot();
    for path in escaping_paths(&arena) {
        let report = arena.attack(&format!("read {path}"));
        no_breach(arena.check_no_leak(&report));
        assert!(
            report
                .messages
                .iter()
                .all(|message| !message.starts_with("OK")),
            "{path:?}: {:?}",
            report.messages
        );
    }
    no_breach(arena.check_unchanged(&before));
    no_breach(arena.check_host_still_works());
}

#[test]
fn no_way_of_naming_something_outside_the_home_can_change_it() {
    let arena = Arena::new();
    let before = arena.snapshot();
    for path in escaping_paths(&arena) {
        let wrote = arena.attack(&format!("write {path} overwritten"));
        no_breach(arena.check_unchanged(&before));
        assert!(
            wrote
                .messages
                .iter()
                .all(|message| !message.starts_with("OK")),
            "{path:?}: {:?}",
            wrote.messages
        );
        arena.attack(&format!("remove {path}"));
        no_breach(arena.check_unchanged(&before));
    }
    no_breach(arena.check_host_still_works());
}

#[test]
fn no_way_of_naming_something_outside_the_home_can_list_it() {
    let arena = Arena::new();
    let before = arena.snapshot();
    for path in escaping_paths(&arena) {
        let report = arena.attack(&format!("list {path}"));
        assert!(
            report
                .messages
                .iter()
                .all(|message| !message.starts_with("OK")),
            "{path:?}: {:?}",
            report.messages
        );
    }
    no_breach(arena.check_unchanged(&before));
}

fn symlink(target: &std::path::Path, link: &std::path::Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, link).unwrap();
    } else {
        std::os::windows::fs::symlink_file(target, link).unwrap();
    }
}

#[test]
fn links_left_in_the_home_lead_nowhere() {
    let arena = Arena::new();
    let secret = arena.outside().join("secret.txt");
    symlink(&secret, &arena.home().join("file-link"));
    symlink(arena.outside(), &arena.home().join("folder-link"));
    let before = arena.snapshot();

    for attack in [
        "read file-link",
        "read folder-link/secret.txt",
        "list folder-link",
    ] {
        let report = arena.attack(attack);
        no_breach(arena.check_no_leak(&report));
        assert!(
            report.messages[0].starts_with("NO"),
            "{attack}: {:?}",
            report.messages
        );
    }
    arena.attack("write folder-link/new.txt x");
    arena.attack("write file-link overwritten");
    arena.attack("remove folder-link/secret.txt");
    no_breach(arena.check_unchanged(&before));
    assert_eq!(fs::read_to_string(&secret).unwrap(), OUTSIDE_CANARY);
    assert!(!arena.outside().join("new.txt").exists());
    no_breach(arena.check_host_still_works());
}

#[test]
fn a_hard_link_left_in_the_home_cant_be_read_or_changed_through() {
    let arena = Arena::new();
    let secret = arena.outside().join("secret.txt");
    // Temporary folders can be on another drive, and then there's nothing to test.
    if fs::hard_link(&secret, arena.home().join("hard-link")).is_err() {
        return;
    }
    let before = arena.snapshot();
    let report = arena.attack("read hard-link");
    // The link is in its home, so this looks at what it saw, not what's there.
    assert!(
        !report.agent_visible().contains(OUTSIDE_CANARY),
        "{report:?}"
    );
    assert!(
        report.messages[0].starts_with("NO"),
        "{:?}",
        report.messages
    );

    // Replacing the name leaves the other file alone.
    arena.attack("write hard-link overwritten");
    assert_eq!(fs::read_to_string(&secret).unwrap(), OUTSIDE_CANARY);
    no_breach(arena.check_unchanged(&before));
}

#[test]
fn another_agents_files_cant_be_reached() {
    let arena = Arena::new();
    let before = arena.snapshot();
    for attack in [
        "read ../../bystander/home/secret.txt",
        "list ../../bystander/home",
        "write ../../bystander/home/secret.txt changed",
        "remove ../../bystander/home/secret.txt",
    ] {
        let report = arena.attack(attack);
        no_breach(arena.check_no_leak(&report));
    }
    no_breach(arena.check_unchanged(&before));
    let other = arena.root().join("agents/bystander/home/secret.txt");
    assert_eq!(fs::read_to_string(other).unwrap(), OTHER_AGENT_CANARY);
}

#[test]
fn filling_the_disk_stops_at_the_limit_and_nothing_else_is_touched() {
    let arena = Arena::with_limits(Limits {
        disk_mb: 1,
        ..Limits::default()
    });
    let before = arena.snapshot();
    let report = arena.attack("fill-disk");
    assert_eq!(
        report.record.ending,
        Some(RunEnding::Ran(RunOutcome::Completed)),
        "{report:?}"
    );

    let mut used = 0;
    for entry in fs::read_dir(arena.home()).unwrap() {
        used += entry.unwrap().metadata().unwrap().len();
    }
    assert!(used > 0, "it should have got somewhere");
    assert!(used <= 1024 * 1024, "{used} is over the limit");
    no_breach(arena.check_unchanged(&before));

    // A file that wouldn't fit can't be written, and what's there is still good.
    let big = arena.attack("big-write 2097152");
    assert!(big.messages[0].starts_with("NO"), "{:?}", big.messages);
}

#[test]
fn a_big_file_that_fits_is_written_whole() {
    let arena = Arena::new();
    let report = arena.attack("big-write 500000");
    assert!(
        report.messages[0].starts_with("OK"),
        "{:?}",
        report.messages
    );
    assert_eq!(fs::read(arena.home().join("big")).unwrap().len(), 500_000);
}
