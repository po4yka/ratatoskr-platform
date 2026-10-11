//! Who may report progress for which operation (XR-021 CONTRACTS.md section R2-02).
//!
//! `evt.platform.operation.reported.v1` is one shared subject, so the identities allowed to publish
//! it are told apart only by the envelope `producer` field. Before a report is applied, the
//! projection loads the operation and asks [`authorize`] two questions: is the envelope tenant the
//! operation's owner, and is the producer in the list for the operation's kind. The table below is
//! the only place the second answer lives.
//!
//! A kind that is not in the table allows no producer. Adding a new operation kind to Platform
//! therefore fails closed: its reports are rejected until a row names who may send them, and the
//! test `every_minted_operation_kind_has_an_authority_row` fails first.
//!
//! Residual risk, recorded in `docs/ARCHITECTURE.md` section 11.2: the four extraction and social
//! producers of `content.capture.submit` can still report for each other's capture operations,
//! because a capture operation does not record which service it was dispatched to. Per-producer
//! report subjects would close that and need a contract change.

use ratatoskr_identifiers::{TenantRef, UserId};
use uuid::Uuid;

/// Which producers may report for an operation kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Producers {
    /// Exactly these producers, named as they appear in the envelope `producer` field.
    Listed(&'static [&'static str]),
    /// The producer is bound by the operation's own acceptance record (the provider it was
    /// accepted for), not by this table. The projection keeps that check unchanged.
    ProviderBound,
}

/// The producers that report for a capture, whichever lane the capture took.
const CAPTURE_PRODUCERS: &[&str] = &[
    "ratatoskr-extractor",
    "ratatoskr-x",
    "ratatoskr-instagram",
    "ratatoskr-threads",
];

/// The one producer of every channel-digest operation.
const CHANNEL_DIGEST_PRODUCERS: &[&str] = &["ratatoskr-channel-digests"];

/// Every operation kind Platform mints, and who may report for it.
///
/// `channel_digest.schedule.occurrence` is the `operation_kind` that channel-digests registers with
/// the scheduler; Platform mints it for each due occurrence.
pub const AUTHORITY: &[(&str, Producers)] = &[
    (
        "content.capture.submit",
        Producers::Listed(CAPTURE_PRODUCERS),
    ),
    ("ai_archive.import", Producers::ProviderBound),
    (
        "channel_digest.subscription.set",
        Producers::Listed(CHANNEL_DIGEST_PRODUCERS),
    ),
    (
        "channel_digest.run",
        Producers::Listed(CHANNEL_DIGEST_PRODUCERS),
    ),
    (
        "channel_digest.schedule.occurrence",
        Producers::Listed(CHANNEL_DIGEST_PRODUCERS),
    ),
];

/// The row for `kind`, if the table has one.
#[must_use]
pub fn producers_for(kind: &str) -> Option<&'static Producers> {
    AUTHORITY
        .iter()
        .find(|(row, _)| *row == kind)
        .map(|(_, producers)| producers)
}

/// Whether a report from `producer` whose envelope names `envelope_tenant` may be applied to an
/// operation of `kind` owned by `owner`.
///
/// `envelope_tenant` is the wire text of the envelope `tenant_id` (`user:<uuid>`), or `None` when
/// the envelope has none. A missing, malformed or different tenant is a refusal, and so is a kind
/// without a row. For a [`Producers::ProviderBound`] kind this answers only the tenant question:
/// the caller still checks the acceptance binding.
#[must_use]
pub fn authorize(kind: &str, producer: &str, envelope_tenant: Option<&str>, owner: Uuid) -> bool {
    let Some(tenant) = envelope_tenant.and_then(|raw| TenantRef::parse(raw).ok()) else {
        return false;
    };
    if tenant != TenantRef::of_user(UserId(owner)) {
        return false;
    }
    match producers_for(kind) {
        Some(Producers::Listed(listed)) => listed.contains(&producer),
        Some(Producers::ProviderBound) => true,
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: Uuid = Uuid::from_u128(0x018f_0000_0000_7000_8000_0000_0000_0c01);

    #[test]
    fn a_listed_producer_with_the_owners_tenant_is_authorized() {
        assert!(authorize(
            "channel_digest.run",
            "ratatoskr-channel-digests",
            Some(&format!("user:{OWNER}")),
            OWNER,
        ));
    }

    #[test]
    fn a_malformed_tenant_is_refused() {
        for raw in [
            "",
            "user:",
            "user:not-a-uuid",
            "device:018f0000-0000-7000-8000-000000000c01",
        ] {
            assert!(
                !authorize(
                    "channel_digest.run",
                    "ratatoskr-channel-digests",
                    Some(raw),
                    OWNER
                ),
                "{raw:?}"
            );
        }
    }
}
