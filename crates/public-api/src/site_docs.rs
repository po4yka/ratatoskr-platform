//! The bodies and the description of the routes of the site owner session (workspace change
//! `add-site-owner-editing-and-live`, tasks 8.6 and 8.7). The site SDK and the web client generate
//! their types from the description that these entries produce.

use platform_api_doc::{In, Method, Parameter, Payload, ResponseDoc, RouteDoc, Security};
use ratatoskr_identifiers::BlobRef;

/// The source of one note in the site's publications, for the owner session of the site
/// (`GET .../drafts/pages/{slug}` and `GET .../drafts/sources/{source_ref}`).
#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SiteDraft {
    /// The opaque reference of the note. Only a site owner session sees it; it names no vault path.
    pub source_ref: String,
    /// The lowercase hex SHA-256 of the note's current bytes: the `base_hash` of the next edit.
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub revision: String,
    /// The slug of the note in the current release; absent when the note is not published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    /// The Markdown source of the note.
    pub markdown: String,
}

/// The answer of `POST .../drafts/sources/{source_ref}/blobs`: the stored content of an edit.
#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SiteBlobUploaded {
    /// The stored content, owned by `ratatoskr-notes`.
    pub content_blob: BlobRef,
    /// The lowercase hex SHA-256 of the content.
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub content_hash: String,
}

/// What the owner session sends to `POST /v1/sites/{site}/edits`: the content names a stored blob,
/// never inline text.
#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SiteEditRequest {
    /// The note to change.
    pub source_ref: String,
    /// The lowercase hex SHA-256 of the revision that the owner read and changed.
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub base_hash: String,
    /// The new content, uploaded first to `POST .../drafts/sources/{source_ref}/blobs`.
    pub content_blob: BlobRef,
    /// The lowercase hex SHA-256 of the new content.
    #[schemars(extend("pattern" = "^[0-9a-f]{64}$"))]
    pub content_hash: String,
    /// A client-chosen key, 1 to 255 characters: a repeated edit with the same key answers the
    /// same operation and writes nothing more.
    pub idempotency_key: String,
}

/// The answer of `POST /v1/sites/{site}/edits`: an operation to follow, never a result.
#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct SiteEditAccepted {
    /// The operation of the write. The site's event stream sends `edit.completed` or
    /// `edit.conflicted` with this id.
    pub operation_id: uuid::Uuid,
    /// Always `accepted` here.
    pub status: String,
}

/// The texts of a conflict of a Markdown or plain-text note, when each is at most 1 MiB.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SiteConflictTexts {
    /// The revision that the edit read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// The revision that the vault holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    /// The content of the edit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer: Option<String>,
    /// The merge proposal of `ratatoskr-notes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged: Option<String>,
}

/// The merge proposal of a conflict.
#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SiteMergeProposal {
    /// The lowercase hex SHA-256 of the proposed merge.
    pub merged_hash: String,
    /// Whether the two changes overlap.
    pub has_overlapping_hunks: bool,
}

/// The answer of `GET /v1/sites/{site}/conflicts/{conflict_id}`: one conflict of a note in the
/// site's publications, with no vault path and no connection.
#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SiteConflict {
    /// The conflict.
    pub conflict_id: uuid::Uuid,
    /// The operation of the edit that met it.
    pub operation_id: uuid::Uuid,
    /// The note of the conflict.
    pub source_ref: String,
    /// `open` or `resolved`.
    pub state: String,
    /// The texts of the sides and of the proposal.
    pub texts: SiteConflictTexts,
    /// The sides that are larger than 1 MiB and so are not in `texts`.
    pub texts_over_limit: Vec<String>,
    /// The hash of the revision that the edit read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_hash: Option<String>,
    /// The hash of the revision that the vault holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_hash: Option<String>,
    /// The hash of the content of the edit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer_hash: Option<String>,
    /// The merge proposal, when `ratatoskr-notes` made one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_proposal: Option<SiteMergeProposal>,
}

const fn response(status: u16, description: &'static str, payload: &'static str) -> ResponseDoc {
    ResponseDoc {
        status,
        description,
        payload: Some(Payload::Json(payload)),
    }
}

