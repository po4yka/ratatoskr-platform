//! Session-authenticated library search and read-state resources.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse as _, Response};
use http::header::{CACHE_CONTROL, CONTENT_TYPE};
use http::{HeaderValue, Method, StatusCode};
use platform_api_doc::{
    In, Method as DocMethod, Parameter, Payload, ResponseDoc, RouteDoc, Security,
};
use platform_core::FailureKind;

use crate::{ApiState, Principal};

const DEFAULT_LIMIT: u32 = 25;
const MAX_LIMIT: u32 = 100;
const MAX_QUERY_CHARS: usize = 512;
const MAX_TITLE_CHARS: usize = 256;
const MAX_SNIPPET_CHARS: usize = 512;
const KNOWLEDGE_RESPONSE_BYTES: usize = 256 * 1024;

/// The effective read state of an accepted analysis.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ReadState {
    /// The user has not marked the analysis as read.
    Unread,
    /// The user has marked the analysis as read.
    Read,
}

impl ReadState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Unread => "unread",
            Self::Read => "read",
        }
    }
}

/// Strict public search parameters.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchParams {
    q: Option<String>,
    read_state: Option<ReadState>,
    limit: Option<u32>,
    offset: Option<u64>,
}

/// One minimized public library result.
#[derive(Debug, PartialEq, serde::Serialize, schemars::JsonSchema)]
pub struct LibraryItem {
    /// Accepted Knowledge analysis identity.
    pub analysis_id: uuid::Uuid,
    /// Canonical document identity.
    pub document_id: uuid::Uuid,
    /// A bounded display title.
    pub title: String,
    /// A bounded result excerpt, absent for browse results.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// A finite positive relevance score, absent for browse results.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
    /// The effective user read state.
    pub read_state: ReadState,
}

/// A bounded public page.
#[derive(Debug, PartialEq, serde::Serialize, schemars::JsonSchema)]
pub struct LibraryPage {
    /// Ordered result summaries.
    pub items: Vec<LibraryItem>,
    /// Applied page size.
    pub limit: u32,
    /// Applied zero-based offset.
    pub offset: u64,
    /// Whether at least one later result exists.
    pub has_more: bool,
}

/// The complete read-state resource accepted by `PUT`.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplaceReadState {
    /// Replacement state.
    pub read_state: ReadState,
}

/// Authoritative read state returned after replacement.
#[derive(Debug, PartialEq, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadStateResource {
    /// Stored effective state.
    pub read_state: ReadState,
}

#[derive(Debug, serde::Deserialize)]
struct KnowledgePage {
    results: Vec<KnowledgeItem>,
    has_more: bool,
}

#[derive(Debug, serde::Deserialize)]
struct KnowledgeItem {
    analysis_id: uuid::Uuid,
    document_id: uuid::Uuid,
    title: String,
    snippet: Option<String>,
    rank: Option<f32>,
    read_state: ReadState,
}

/// `GET /v1/library/search`.
pub async fn search(
    State(state): State<Arc<ApiState>>,
    principal: Principal,
    params: Result<Query<SearchParams>, QueryRejection>,
) -> Response {
    let Ok(Query(params)) = params else {
        return platform_http::reject(FailureKind::InvalidRequest);
    };
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
    let offset = params.offset.unwrap_or(0);
    if limit == 0
        || limit > MAX_LIMIT
        || offset > i64::MAX as u64
        || params
            .q
            .as_ref()
            .is_some_and(|q| q.chars().count() > MAX_QUERY_CHARS)
    {
        return platform_http::reject(FailureKind::InvalidRequest);
    }
    let client = KnowledgeClient::new(&state);
    match client
        .search(principal.user_id, &params, limit, offset)
        .await
    {
        Ok(page) => {
            let items = match page
                .results
                .into_iter()
                .map(public_item)
                .collect::<Result<Vec<_>, _>>()
            {
                Ok(items) => items,
                Err(kind) => return platform_http::reject(kind),
            };
            no_store(
                Json(LibraryPage {
                    items,
                    limit,
                    offset,
                    has_more: page.has_more,
                })
                .into_response(),
            )
        }
        Err(kind) => platform_http::reject(kind),
    }
}

