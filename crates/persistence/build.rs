//! Rebuild this crate when `migrations/` changes — including when a file is ADDED.
//!
//! `sqlx::migrate!` embeds the directory at compile time, and the tracking it emits is per FILE.
//! A file that does not exist yet cannot be tracked, so adding `0008_*.sql` left every already-built
//! artifact holding migrations 1 to 7 and cargo saw no reason to rebuild. `Database::migrate` then
//! reported success having applied a set that was one migration short.
//!
//! That was observed, not theorised: a test asserting a CHECK constraint added by a new migration
//! passed when its own crate had just been rebuilt and failed under `cargo test --workspace`, where
//! a stale `platform_persistence` was linked. On the deployment target the same staleness is a
//! process that starts, reports itself ready, and is missing a table.
//!
//! `rerun-if-changed` on the DIRECTORY is what covers addition and removal: cargo hashes the
//! directory listing as well as the contents.

fn main() {
    println!("cargo:rerun-if-changed=../../migrations");
}
