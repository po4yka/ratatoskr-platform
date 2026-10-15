//! `GET /v1/sites` and `GET /v1/sites/{site}/manifest`: the two reads of the web client (workspace
//! change `add-site-publications`, task 6.15).
//!
//! Both are `management` routes (ADR-0017). Edge checks the owner and forwards them to the sites
//! service, which answers them; Edge reads neither answer. The types below are the shapes that the
//! sites service answers, written down here so that the generated `OpenAPI` document states them.

use platform_api_doc::{In, Method, Parameter, Payload, ResponseDoc, RouteDoc, Security};

/// The answer of `GET /v1/sites`: the owner's sites, each with the state of its manifest.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
#[schemars(rename = "SiteList")]
pub struct SiteList {
    /// The sites, in ascending `site_id` order.
    pub sites: Vec<SiteManifestState>,
}

/// One site of [`SiteList`].
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
#[schemars(rename = "SiteManifestState")]
pub struct SiteManifestState {
    /// The site id.
    pub site_id: String,
    /// The SHA-256 of the active manifest, the last valid submission; `null` when the site has
    /// none.
    pub submitted_manifest_sha256: Option<String>,
    /// The SHA-256 that the last approval or withdrawal named, from the projection of the approval
    /// events; `null` when the site was never approved.
    pub approved_manifest_sha256: Option<String>,
    /// The approval state of the site.
    pub approval_state: ApprovalStateView,
}

/// The approval state of a site in [`SiteManifestState`].
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(rename = "SiteApprovalState")]
pub enum ApprovalStateView {
    /// No approval was ever announced for the site.
    Unapproved,
    /// The approval is in force.
    Approved,
    /// The approval was withdrawn.
    Withdrawn,
}

/// The answer of `GET /v1/sites/{site}/manifest`: the active manifest of the site.
#[derive(Debug, serde::Serialize, schemars::JsonSchema)]
#[schemars(rename = "ActiveSiteManifest")]
pub struct ActiveSiteManifest {
    /// The site id.
    pub site_id: String,
    /// The SHA-256 of the manifest bytes, 64 lower-case hex digits. The owner approves this digest.
    pub manifest_sha256: String,
    /// The hostname of the manifest.
    pub hostname: String,
    /// The publication ids of the manifest.
    pub publications: Vec<String>,
    /// The capabilities that the manifest requests.
    pub capabilities: Vec<String>,
}

/// One documented answer.
const fn response(status: u16, description: &'static str, payload: &'static str) -> ResponseDoc {
    ResponseDoc {
        status,
        description,
        payload: Some(Payload::Json(payload)),
    }
}

/// How `GET /v1/sites` is described in the generated `OpenAPI` document. Edge forwards the route
/// only when the deployment routes the sites service.
pub const LIST_DOC: RouteDoc = RouteDoc {
    method: Method::Get,
    path: "/v1/sites",
    operation_id: "listSites",
    summary: "List the owner's sites",
    description: "\
Lists the owner's sites, each with the digest of its active manifest, the digest of its last \
approval or withdrawal and its approval state (`unapproved`, `approved` or `withdrawn`). The sites \
service answers; Edge forwards the request after it checks the caller. Only a primary session \
whose user holds the live `platform.owner` grant may call this route (class `management`, \
ADR-0017).",
    tag: "sites",
    security: Security::Session,
    parameters: &[],
    request: None,
    responses: &[
        response(200, "The owner's sites.", "SiteList"),
        response(
            401,
            "No credential, or one that does not authenticate here.",
            "ErrorEnvelope",
        ),
        response(
            403,
            "The session is not a primary session whose user holds the live `platform.owner` \
             grant.",
            "ErrorEnvelope",
        ),
        response(
            503,
            "The sites service could not be reached.",
            "ErrorEnvelope",
        ),
        response(
            504,
            "The sites service did not answer in time.",
            "ErrorEnvelope",
        ),
    ],
};

/// How `GET /v1/sites/{site}/manifest` is described in the generated `OpenAPI` document.
pub const MANIFEST_DOC: RouteDoc = RouteDoc {
    method: Method::Get,
    path: "/v1/sites/{site}/manifest",
    operation_id: "readSiteManifest",
    summary: "Read the active manifest of a site",
    description: "\
Reads the active manifest of the site, the last valid submission, with its SHA-256. An approval \
names that digest. The sites service answers; Edge forwards the request after it checks the \
caller. Only a primary session whose user holds the live `platform.owner` grant may call this \
route (class `management`, ADR-0017).",
    tag: "sites",
    security: Security::Session,
    parameters: &[Parameter {
        name: "site",
        location: In::Path,
        required: true,
        format: None,
        description: "The site id: 3 to 40 lower-case ASCII letters, digits and `-`.",
    }],
    request: None,
    responses: &[
        response(
            200,
            "The active manifest of the site.",
            "ActiveSiteManifest",
        ),
        response(
            401,
            "No credential, or one that does not authenticate here.",
            "ErrorEnvelope",
        ),
        response(
            403,
            "The session is not a primary session whose user holds the live `platform.owner` \
             grant.",
            "ErrorEnvelope",
        ),
        response(404, "The site has no active manifest.", "ErrorEnvelope"),
        response(
            503,
            "The sites service could not be reached.",
            "ErrorEnvelope",
        ),
        response(
            504,
            "The sites service did not answer in time.",
            "ErrorEnvelope",
        ),
    ],
};
