//! The generated `OpenAPI` document — tests O-1 … O-6.
//!
//! ADR-0006 makes this document a build artifact, drift-checked the way `ratatoskr-contracts`
//! drift-checks its `JSON` Schema. These tests are the check, plus the two properties that make the
//! check meaningful: the output is deterministic, and it describes only what the route tree
//! contains.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    reason = "assertions in a test binary"
)]

use std::collections::BTreeSet;

use serde_json::Value;

/// Where the committed document lives.
fn path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the workspace root")
        .join("openapi/openapi.json")
}

/// The document as the generator produces it right now.
fn generated() -> Value {
    let surfaces = vec![platform_public_api::surface(), platform_ingest::surface()];
    platform_api_doc::document(env!("CARGO_PKG_VERSION"), &surfaces).expect("a document")
}

/// Every `$ref` target in the document.
fn refs(value: &Value) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    collect(value, &mut found);
    found
}

fn collect(value: &Value, found: &mut BTreeSet<String>) {
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                if key == "$ref"
                    && let Some(target) = child.as_str()
                {
                    found.insert(target.to_owned());
                }
                collect(child, found);
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect(item, found)),
        _ => {}
    }
}

/// O-1. The committed document is what the routes say it should be.
///
/// The gate. It fails on any change to a path, a status, a description or a payload type that was
/// not regenerated, and the fix is never to edit the file: run
/// `cargo run -p openapic -- generate`.
#[test]
fn the_committed_document_matches_the_routes() {
    let path = path();
    let found = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "{} could not be read: {error}. Run `cargo run -p openapic -- generate`.",
            path.display()
        )
    });
    let mut expected = serde_json::to_string_pretty(&generated()).expect("a rendering");
    expected.push('\n');

    assert_eq!(
        found,
        expected,
        "{} is stale. Run `cargo run -p openapic -- generate` and commit the result.",
        path.display()
    );
}

/// O-2. Two renderings on one commit are identical.
///
/// Without this the gate above is a coin toss. `serde_json::Map` is a `BTreeMap` — `preserve_order`
/// is enabled nowhere in this workspace — and nothing in the document reads a clock, an
/// environment variable or an address, so the output is a function of the route tables alone.
#[test]
fn the_document_is_deterministic() {
    assert_eq!(generated(), generated());
}

/// O-3. No reference dangles, and no component is unreferenced.
///
/// A dangling `$ref` renders as a blank body in every viewer and generates an uncompilable client.
/// An unreferenced component is the other half of the same mistake: a type registered but never
/// used means a route lost its body without anyone noticing.
#[test]
fn every_reference_resolves_and_every_component_is_used() {
    let document = generated();
    let names: BTreeSet<String> = document["components"]["schemas"]
        .as_object()
        .expect("components")
        .keys()
        .cloned()
        .collect();
    let referenced = refs(&document);

    for target in &referenced {
        let name = target
            .strip_prefix("#/components/schemas/")
            .unwrap_or_else(|| panic!("{target} is not a component reference"));
        assert!(names.contains(name), "{target} resolves to nothing");
    }
    for name in &names {
        assert!(
            referenced.contains(&format!("#/components/schemas/{name}")),
            "{name} is registered but no route uses it"
        );
    }
}

/// O-4. Every route both crates serve is described exactly once.
#[test]
fn every_served_route_is_described_once() {
    let document = generated();
    let paths = document["paths"].as_object().expect("paths");

    let mut described = 0_usize;
    let mut operation_ids = BTreeSet::new();
    for item in paths.values() {
        for (_, operation) in item.as_object().expect("a path item") {
            described += 1;
            let id = operation["operationId"].as_str().expect("an operation id");
            assert!(operation_ids.insert(id.to_owned()), "{id} appears twice");
        }
    }

    let served =
        platform_public_api::surface().routes.len() + platform_ingest::surface().routes.len();
    assert_eq!(described, served, "every served route is described");
}

/// O-5. `ARCHITECTURE.md` S15: an internal subject, an internal identifier and the deployment
/// topology never reach a client.
///
/// A generated document cannot publish what the route tree does not contain, which is the security
/// argument ADR-0006 makes for generating it. This test is that argument, checked rather than
/// asserted: a hand-written document would fail it the first time somebody pasted an example in.
#[test]
fn the_document_discloses_no_internal_detail() {
    let rendered = serde_json::to_string(&generated()).expect("a rendering");

    let forbidden = [
        // NATS subjects and stream names.
        "cmd.",
        "evt.",
        "jetstream",
        "ratatoskr_commands",
        "ratatoskr_events",
        // Schema and table names.
        "operations.outbox",
        "operations.inbox",
        "identity.sessions",
        "platform_ingest.",
        "idempotency_records",
        // Topology.
        "postgres",
        "nats://",
        "localhost",
        "127.0.0.1",
    ];
    for needle in forbidden {
        assert!(
            !rendered.to_lowercase().contains(needle),
            "the public document names `{needle}`"
        );
    }

    // No `servers` block either: which host each listener is published on is deployment topology,
    // and a client is configured with its base address rather than told one.
    assert!(
        generated().get("servers").is_none(),
        "the document must not name a server"
    );
}

/// O-6. Every route says which credential it needs, and the two are not conflated.
///
/// A route with no `security` reads as public, and a public route on this surface would be a
/// mistake nobody would see in a diff.
#[test]
fn every_route_names_its_credential() {
    let document = generated();
    let schemes: BTreeSet<String> = document["components"]["securitySchemes"]
        .as_object()
        .expect("securitySchemes")
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        schemes,
        ["sessionBearer".to_owned(), "sourceBearer".to_owned()]
            .into_iter()
            .collect()
    );

    for (path, item) in document["paths"].as_object().expect("paths") {
        for (method, operation) in item.as_object().expect("a path item") {
            let security = operation["security"].as_array().expect("security");
            assert_eq!(security.len(), 1, "{method} {path}");
            let named = security[0].as_object().expect("a requirement");
            assert_eq!(named.len(), 1, "{method} {path}");
            let scheme = named.keys().next().expect("a scheme");
            assert!(schemes.contains(scheme), "{method} {path} names {scheme}");

            let expected = if path.starts_with("/v2/ingest/") {
                "sourceBearer"
            } else {
                "sessionBearer"
            };
            assert_eq!(scheme, expected, "{method} {path}");
        }
    }
}