const fn path(name: &'static str, description: &'static str) -> Parameter {
    Parameter {
        name,
        location: In::Path,
        required: true,
        format: None,
        description,
    }
}

const SITE: Parameter = path(
    "site",
    "The site id: 3 to 40 lower-case letters, digits and `-`.",
);
const SLUG: Parameter = path("slug", "The slug of a page of the site's current release.");
const SOURCE_REF: Parameter = path("source_ref", "The opaque reference of a note of the site.");
const CONFLICT: Parameter = Parameter {
    name: "conflict_id",
    location: In::Path,
    required: true,
    format: Some("uuid"),
    description: "The conflict that the event stream named in `edit.conflicted`.",
};

/// The refusals that every route of the site owner session shares.
const OWNER_REFUSALS: [ResponseDoc; 4] = [
    response(
        401,
        "No site owner session of this site, in its cookie.",
        "ErrorEnvelope",
    ),
    response(
        403,
        "The site's effective capability set lacks the route's capability \
         (`edge.capability_not_granted`), or an unsafe request lacks `X-Ratatoskr-Site-Request: 1` \
         or the `Origin` of the approved hostname.",
        "ErrorEnvelope",
    ),
    response(
        404,
        "The note, the page or the conflict is not in the site's publications.",
        "ErrorEnvelope",
    ),
    response(
        503,
        "The effective capability set or the service behind the route could not be read; \
         retryable.",
        "ErrorEnvelope",
    ),
];

/// `POST /v1/sites/{site}/sign-in-codes`.
pub const SIGN_IN_CODE_DOC: RouteDoc = RouteDoc {
    method: Method::Post,
    path: "/v1/sites/{site}/sign-in-codes",
    operation_id: "requestSiteSignInCode",
    summary: "Approve a sign-in of the owner on one site",
    description: "\
The owner's primary session approves a sign-in on an approved site: the body names the PKCE S256 \
challenge of the site's verifier and the path to return to. The answer carries a single-use code \
that expires after 60 seconds, and the address of the site's callback page with the code. The \
answer is not stored and carries `Cache-Control: no-store` and `Referrer-Policy: no-referrer`. \
Class `management` (ADR-0017).",
    tag: "sites",
    security: Security::Session,
    parameters: &[SITE],
    request: Some(Payload::Json("SignInCodeRequest")),
    responses: &[
        response(
            201,
            "The code and the callback address.",
            "SignInCodeIssued",
        ),
        response(
            400,
            "The body is not a challenge and a path.",
            "ErrorEnvelope",
        ),
        response(
            401,
            "No credential, or one that does not authenticate.",
            "ErrorEnvelope",
        ),
        response(
            403,
            "Not a primary session with the live `platform.owner` grant.",
            "ErrorEnvelope",
        ),
        response(
            404,
            "`sites.site_not_found`: the site has no approved manifest.",
            "ErrorEnvelope",
        ),
    ],
};

/// `POST /v1/sites/{site}/sessions`.
pub const SITE_SESSION_DOC: RouteDoc = RouteDoc {
    method: Method::Post,
    path: "/v1/sites/{site}/sessions",
    operation_id: "openSiteSession",
    summary: "Redeem a sign-in code on the site",
    description: "\
The site's callback page redeems the code with its PKCE verifier, from the approved hostname. The \
answer sets the host-only cookie `__Host-ratatoskr-site` of a site owner session (`HttpOnly`, \
`Secure`, `SameSite=Strict`, `Path=/`), which ends 12 hours after issue or 1 hour after its last \
use. Every invalid code is the same `401`. Class `sign-in` (ADR-0017).",
    tag: "sites",
    security: Security::None,
    parameters: &[SITE],
    request: Some(Payload::Json("SiteSessionRequest")),
    responses: &[
        response(201, "The session is in the cookie.", "SiteSessionOpened"),
        response(
            400,
            "The body is not a code and a verifier.",
            "ErrorEnvelope",
        ),
        response(401, "The code was not accepted.", "ErrorEnvelope"),
    ],
};

