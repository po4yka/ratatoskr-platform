//! The binary carries the migrations that are on disk — test M-1.
//!
//! No database. The failure this catches is entirely a build-time one, and it has already happened
//! once: `sqlx::migrate!` emits change tracking for the files it FINDS, so adding `0008_*.sql`
//! invalidated nothing, every already-built artifact kept migrations 1 to 7, and
//! `Database::migrate` reported success having applied a set that was one short. It surfaced as a
//! test in another crate failing only under `cargo test --workspace`, which is the worst possible
//! way to learn it: on the deployment target the same staleness is a process that starts, reports
//! itself ready, and is missing a table.
//!
//! `crates/persistence/build.rs` fixes it by tracking the directory. This checks that the fix works,
//! because a build script that quietly stops doing its job produces exactly the condition it exists
//! to prevent.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

/// M-1. Every `.sql` file in `migrations/` is embedded, and nothing else is.
#[test]
fn the_embedded_migrations_are_the_files_on_disk() {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../migrations");
    let mut on_disk: Vec<i64> = std::fs::read_dir(&directory)
        .expect("migrations/ must exist at the repository root")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        // `sqlx::migrate!` matches the extension case-sensitively, so this does too: a
        // `0009_X.SQL` would be embedded by neither, and a test that accepted it would report a
        // drift that does not exist.
        .filter(|name| {
            std::path::Path::new(name)
                .extension()
                .is_some_and(|extension| extension == "sql")
        })
        .map(|name| {
            // `0007_scheduling.sql` -> 7. The same convention sqlx parses, restated here rather
            // than imported, so the test does not agree with the code by construction.
            let digits: String = name.chars().take_while(char::is_ascii_digit).collect();
            digits
                .parse::<i64>()
                .unwrap_or_else(|_| panic!("{name} does not begin with a version number"))
        })
        .collect();
    on_disk.sort_unstable();

    let embedded = platform_persistence::embedded_migrations();

    assert_eq!(
        embedded, on_disk,
        "the binary carries a different set of migrations than the repository does. If the counts \
         differ by a recently added file, the build did not pick it up — `touch` a file in \
         crates/persistence/src/ and rebuild, then work out why build.rs did not fire",
    );
    assert!(
        !on_disk.is_empty(),
        "reading zero migrations would make this test pass against an empty binary",
    );
}
