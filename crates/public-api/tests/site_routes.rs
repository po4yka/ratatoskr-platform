//! The request and response bodies of the site routes in the generated `openapi/openapi.json`
//! (workspace change `add-site-owner-editing-and-live`, tasks 8.6 and 8.7, which were tasks 3.5 and
//! 3.6 of the contracts group). The site SDK and the web client generate their types from this
//! document, so each body is checked here against a sample that a client sends or receives.
//!
//! The check is a small structural validator: the type of each value, `required` members, and no
//! member that an object with `additionalProperties: false` does not declare.

#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "assertions in a test binary"
)]

use serde_json::{Value, json};

fn document() -> Value {
    serde_json::from_str(include_str!("../../../openapi/openapi.json"))
        .expect("the committed OpenAPI document parses")
}

/// The schema that `reference` (`#/components/schemas/<name>`) names.
fn resolve<'a>(document: &'a Value, schema: &'a Value) -> &'a Value {
    match schema.get("$ref").and_then(Value::as_str) {
        Some(reference) => {
            let name = reference
                .strip_prefix("#/components/schemas/")
                .unwrap_or_else(|| panic!("an unexpected reference {reference}"));
            let found = &document["components"]["schemas"][name];
            assert!(!found.is_null(), "the component {name} exists");
            resolve(document, found)
        }
        None => schema,
    }
}

/// Whether `value` conforms to `schema`, structurally. `Err` names the first violation.
fn conforms(document: &Value, schema: &Value, value: &Value, at: &str) -> Result<(), String> {
    let schema = resolve(document, schema);
    for key in ["anyOf", "oneOf"] {
        if let Some(options) = schema.get(key).and_then(Value::as_array) {
            return if options
                .iter()
                .any(|option| conforms(document, option, value, at).is_ok())
            {
                Ok(())
            } else {
                Err(format!("{at}: no option of {key} matches {value}"))
            };
        }
    }
    if let Some(all) = schema.get("allOf").and_then(Value::as_array) {
        for part in all {
            conforms(document, part, value, at)?;
        }
    }
    let types: Vec<&str> = match schema.get("type") {
        Some(Value::String(one)) => vec![one.as_str()],
        Some(Value::Array(many)) => many.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    let kind = match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_u64() || number.is_i64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    };
    let accepted = types.contains(&kind) || (kind == "integer" && types.contains(&"number"));
    if !types.is_empty() && !accepted {
        return Err(format!("{at}: {kind} where {types:?} is required"));
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        return Err(format!("{at}: {value} is not one of {allowed:?}"));
    }
    match value {
        Value::Object(members) => {
            let properties = schema.get("properties").and_then(Value::as_object);
            for required in schema
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                if !members.contains_key(required) {
                    return Err(format!("{at}: the required member {required} is missing"));
                }
            }
            let closed = schema.get("additionalProperties") == Some(&Value::Bool(false));
            for (name, member) in members {
                match properties.and_then(|properties| properties.get(name)) {
                    Some(property) => {
                        conforms(document, property, member, &format!("{at}.{name}"))?;
                    }
                    None if closed => {
                        return Err(format!("{at}: the member {name} is not declared"));
                    }
                    None => {}
                }
            }
            Ok(())
        }
        Value::Array(items) => {
            if let Some(item) = schema.get("items") {
                for (index, member) in items.iter().enumerate() {
                    conforms(document, item, member, &format!("{at}[{index}]"))?;
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// The operation of `method` at `path`.
fn operation<'a>(document: &'a Value, method: &str, path: &str) -> &'a Value {
    let found = &document["paths"][path][method];
    assert!(
        !found.is_null(),
        "{} {path} is not in the description",
        method.to_ascii_uppercase()
    );
    found
}

fn request_schema(operation: &Value) -> &Value {
    &operation["requestBody"]["content"]["application/json"]["schema"]
}

fn response_schema<'a>(operation: &'a Value, status: &str) -> &'a Value {
    &operation["responses"][status]["content"]["application/json"]["schema"]
}

fn blob_ref() -> Value {
    json!({
        "owner_service": "ratatoskr-notes",
        "digest": {"algorithm": "sha256", "hex": "c".repeat(64)},
        "media_type": "text/markdown",
        "length_bytes": 120,
    })
}

fn edit() -> Value {
    json!({
        "source_ref": "src_garden_start",
        "base_hash": "b".repeat(64),
        "content_blob": blob_ref(),
        "content_hash": "c".repeat(64),
        "idempotency_key": "edit-0001",
    })
}