/// `GET /v1/sites/{site}/drafts/pages/{slug}`.
pub const DRAFT_PAGE_DOC: RouteDoc = RouteDoc {
    method: Method::Get,
    path: "/v1/sites/{site}/drafts/pages/{slug}",
    operation_id: "readSiteDraftPage",
    summary: "Read the source of a published page",
    description: "\
The Markdown source, the reference and the current revision of the note behind a page of the \
current release. Capability `draft.read`, for the site owner session of the site.",
    tag: "sites",
    security: Security::SiteSession,
    parameters: &[SITE, SLUG],
    request: None,
    responses: &[
        response(200, "The source of the note.", "SiteDraft"),
        OWNER_REFUSALS[0],
        OWNER_REFUSALS[1],
        OWNER_REFUSALS[2],
        OWNER_REFUSALS[3],
    ],
};

/// `GET /v1/sites/{site}/drafts/sources/{source_ref}`.
pub const DRAFT_SOURCE_DOC: RouteDoc = RouteDoc {
    method: Method::Get,
    path: "/v1/sites/{site}/drafts/sources/{source_ref}",
    operation_id: "readSiteDraftSource",
    summary: "Read the source of a note of the site",
    description: "\
The Markdown source and the current revision of a note of the site's publications, published or \
not. Capability `draft.read`, for the site owner session of the site.",
    tag: "sites",
    security: Security::SiteSession,
    parameters: &[SITE, SOURCE_REF],
    request: None,
    responses: &[
        response(200, "The source of the note.", "SiteDraft"),
        OWNER_REFUSALS[0],
        OWNER_REFUSALS[1],
        OWNER_REFUSALS[2],
        OWNER_REFUSALS[3],
    ],
};

/// `POST /v1/sites/{site}/drafts/sources/{source_ref}/blobs`.
pub const DRAFT_BLOB_DOC: RouteDoc = RouteDoc {
    method: Method::Post,
    path: "/v1/sites/{site}/drafts/sources/{source_ref}/blobs",
    operation_id: "uploadSiteEditContent",
    summary: "Upload the new content of a note",
    description: "\
The body is the new content of the note, at most 16 MiB. Edge stores it in `ratatoskr-notes` for the \
connection of the note and keeps none of it; the answer names the stored blob for the edit. \
Capability `notes.edit`, budget class `transfer`, for the site owner session of the site, with \
`X-Ratatoskr-Site-Request: 1` and the approved `Origin`.",
    tag: "sites",
    security: Security::SiteSession,
    parameters: &[SITE, SOURCE_REF],
    request: Some(Payload::Binary),
    responses: &[
        response(201, "The stored content.", "SiteBlobUploaded"),
        response(400, "An empty body.", "ErrorEnvelope"),
        OWNER_REFUSALS[0],
        OWNER_REFUSALS[1],
        OWNER_REFUSALS[2],
        response(413, "The content is larger than 16 MiB.", "ErrorEnvelope"),
        OWNER_REFUSALS[3],
    ],
};

/// `POST /v1/sites/{site}/edits`.
pub const EDIT_DOC: RouteDoc = RouteDoc {
    method: Method::Post,
    path: "/v1/sites/{site}/edits",
    operation_id: "editSiteNote",
    summary: "Change the content of one note",
    description: "\
Changes the content of one existing note of the site's publications to an uploaded blob, if the \
note is still at `base_hash`. The write is pending until the vault has it: the site's event stream \
sends `edit.completed` with the new revision, or `edit.conflicted` when the note changed in the \
meantime. A repeated request with the same `idempotency_key` answers the same operation. \
Capability `notes.edit`, for the site owner session of the site, with \
`X-Ratatoskr-Site-Request: 1` and the approved `Origin`.",
    tag: "sites",
    security: Security::SiteSession,
    parameters: &[SITE],
    request: Some(Payload::Json("SiteEditRequest")),
    responses: &[
        response(202, "Accepted; follow the operation.", "SiteEditAccepted"),
        response(400, "The body is not an edit.", "ErrorEnvelope"),
        OWNER_REFUSALS[0],
        OWNER_REFUSALS[1],
        OWNER_REFUSALS[2],
        response(
            409,
            "The `idempotency_key` names another edit.",
            "ErrorEnvelope",
        ),
        OWNER_REFUSALS[3],
    ],
};