/// `PUT /v1/library/items/{analysis_id}/read-state`.
pub async fn replace_read_state(
    State(state): State<Arc<ApiState>>,
    principal: Principal,
    Path(analysis_id): Path<uuid::Uuid>,
    body: Result<Json<ReplaceReadState>, JsonRejection>,
) -> Response {
    let Ok(Json(body)) = body else {
        return platform_http::reject(FailureKind::InvalidRequest);
    };
    match KnowledgeClient::new(&state)
        .replace_read_state(principal.user_id, analysis_id, body.read_state)
        .await
    {
        Ok(resource) => no_store(Json(resource).into_response()),
        Err(kind) => platform_http::reject(kind),
    }
}

fn public_item(item: KnowledgeItem) -> Result<LibraryItem, FailureKind> {
    if item
        .rank
        .is_some_and(|rank| !rank.is_finite() || rank <= 0.0)
    {
        return Err(FailureKind::UpstreamInvalidResponse);
    }
    Ok(LibraryItem {
        analysis_id: item.analysis_id,
        document_id: item.document_id,
        title: truncate(item.title, MAX_TITLE_CHARS),
        snippet: item
            .snippet
            .map(|snippet| truncate(snippet, MAX_SNIPPET_CHARS)),
        score: item.rank,
        read_state: item.read_state,
    })
}

fn truncate(mut value: String, max_chars: usize) -> String {
    let Some((byte_index, _)) = value.char_indices().nth(max_chars) else {
        return value;
    };
    value.truncate(byte_index);
    value
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

struct KnowledgeClient<'a> {
    state: &'a ApiState,
}

impl<'a> KnowledgeClient<'a> {
    const fn new(state: &'a ApiState) -> Self {
        Self { state }
    }

    async fn search(
        &self,
        user_id: uuid::Uuid,
        params: &SearchParams,
        limit: u32,
        offset: u64,
    ) -> Result<KnowledgePage, FailureKind> {
        let mut url = self.url("/internal/search")?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("tenant", &format!("user:{user_id}"));
            if let Some(value) = &params.q {
                query.append_pair("q", value);
            }
            if let Some(value) = params.read_state {
                query.append_pair("read_state", value.as_str());
            }
            query.append_pair("limit", &limit.to_string());
            query.append_pair("offset", &offset.to_string());
        }
        let request = hyper::Request::builder()
            .method(Method::GET)
            .uri(url.as_str())
            .body(Body::empty())
            .map_err(|_| FailureKind::UpstreamUnavailable)?;
        self.execute(request, false).await
    }

    async fn replace_read_state(
        &self,
        user_id: uuid::Uuid,
        analysis_id: uuid::Uuid,
        read_state: ReadState,
    ) -> Result<ReadStateResource, FailureKind> {
        let url = self.url("/internal/user-content/command")?;
        let bytes = serde_json::to_vec(&serde_json::json!({
            "operation": "set_read_state",
            "tenant": format!("user:{user_id}"),
            "output_id": analysis_id,
            "read_state": read_state,
        }))
        .map_err(|_| FailureKind::UpstreamInvalidResponse)?;
        let request = hyper::Request::builder()
            .method(Method::POST)
            .uri(url.as_str())
            .header(CONTENT_TYPE, "application/json")
            .body(Body::from(bytes))
            .map_err(|_| FailureKind::UpstreamUnavailable)?;
        self.execute(request, true).await
    }

    fn url(&self, path: &str) -> Result<url::Url, FailureKind> {
        let listener = self
            .state
            .gateway
            .service_listener("knowledge")
            .ok_or(FailureKind::UpstreamUnavailable)?;
        url::Url::parse(&format!("http://{listener}{path}"))
            .map_err(|_| FailureKind::UpstreamUnavailable)
    }

    async fn execute<T: serde::de::DeserializeOwned>(
        &self,
        request: hyper::Request<Body>,
        scoped_not_found: bool,
    ) -> Result<T, FailureKind> {
        let budget = self.state.gateway.control_budget();
        let max_body = usize::try_from(budget.max_body_bytes)
            .unwrap_or(usize::MAX)
            .min(KNOWLEDGE_RESPONSE_BYTES);
        tokio::time::timeout(
            Duration::from_secs(budget.response_timeout_seconds),
            async {
                let response = self.state.gateway.request_control(request).await?;
                if scoped_not_found && response.status() == StatusCode::NOT_FOUND {
                    return Err(FailureKind::NotFound);
                }
                if !response.status().is_success() {
                    tracing::warn!(
                        dependency = "knowledge",
                        class = "invalid_status",
                        "typed dependency returned an unusable status"
                    );
                    return Err(FailureKind::UpstreamInvalidResponse);
                }
                let body = axum::body::to_bytes(Body::new(response.into_body()), max_body)
                    .await
                    .map_err(|_| {
                        tracing::warn!(
                            dependency = "knowledge",
                            class = "oversized_body",
                            "typed dependency response exceeded its bound"
                        );
                        FailureKind::UpstreamInvalidResponse
                    })?;
                serde_json::from_slice(&body).map_err(|_| {
                    tracing::warn!(
                        dependency = "knowledge",
                        class = "invalid_json",
                        "typed dependency returned an invalid success body"
                    );
                    FailureKind::UpstreamInvalidResponse
                })
            },
        )
        .await
        .map_err(|_| {
            tracing::warn!(
                dependency = "knowledge",
                class = "total_timeout",
                "typed dependency total deadline elapsed"
            );
            FailureKind::UpstreamTimeout
        })?
    }
}

