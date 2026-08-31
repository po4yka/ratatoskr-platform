//! `OpenAPI` descriptions for operation-bound archive transfer routes.

use platform_api_doc::{In, Method, Parameter, Payload, ResponseDoc, RouteDoc, Security};

use super::{CHUNK_ROUTE, FINALIZE_ROUTE, OPEN_ROUTE, PREPARE_ROUTE, STATUS_ROUTE};

/// Generated `OpenAPI` description for preparation. The body upload is intentionally a binary
/// transport endpoint and uses the operation-bound URL returned here.
pub const PREPARE_DOC: RouteDoc = RouteDoc {
    method: Method::Post,
    path: PREPARE_ROUTE,
    operation_id: "prepareAiArchive",
    summary: "Prepare an AI archive import",
    description: "Accepts immutable metadata for a registered export-agent archive and returns the operation-bound upload path. It does not mean that archive bytes have been received or imported.",
    tag: "ai-archives",
    security: Security::Session,
    parameters: &[
        Parameter {
            name: "provider",
            location: In::Path,
            required: true,
            format: None,
            description: "`chatgpt` or `claude`.",
        },
        Parameter {
            name: "Idempotency-Key",
            location: In::Header,
            required: true,
            format: None,
            description: "A client-chosen 1 to 255 character replay key.",
        },
    ],
    request: Some(Payload::Json("PrepareArchive")),
    responses: &[
        ResponseDoc {
            status: 202,
            description: "Metadata accepted durably; upload bytes to the returned path.",
            payload: Some(Payload::Json("ArchivePrepared")),
        },
        ResponseDoc {
            status: 400,
            description: "The metadata or idempotency header is invalid.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 401,
            description: "No credential, or one that does not authenticate here.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 404,
            description: "The provider is unsupported or this is not a registered export-agent device.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 409,
            description: "The idempotency key is in use for different metadata.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};

/// Generated `OpenAPI` description for opening an operation-owned transfer.
pub const OPEN_DOC: RouteDoc = RouteDoc {
    method: Method::Post,
    path: OPEN_ROUTE,
    operation_id: "openAiArchiveTransfer",
    summary: "Open an AI archive transfer",
    description: "Opens bounded resumable staging under the prepared provider and operation.",
    tag: "ai-archives",
    security: Security::Session,
    parameters: &[
        Parameter {
            name: "provider",
            location: In::Path,
            required: true,
            format: None,
            description: "The prepared provider.",
        },
        Parameter {
            name: "operation_id",
            location: In::Path,
            required: true,
            format: Some("uuid"),
            description: "The prepared operation.",
        },
    ],
    request: Some(Payload::Json("UploadSessionRequest")),
    responses: &[
        ResponseDoc {
            status: 201,
            description: "The resumable session is durable.",
            payload: Some(Payload::Json("UploadSessionOpened")),
        },
        ResponseDoc {
            status: 400,
            description: "The declaration is invalid.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 401,
            description: "The credential does not authenticate.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 404,
            description: "The bound operation is unavailable to this device.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};

/// Generated `OpenAPI` description for one transfer chunk.
pub const CHUNK_DOC: RouteDoc = RouteDoc {
    method: Method::Put,
    path: CHUNK_ROUTE,
    operation_id: "putAiArchiveTransferChunk",
    summary: "Store one AI archive chunk",
    description: "Stores one exact indexed chunk beneath the operation-owned session.",
    tag: "ai-archives",
    security: Security::Session,
    parameters: &[
        Parameter {
            name: "provider",
            location: In::Path,
            required: true,
            format: None,
            description: "The prepared provider.",
        },
        Parameter {
            name: "operation_id",
            location: In::Path,
            required: true,
            format: Some("uuid"),
            description: "The prepared operation.",
        },
        Parameter {
            name: "token",
            location: In::Path,
            required: true,
            format: None,
            description: "The opaque resumption token.",
        },
        Parameter {
            name: "index",
            location: In::Path,
            required: true,
            format: None,
            description: "The zero-based chunk index.",
        },
    ],
    request: Some(Payload::Binary),
    responses: &[
        ResponseDoc {
            status: 200,
            description: "The chunk is acknowledged.",
            payload: Some(Payload::Json("UploadChunkReceipt")),
        },
        ResponseDoc {
            status: 400,
            description: "The chunk does not match the declared plan.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 404,
            description: "The bound session is unavailable to this device.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};

/// Generated `OpenAPI` description for resumable transfer status.
pub const STATUS_DOC: RouteDoc = RouteDoc {
    method: Method::Get,
    path: STATUS_ROUTE,
    operation_id: "readAiArchiveTransferStatus",
    summary: "Read AI archive transfer status",
    description: "Returns the exact acknowledged indices for restart-safe resume.",
    tag: "ai-archives",
    security: Security::Session,
    parameters: &[
        Parameter {
            name: "provider",
            location: In::Path,
            required: true,
            format: None,
            description: "The prepared provider.",
        },
        Parameter {
            name: "operation_id",
            location: In::Path,
            required: true,
            format: Some("uuid"),
            description: "The prepared operation.",
        },
        Parameter {
            name: "token",
            location: In::Path,
            required: true,
            format: None,
            description: "The opaque resumption token.",
        },
    ],
    request: None,
    responses: &[
        ResponseDoc {
            status: 200,
            description: "The current resumable view.",
            payload: Some(Payload::Json("UploadStatusResponse")),
        },
        ResponseDoc {
            status: 404,
            description: "The bound session is unavailable to this device.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};

/// Generated `OpenAPI` description for transfer verification and provider delivery.
pub const FINALIZE_DOC: RouteDoc = RouteDoc {
    method: Method::Post,
    path: FINALIZE_ROUTE,
    operation_id: "finalizeAiArchiveTransfer",
    summary: "Finalize an AI archive transfer",
    description: "Verifies ordered staged bytes and delivers them only to the operation-bound provider receipt.",
    tag: "ai-archives",
    security: Security::Session,
    parameters: &[
        Parameter {
            name: "provider",
            location: In::Path,
            required: true,
            format: None,
            description: "The prepared provider.",
        },
        Parameter {
            name: "operation_id",
            location: In::Path,
            required: true,
            format: Some("uuid"),
            description: "The prepared operation.",
        },
        Parameter {
            name: "token",
            location: In::Path,
            required: true,
            format: None,
            description: "The opaque resumption token.",
        },
    ],
    request: Some(Payload::Json("UploadFinalizeRequest")),
    responses: &[
        ResponseDoc {
            status: 200,
            description: "Verification produced a truthful stored or digest-mismatch outcome.",
            payload: Some(Payload::Json("UploadCompletionOutcome")),
        },
        ResponseDoc {
            status: 400,
            description: "Chunks are incomplete or the finalize request is invalid.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
        ResponseDoc {
            status: 404,
            description: "The bound session is unavailable to this device.",
            payload: Some(Payload::Json("ErrorEnvelope")),
        },
    ],
};