/// `GET /v1/sites/{site}/conflicts/{conflict_id}`.
pub const CONFLICT_DOC: RouteDoc = RouteDoc {
    method: Method::Get,
    path: "/v1/sites/{site}/conflicts/{conflict_id}",
    operation_id: "readSiteConflict",
    summary: "Read a conflict of an edit",
    description: "\
The base, current and proposed versions of a conflict of a note of the site's publications, and \
the merge proposal. The owner resolves it with a new edit whose `base_hash` is the current \
revision. Capability `draft.read`, for the site owner session of the site.",
    tag: "sites",
    security: Security::SiteSession,
    parameters: &[SITE, CONFLICT],
    request: None,
    responses: &[
        response(200, "The conflict.", "SiteConflict"),
        OWNER_REFUSALS[0],
        OWNER_REFUSALS[1],
        OWNER_REFUSALS[2],
        OWNER_REFUSALS[3],
    ],
};

/// `GET /v1/sites/{site}/library`.
pub const LIBRARY_DOC: RouteDoc = RouteDoc {
    method: Method::Get,
    path: "/v1/sites/{site}/library",
    operation_id: "readSiteLibrary",
    summary: "Read the owner's library from the site",
    description: "\
The same items, bounds and read state as `GET /v1/library/search`, for the user of the site owner \
session; that user is the only tenant, and a tenant parameter is refused. Capability \
`library.read`, for the site owner session of the site.",
    tag: "sites",
    security: Security::SiteSession,
    parameters: crate::library::SEARCH_PARAMETERS,
    request: None,
    responses: &[
        response(200, "One page of the owner's library.", "LibraryPage"),
        response(
            400,
            "A parameter is invalid, or a tenant is named.",
            "ErrorEnvelope",
        ),
        OWNER_REFUSALS[0],
        OWNER_REFUSALS[1],
        OWNER_REFUSALS[3],
    ],
};

/// `GET /v1/sites/{site}/knowledge/search`.
pub const KNOWLEDGE_DOC: RouteDoc = RouteDoc {
    method: Method::Get,
    path: "/v1/sites/{site}/knowledge/search",
    operation_id: "searchSiteKnowledge",
    summary: "Search the owner's knowledge from the site",
    description: "\
Searches only what the user of the site owner session owns, with the bounds of \
`GET /v1/library/search`: a query of at most 512 Unicode scalar values and a `limit` from 1 \
through 100. A tenant parameter is refused. Capability `knowledge.search`, for the site owner \
session of the site.",
    tag: "sites",
    security: Security::SiteSession,
    parameters: crate::library::SEARCH_PARAMETERS,
    request: None,
    responses: &[
        response(200, "One page of the owner's items.", "LibraryPage"),
        response(
            400,
            "A parameter is invalid, or a tenant is named.",
            "ErrorEnvelope",
        ),
        OWNER_REFUSALS[0],
        OWNER_REFUSALS[1],
        OWNER_REFUSALS[3],
    ],
};

/// Every route of this module, for the description.
pub const DOCS: [RouteDoc; 9] = [
    SIGN_IN_CODE_DOC,
    SITE_SESSION_DOC,
    DRAFT_PAGE_DOC,
    DRAFT_SOURCE_DOC,
    DRAFT_BLOB_DOC,
    EDIT_DOC,
    CONFLICT_DOC,
    LIBRARY_DOC,
    KNOWLEDGE_DOC,
];

/// Put the bodies of the routes in the description.
pub(crate) fn register(generator: &mut schemars::SchemaGenerator) {
    generator.subschema_for::<crate::site_sessions::SignInCodeRequest>();
    generator.subschema_for::<crate::site_sessions::SignInCodeIssued>();
    generator.subschema_for::<crate::site_sessions::SiteSessionRequest>();
    generator.subschema_for::<crate::site_sessions::SiteSessionOpened>();
    generator.subschema_for::<SiteDraft>();
    generator.subschema_for::<SiteBlobUploaded>();
    generator.subschema_for::<SiteEditRequest>();
    generator.subschema_for::<SiteEditAccepted>();
    generator.subschema_for::<SiteConflict>();
}