const SEARCH_PARAMETERS: &[Parameter] = &[
    Parameter {
        name: "q",
        location: In::Query,
        required: false,
        format: None,
        description: "Optional search text, at most 512 Unicode scalar values.",
    },
    Parameter {
        name: "read_state",
        location: In::Query,
        required: false,
        format: None,
        description: "Optional effective state: read or unread.",
    },
    Parameter {
        name: "limit",
        location: In::Query,
        required: false,
        format: Some("uint32"),
        description: "Page size from 1 through 100; defaults to 25.",
    },
    Parameter {
        name: "offset",
        location: In::Query,
        required: false,
        format: Some("uint64"),
        description: "Non-negative zero-based page offset; defaults to 0.",
    },
];

/// Public search route documentation.
pub const SEARCH_DOC: RouteDoc = RouteDoc {
    method: DocMethod::Get,
    path: "/v1/library/search",
    operation_id: "searchLibrary",
    summary: "Search or browse the caller's library",
    description: "Returns a bounded page owned by the authenticated principal. Tenant identity is derived by Edge and cannot be supplied by the caller.",
    tag: "library",
    security: Security::Session,
    parameters: SEARCH_PARAMETERS,
    request: None,
    responses: &[
        ResponseDoc {
            status: 200,
            description: "A bounded library page.",
            payload: Some(Payload::Json("LibraryPage")),
        },
        ResponseDoc {
            status: 400,
            description: "One or more search parameters are invalid.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 401,
            description: "The session does not authenticate here.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 502,
            description: "Knowledge returned an invalid response.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 503,
            description: "Knowledge is unavailable.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 504,
            description: "Knowledge exceeded its response budget.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};

/// Public read-state route documentation.
pub const READ_STATE_DOC: RouteDoc = RouteDoc {
    method: DocMethod::Put,
    path: "/v1/library/items/{analysis_id}/read-state",
    operation_id: "replaceLibraryReadState",
    summary: "Replace one library item's read state",
    description: "Idempotently replaces the complete read-state resource for a caller-owned accepted analysis.",
    tag: "library",
    security: Security::Session,
    parameters: &[Parameter {
        name: "analysis_id",
        location: In::Path,
        required: true,
        format: Some("uuid"),
        description: "Opaque accepted analysis identity.",
    }],
    request: Some(Payload::Json("ReplaceReadState")),
    responses: &[
        ResponseDoc {
            status: 200,
            description: "The authoritative effective state.",
            payload: Some(Payload::Json("ReadStateResource")),
        },
        ResponseDoc {
            status: 400,
            description: "The identifier or exact body is invalid.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 401,
            description: "The session does not authenticate here.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 404,
            description: "The item is absent or belongs to another principal.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 502,
            description: "Knowledge returned an invalid response.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 503,
            description: "Knowledge is unavailable.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 504,
            description: "Knowledge exceeded its response budget.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};
