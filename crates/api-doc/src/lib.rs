//! The public `OpenAPI` document, generated from the tables that build the routes.
//!
//! ADR-0006 decided the direction: **Platform owns the public `OpenAPI` document and generates it
//! from its own routes**, while `ratatoskr-contracts` owns the payload types those routes carry.
//! Contracts says what an `OperationSnapshot` is; this document says that
//! `GET /v2/operations/{id}` returns one, under which authentication, with which failures.
//!
//! # Why a crate rather than a `utoipa` annotation
//!
//! The payload types are contract types, and they describe themselves through `schemars`
//! (ADR-0001: Rust-first, schema generated). An annotation framework with its own schema vocabulary
//! would need every one of those types described a second time, in a second language, in this
//! repository — which is exactly the duplication the contracts repository exists to prevent. Here
//! the request and response schemas are produced by the same generator that produces contracts'
//! published JSON Schema, from the same derives.
//!
//! # Why it cannot drift
//!
//! A serving crate exposes ONE table of [`RouteDoc`] values, and builds both its `axum` router and
//! its half of this document from it. There is no second list to keep in step: a route with no
//! description does not compile, and a description with no route has no handler to attach.
//!
//! # Determinism
//!
//! `serde_json::Map` is a `BTreeMap` — `preserve_order` is enabled nowhere in this workspace — so
//! every object in the output is key-sorted regardless of the order members were inserted in. The
//! document is therefore a function of the route tables alone, which is what makes
//! `openapic check` a usable drift gate.

use std::collections::BTreeSet;

use serde_json::{Value, json};

/// What one deployable contributes to the public surface.
#[derive(Debug)]
pub struct ApiSurface {
    /// Every route it serves, in table order.
    pub routes: Vec<RouteDoc>,
    /// Registers the payload types those routes carry with the shared generator.
    ///
    /// A function pointer rather than a list of types, because the types live in the serving crate
    /// and `schemars` registration is generic: a `Vec<TypeId>` cannot express it, and a trait
    /// object would need one implementation per type for no reader's benefit.
    pub register: fn(&mut schemars::SchemaGenerator),
}

/// The HTTP method of a route. Two, because two are served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// `GET`.
    Get,
    /// `POST`.
    Post,
}

impl Method {
    /// The lowercase key `OpenAPI` uses under a path item.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Post => "post",
        }
    }
}

/// Which credential a route accepts.
///
/// Two distinct schemes rather than one shared `bearerAuth`, because they are not interchangeable:
/// a session credential authenticates a person and a source credential authenticates a machine that
/// pushes signals, and a document that called both "bearer" would invite a client to try one where
/// the other is required.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Security {
    /// A session credential minted for this listener's audience.
    Session,
    /// The credential a registered webhook source presents.
    SourceToken,
    /// None: the route is how a caller becomes authenticated, or is reached by a party that has no
    /// credential of ours to present. Spelled rather than omitted, because a route with no
    /// `security` key reads as an oversight and this must read as a decision.
    None,
}

impl Security {
    /// The `components.securitySchemes` key, for the routes that need one.
    #[must_use]
    pub const fn scheme(self) -> Option<&'static str> {
        match self {
            Self::Session => Some("sessionBearer"),
            Self::SourceToken => Some("sourceBearer"),
            Self::None => None,
        }
    }

    /// What the scheme means, for a reader of the document.
    const fn description(self) -> &'static str {
        match self {
            Self::Session => {
                "A session credential, presented as `Authorization: Bearer <credential>`. It must \
                 have been issued for this listener's audience; one issued for another surface \
                 does not authenticate here."
            }
            Self::SourceToken => {
                "The credential issued to a registered webhook source, presented as \
                 `Authorization: Bearer <credential>`. It authenticates the source, not a person."
            }
            Self::None => "",
        }
    }
}

/// Where a parameter is read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum In {
    /// A path segment.
    Path,
    /// A request header.
    Header,
    /// A query-string parameter.
    Query,
}

impl In {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::Header => "header",
            Self::Query => "query",
        }
    }
}

/// One parameter of a route.
#[derive(Debug, Clone, Copy)]
pub struct Parameter {
    /// The path segment or header name.
    pub name: &'static str,
    /// Where it is read from.
    pub location: In,
    /// Whether the route refuses the request without it.
    pub required: bool,
    /// The `format` keyword, when the value is more specific than a string, e.g. `uuid`.
    pub format: Option<&'static str>,
    /// What it means and what the route does with it.
    pub description: &'static str,
}

/// What a request or a response body carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Payload {
    /// `application/json`, described by the named schema component.
    Json(&'static str),
    /// `text/event-stream`. Deliberately unschematised: an SSE body is a framed sequence, and
    /// `OpenAPI` has no vocabulary for one. The event names and their JSON shape are described in
    /// the route's own text, which is the honest place for something the format cannot express.
    EventStream,
}

