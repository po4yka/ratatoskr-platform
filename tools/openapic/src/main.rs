//! `openapic` — the public `OpenAPI` document, generated and drift-checked.
//!
//! ADR-0006: Platform owns the document and generates it from its own routes. This binary is the
//! generator, and it is deliberately the same shape as `contractsc` in `ratatoskr-contracts` — a
//! `generate` that writes and a `check` that fails on any difference — because CI already knows
//! that shape and an operator should not have to learn a second one.
//!
//! ```text
//! cargo run -p openapic -- generate    # write openapi/openapi.json
//! cargo run -p openapic -- check       # exit 1 if it would differ
//! ```
//!
//! The document is a pure function of the route tables: no database, no bus, no listener, no clock
//! and no environment. Two runs on one commit therefore produce identical bytes, which is what
//! makes `check` a gate rather than a coin toss.

use std::path::Path;
use std::process::ExitCode;

/// Where the document lives. A path relative to the workspace root, resolved from
/// `CARGO_MANIFEST_DIR` so the command works from any directory.
const RELATIVE_PATH: &str = "openapi/openapi.json";

/// The `info.version` of the document: this workspace's version.
///
/// Not the API's major version — that is in every path, and in the capability document. This says
/// which build produced these bytes, which is what a client generator records in its own metadata.
const DOCUMENT_VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let Some(command) = std::env::args().nth(1) else {
        return usage();
    };

    let rendered = match render() {
        Ok(rendered) => rendered,
        Err(error) => {
            eprintln!("openapic: the document could not be built: {error}");
            return ExitCode::FAILURE;
        }
    };
    let path = workspace_root().join(RELATIVE_PATH);

    match command.as_str() {
        "generate" => generate(&path, &rendered),
        "check" => check(&path, &rendered),
        _ => usage(),
    }
}

/// Write the document, creating its directory if it is not there yet.
fn generate(path: &Path, rendered: &str) -> ExitCode {
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        eprintln!(
            "openapic: {} could not be created: {error}",
            parent.display()
        );
        return ExitCode::FAILURE;
    }
    match std::fs::write(path, rendered) {
        Ok(()) => {
            eprintln!("openapic: wrote {}", path.display());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("openapic: {} could not be written: {error}", path.display());
            ExitCode::FAILURE
        }
    }
}

/// Fail if the file on disk is not what the routes say it should be.
///
/// The message names the command that fixes it, because the fix is never to edit the file: the
/// routes are the source, and a hand-edit is undone by the next `generate`.
fn check(path: &Path, rendered: &str) -> ExitCode {
    match std::fs::read_to_string(path) {
        Ok(found) if found == rendered => {
            eprintln!("openapic: {} is up to date", path.display());
            ExitCode::SUCCESS
        }
        Ok(_) => {
            eprintln!(
                "openapic: {} does not match the routes it is generated from.\n\
                 Run `cargo run -p openapic -- generate` and commit the result.",
                path.display()
            );
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!(
                "openapic: {} could not be read: {error}\n\
                 Run `cargo run -p openapic -- generate` to create it.",
                path.display()
            );
            ExitCode::FAILURE
        }
    }
}

/// The document, as bytes.
///
/// Two-space pretty printing and exactly one trailing newline — the same rule `contractsc` applies
/// to every artifact it generates, so a diff of either repository's generated output reads the same
/// way.
fn render() -> Result<String, platform_api_doc::DocumentError> {
    let surfaces = vec![platform_public_api::surface(), platform_ingest::surface()];
    let document = platform_api_doc::document(DOCUMENT_VERSION, &surfaces)?;
    let mut rendered = serde_json::to_string_pretty(&document).unwrap_or_default();
    rendered.push('\n');
    Ok(rendered)
}

/// The workspace root, from this crate's manifest directory.
fn workspace_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap_or(Path::new("."))
        .to_path_buf()
}

/// What to type.
fn usage() -> ExitCode {
    eprintln!(
        "usage: openapic <generate|check>\n\
         \n\
           generate   write {RELATIVE_PATH} from the route tables\n\
           check      exit 1 if {RELATIVE_PATH} differs from them"
    );
    ExitCode::FAILURE
}
