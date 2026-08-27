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

/// O-6. Every route says which credential it needs — including the routes that need none.
///
/// An ABSENT `security` key inherits the document default, so a route that forgot to declare one
/// would read as whatever the document says later. An EMPTY array is `OpenAPI`'s way of saying "no
/// security applies here", and the public routes emit it deliberately: each is how an
/// unauthenticated caller becomes authenticated or hands over a grant — the Telegram assertion
/// exchange, the OAuth callback a browser arrives at, device pairing, and the two credential
/// exchanges a device drives with credentials Platform issued it rather than session credentials.
/// Every other route names a scheme, and the schemes are never conflated.
#[test]
fn every_route_names_its_credential() {
    // The routes that are public, named rather than derived: a route becoming public by accident
    // must fail here, and a list is the only thing a diff shows.
    const PUBLIC: [&str; 6] = [
        "/v1/sessions/telegram",
        "/v1/oauth/{provider}/callback",
        "/v1/devices/pair",
        "/v1/sessions/device",
        "/v1/sessions/refresh",
        "/v1/status",
    ];

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

            if PUBLIC.contains(&path.as_str()) {
                assert!(
                    security.is_empty(),
                    "{method} {path} is public and must say so with an empty requirement"
                );
                continue;
            }

            assert_eq!(security.len(), 1, "{method} {path}");
            let named = security[0].as_object().expect("a requirement");
            assert_eq!(named.len(), 1, "{method} {path}");
            let scheme = named.keys().next().expect("a scheme");
            assert!(schemes.contains(scheme), "{method} {path} names {scheme}");

            let expected = if path.starts_with("/v1/ingest/") {
                "sourceBearer"
            } else {
                "sessionBearer"
            };
            assert_eq!(scheme, expected, "{method} {path}");
        }
    }
}

/// O-7. Operational administration and anonymous status have exact security and shared schemas.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one exact OpenAPI inventory keeps security, responses, schemas, and query contracts together"
)]
fn operational_and_status_security_is_exact() {
    const RESPONSES: [(&str, &str); 5] = [
        ("/v1/admin/operations", "OperationInspectionPage"),
        ("/v1/admin/operations/{operation_id}", "OperationSnapshot"),
        ("/v1/admin/schedules", "ScheduleInspectionPage"),
        ("/v1/admin/audit-events", "AuditEventPage"),
        ("/v1/status", "PublicStatusDocument"),
    ];
    const LISTS: [&str; 3] = [
        "/v1/admin/operations",
        "/v1/admin/schedules",
        "/v1/admin/audit-events",
    ];

    let document = generated();
    let paths = document["paths"].as_object().expect("paths");
    let status_security = paths["/v1/status"]["get"]["security"]
        .as_array()
        .expect("GET /v1/status explicitly declares security");
    assert!(
        status_security.is_empty(),
        "GET /v1/status must explicitly opt out of authentication"
    );

    let expected_admin_security = serde_json::json!([{ "sessionBearer": [] }]);
    for (path, item) in paths
        .iter()
        .filter(|(path, _)| path.starts_with("/v1/admin/"))
    {
        let methods = item.as_object().expect("an admin path item");
        assert_eq!(
            methods.keys().collect::<Vec<_>>(),
            ["get"],
            "{path} must expose only the documented GET"
        );
        assert_eq!(
            methods["get"]["security"], expected_admin_security,
            "GET {path} must name only sessionBearer"
        );
    }
    assert_eq!(
        paths
            .keys()
            .filter(|path| path.starts_with("/v1/admin/"))
            .count(),
        4,
        "the operational admin inventory must be exact"
    );

    for (path, schema) in RESPONSES {
        assert_eq!(
            paths[path]["get"]["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
            format!("#/components/schemas/{schema}"),
            "GET {path} must reuse the shared {schema} response"
        );
    }

    let schemas = document["components"]["schemas"]
        .as_object()
        .expect("schemas");
    for schema in [
        "OperationInspectionPage",
        "ScheduleInspectionPage",
        "AuditEventPage",
        "PublicStatusDocument",
    ] {
        assert!(
            schemas.contains_key(schema),
            "missing shared schema {schema}"
        );
    }

    for path in LISTS {
        let parameters = paths[path]["get"]["parameters"]
            .as_array()
            .unwrap_or_else(|| panic!("GET {path} must document its query"));
        let parameter = |name: &str| {
            parameters
                .iter()
                .find(|parameter| parameter["name"] == name)
                .unwrap_or_else(|| panic!("GET {path} is missing query parameter {name}"))
        };
        let limit = parameter("limit");
        assert_eq!(limit["in"], "query", "GET {path} limit");
        assert!(
            limit["description"]
                .as_str()
                .is_some_and(|description| description.contains("1 through 100")),
            "GET {path} must document the 1 through 100 limit bound: {limit}"
        );
        let cursor = parameter("cursor");
        assert_eq!(cursor["in"], "query", "GET {path} cursor");
        assert!(
            cursor["description"]
                .as_str()
                .is_some_and(|description| description.to_lowercase().contains("opaque")),
            "GET {path} must document its cursor as opaque: {cursor}"
        );
    }

    let operation_parameters = paths["/v1/admin/operations"]["get"]["parameters"]
        .as_array()
        .expect("operation list query parameters");
    let operation_names: BTreeSet<&str> = operation_parameters
        .iter()
        .map(|parameter| parameter["name"].as_str().expect("a parameter name"))
        .collect();
    assert_eq!(
        operation_names,
        ["cursor", "kind", "limit", "owner_user_id", "state"]
            .into_iter()
            .collect(),
        "operation inspection filters must be exact"
    );
    for name in ["state", "kind", "owner_user_id"] {
        let description = operation_parameters
            .iter()
            .find(|parameter| parameter["name"] == name)
            .and_then(|parameter| parameter["description"].as_str())
            .unwrap_or_else(|| panic!("operation filter {name} needs a description"));
        assert!(
            description.to_lowercase().contains("exact"),
            "operation filter {name} must document exact matching: {description}"
        );
    }

    let committed: Value = serde_json::from_str(
        &std::fs::read_to_string(path()).expect("the committed OpenAPI document"),
    )
    .expect("the committed OpenAPI document is JSON");
    assert_eq!(
        committed, document,
        "the committed OpenAPI document is stale; regenerate it after the route inventory is exact"
    );
}