/// One response a route can produce.
#[derive(Debug, Clone, Copy)]
pub struct ResponseDoc {
    /// The status code.
    pub status: u16,
    /// What it means for the caller — the condition, not a restatement of the code's name.
    pub description: &'static str,
    /// The body, when there is one.
    pub payload: Option<Payload>,
}

/// One route, described once, next to the handler that serves it.
#[derive(Debug, Clone, Copy)]
pub struct RouteDoc {
    /// The method.
    pub method: Method,
    /// The path, with `{name}` placeholders — the same string `axum` is given, because it is the
    /// same syntax and a second spelling could differ.
    pub path: &'static str,
    /// A stable identifier a generated client uses as a method name.
    pub operation_id: &'static str,
    /// One line.
    pub summary: &'static str,
    /// Everything a caller must know that the shapes do not say.
    pub description: &'static str,
    /// The group this route belongs to in a rendered document.
    pub tag: &'static str,
    /// The credential it requires.
    pub security: Security,
    /// Its parameters.
    pub parameters: &'static [Parameter],
    /// Its request body, when it takes one.
    pub request: Option<Payload>,
    /// Every response it can produce.
    pub responses: &'static [ResponseDoc],
}

/// A document that could not be built.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DocumentError {
    /// A route names a schema component that no surface registered. A dangling `$ref` renders as a
    /// blank body in every viewer and generates an uncompilable client, so it fails the build.
    #[error("route {route} refers to schema `{schema}`, which no surface registers")]
    UnknownSchema {
        /// The `operation_id` of the offending route.
        route: &'static str,
        /// The component name it asked for.
        schema: &'static str,
    },

    /// Two routes share an `operation_id`. A generated client would then have two methods with one
    /// name, and which one wins is the generator's business rather than ours.
    #[error("two routes share the operation id `{0}`")]
    DuplicateOperationId(&'static str),
}

/// A generator configured the way this document needs.
///
/// `draft2020_12` rather than `schemars`' own `openapi3` preset: that preset targets `OpenAPI` **3.0**,
/// whose schema object predates JSON Schema and needs `nullable` rewriting. This document is
/// `OpenAPI` 3.1, whose schema object IS JSON Schema 2020-12, so the contract types' published
/// schemas can be used unmodified — which is the point.
///
/// `for_deserialize` matches `ratatoskr-contracts`' published contract: what a consumer must
/// accept, rather than what this build happens to emit.
#[must_use]
pub fn generator() -> schemars::SchemaGenerator {
    schemars::generate::SchemaSettings::draft2020_12()
        .for_deserialize()
        .with(|settings| {
            // Where a `$ref` points. Without this, nested types land in `$defs` inside each
            // component and their refs resolve against the document root, which is a dangling
            // pointer that no viewer reports.
            settings.definitions_path = "/components/schemas".into();
            // A component carries no `$schema`: the dialect is declared once, on the document.
            settings.meta_schema = None;
        })
        .into_generator()
}

/// Build the document from every surface that serves part of the public API.
///
/// # Errors
///
/// [`DocumentError`] when a route refers to a schema nothing registers, or two routes share an
/// operation id.
pub fn document(api_version: &str, surfaces: &[ApiSurface]) -> Result<Value, DocumentError> {
    let mut generator = generator();
    for surface in surfaces {
        (surface.register)(&mut generator);
    }
    let schemas = generator.take_definitions(true);

    let routes: Vec<&RouteDoc> = surfaces.iter().flat_map(|s| s.routes.iter()).collect();
    check(&routes, &schemas)?;

    let mut paths = serde_json::Map::new();
    for route in &routes {
        let item = paths
            .entry(route.path.to_owned())
            .or_insert_with(|| json!({}));
        if let Some(object) = item.as_object_mut() {
            object.insert(route.method.as_str().to_owned(), operation(route));
        }
    }

    let mut security_schemes = serde_json::Map::new();
    for security in routes.iter().map(|route| route.security) {
        if let Some(scheme) = security.scheme() {
            security_schemes.insert(
                scheme.to_owned(),
                json!({
                    "type": "http",
                    "scheme": "bearer",
                    "description": security.description(),
                }),
            );
        }
    }

    Ok(json!({
        "openapi": "3.1.0",
        "jsonSchemaDialect": "https://json-schema.org/draft/2020-12/schema",
        "info": {
            "title": "Ratatoskr Platform public API",
            "version": api_version,
            "summary": "The public control plane: captures, operations, progress and capabilities.",
            "description": DOCUMENT_DESCRIPTION,
            "license": { "name": "BSD-3-Clause" },
        },
        "paths": Value::Object(paths),
        "components": {
            "schemas": Value::Object(schemas),
            "securitySchemes": Value::Object(security_schemes),
        },
    }))
}