/// One site route of the description: (method, path, request sample, (status, response
/// sample)).
type Route = (
    &'static str,
    &'static str,
    Option<Value>,
    (&'static str, Value),
);

/// The site routes and a body that a client sends and receives for each.
fn routes() -> Vec<Route> {
    let draft = json!({
        "source_ref": "src_garden_start",
        "revision": "a".repeat(64),
        "slug": "start",
        "markdown": "# Start\n",
    });
    // (method, path, request sample, (status, response sample))
    vec![
        (
            "post",
            "/v1/sites/{site}/sign-in-codes",
            Some(json!({"challenge": "A".repeat(43), "return_path": "/notes/start"})),
            (
                "201",
                json!({
                    "code": "a-code",
                    "redirect_url": "https://garden.example.org/.ratatoskr/sign-in/?code=a-code",
                    "expires_at": "2026-10-15T08:00:00Z",
                }),
            ),
        ),
        (
            "post",
            "/v1/sites/{site}/sessions",
            Some(json!({"code": "a-code", "verifier": "a-verifier"})),
            (
                "201",
                json!({"return_path": "/notes/start", "expires_at": "2026-10-15T20:00:00Z"}),
            ),
        ),
        (
            "get",
            "/v1/sites/{site}/drafts/pages/{slug}",
            None,
            ("200", draft.clone()),
        ),
        (
            "get",
            "/v1/sites/{site}/drafts/sources/{source_ref}",
            None,
            ("200", draft),
        ),
        (
            "post",
            "/v1/sites/{site}/drafts/sources/{source_ref}/blobs",
            None,
            (
                "201",
                json!({"content_blob": blob_ref(), "content_hash": "c".repeat(64)}),
            ),
        ),
        (
            "post",
            "/v1/sites/{site}/edits",
            Some(edit()),
            (
                "202",
                json!({"operation_id": "01a018ae-b4e5-7f90-a17f-1e60c8ce61be", "status": "accepted"}),
            ),
        ),
        (
            "get",
            "/v1/sites/{site}/conflicts/{conflict_id}",
            None,
            (
                "200",
                json!({
                    "conflict_id": "01a018ae-b4e5-7f90-a17f-1e60c8ce61be",
                    "operation_id": "01a018ae-b4e5-7f90-a17f-1e60c8ce61bf",
                    "source_ref": "src_garden_start",
                    "state": "open",
                    "texts": {"base": "a", "current": "b", "writer": "c", "merged": "d"},
                    "texts_over_limit": [],
                    "base_hash": "a".repeat(64),
                    "current_hash": "b".repeat(64),
                    "writer_hash": "c".repeat(64),
                    "merge_proposal": {"merged_hash": "d".repeat(64), "has_overlapping_hunks": false},
                }),
            ),
        ),
        (
            "get",
            "/v1/sites/{site}/library",
            None,
            ("200", json!({"items": []})),
        ),
        (
            "get",
            "/v1/sites/{site}/knowledge/search",
            None,
            ("200", json!({"items": []})),
        ),
    ]
}

/// Workspace task 8.6: the session, draft, blob, edit, conflict, library and knowledge bodies of
/// the site routes are in the description, and a body that a client sends or receives conforms.
#[test]
fn route_schemas_round_trip() {
    let document = document();
    let routes = routes();
    for (method, path, request, (status, response)) in &routes {
        let operation = operation(&document, method, path);
        if let Some(request) = request {
            let schema = request_schema(operation);
            assert!(!schema.is_null(), "{method} {path} names its request body");
            conforms(&document, schema, request, "request")
                .unwrap_or_else(|error| panic!("{method} {path} request: {error}"));
        }
        let schema = response_schema(operation, status);
        assert!(!schema.is_null(), "{method} {path} names its {status} body");
        conforms(&document, schema, response, "response")
            .unwrap_or_else(|error| panic!("{method} {path} {status}: {error}"));
    }
    // The site reads the owner's library and search with the item shapes of `/v1/library/search`,
    // and names no tenant.
    let library = &document["paths"]["/v1/library/search"]["get"];
    for path in [
        "/v1/sites/{site}/library",
        "/v1/sites/{site}/knowledge/search",
    ] {
        let operation = operation(&document, "get", path);
        assert_eq!(
            response_schema(operation, "200"),
            response_schema(library, "200"),
            "{path} answers the item schema of /v1/library/search"
        );
        let parameters: Vec<&str> = operation["parameters"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|parameter| parameter["name"].as_str())
            .collect();
        assert!(
            !parameters.contains(&"tenant"),
            "{path} names a tenant: {parameters:?}"
        );
    }
}

/// Workspace task 8.6: an edit names its content by a `BlobRef`; a body with the content inline is
/// refused by the description.
#[test]
fn edit_request_refuses_inline_content() {
    let document = document();
    let operation = operation(&document, "post", "/v1/sites/{site}/edits");
    let schema = request_schema(operation);
    let mut inline = edit();
    inline["content"] = json!("# Start\nnew text\n");
    assert!(
        conforms(&document, schema, &inline, "request").is_err(),
        "an edit with inline content conforms"
    );
    let resolved = resolve(&document, schema);
    let mut required: Vec<&str> = resolved["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    required.sort_unstable();
    assert_eq!(
        required,
        [
            "base_hash",
            "content_blob",
            "content_hash",
            "idempotency_key",
            "source_ref"
        ]
    );
    assert_eq!(
        resolved["properties"]["base_hash"]["pattern"], "^[0-9a-f]{64}$",
        "base_hash is 64 lower-case hex"
    );
}