/// What a reader of the document must know before reading any route in it.
const DOCUMENT_DESCRIPTION: &str = "\
Generated from the route tables of `ratatoskr-platform`; never hand-edited (ADR-0006). \
The major version is in the path and a new major is a new prefix served alongside the old one, \
never a change in place. It starts at 2 because Ratatoskr Next is the second system to serve this \
surface.\n\n\
Two listeners serve the paths below and they are not the same address: `/v2/captures`, \
`/v2/operations/*` and `/v2/capabilities` are served by `ratatoskr-edge`, and `/v2/ingest/*` by \
`ratatoskr-ingest`. Which host each is published on is deployment topology and is deliberately \
absent from this document, so no `servers` block is emitted.\n\n\
Every non-2xx response carries a contract `ErrorEnvelope`, built at exactly one place in the \
implementation. Its `code` is a closed vocabulary; its `message` is safe to display; it never \
carries a provider diagnostic.\n\n\
Any route may answer `503 platform.limit.overloaded`. It is not listed per route because it is not \
a property of any route: the listener bounds how many requests it holds at once and sheds the rest \
immediately rather than queueing them, so the answer arrives before routing has happened. It is \
retryable, and the condition clears as work drains. `429 platform.limit.rate_exceeded` IS listed \
per route, because it belongs to the caller rather than to the service and only routes that \
identify a caller can produce it.";

/// Fail on the two mistakes that produce a document which looks right and generates a broken
/// client.
fn check(
    routes: &[&RouteDoc],
    schemas: &serde_json::Map<String, Value>,
) -> Result<(), DocumentError> {
    let mut seen = BTreeSet::new();
    for route in routes {
        if !seen.insert(route.operation_id) {
            return Err(DocumentError::DuplicateOperationId(route.operation_id));
        }
        let payloads = route
            .request
            .into_iter()
            .chain(route.responses.iter().filter_map(|r| r.payload));
        for payload in payloads {
            if let Payload::Json(name) = payload
                && !schemas.contains_key(name)
            {
                return Err(DocumentError::UnknownSchema {
                    route: route.operation_id,
                    schema: name,
                });
            }
        }
    }
    Ok(())
}

/// One `OpenAPI` operation object.
fn operation(route: &RouteDoc) -> Value {
    let mut object = serde_json::Map::new();
    object.insert("operationId".to_owned(), json!(route.operation_id));
    object.insert("summary".to_owned(), json!(route.summary));
    object.insert("description".to_owned(), json!(route.description));
    object.insert("tags".to_owned(), json!([route.tag]));
    // An empty array is OpenAPI's way of saying "no security applies here", and it is emitted
    // deliberately: an absent `security` key inherits the document default, so omitting it would
    // make a public route look like whatever the document happens to say later.
    object.insert(
        "security".to_owned(),
        route
            .security
            .scheme()
            .map_or_else(|| json!([]), |scheme| json!([{ scheme: [] }])),
    );

    if !route.parameters.is_empty() {
        let parameters: Vec<Value> = route.parameters.iter().map(parameter).collect();
        object.insert("parameters".to_owned(), Value::Array(parameters));
    }

    if let Some(payload) = route.request {
        object.insert(
            "requestBody".to_owned(),
            json!({ "required": true, "content": content(payload) }),
        );
    }

    let mut responses = serde_json::Map::new();
    for response in route.responses {
        let mut value = json!({ "description": response.description });
        if let (Some(payload), Some(object)) = (response.payload, value.as_object_mut()) {
            object.insert("content".to_owned(), content(payload));
        }
        responses.insert(response.status.to_string(), value);
    }
    object.insert("responses".to_owned(), Value::Object(responses));

    Value::Object(object)
}

/// One `OpenAPI` parameter object.
fn parameter(parameter: &Parameter) -> Value {
    let mut schema = json!({ "type": "string" });
    if let (Some(format), Some(object)) = (parameter.format, schema.as_object_mut()) {
        object.insert("format".to_owned(), json!(format));
    }
    json!({
        "name": parameter.name,
        "in": parameter.location.as_str(),
        "required": parameter.required,
        "description": parameter.description,
        "schema": schema,
    })
}

/// One `OpenAPI` content map.
fn content(payload: Payload) -> Value {
    match payload {
        Payload::Json(name) => json!({
            "application/json": {
                "schema": { "$ref": format!("#/components/schemas/{name}") },
            },
        }),
        Payload::EventStream => json!({
            "text/event-stream": {
                "schema": { "type": "string" },
            },
        }),
    }
}
