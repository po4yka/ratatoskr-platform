//! Who may report progress for which operation (XR-021 CONTRACTS.md sections R2-02 and R3-02).
//!
//! `evt.platform.operation.reported.v1` is one shared subject, so the identities allowed to publish
//! it are told apart only by the envelope `producer` field. Before a report is applied, the
//! projection loads the operation and asks [`authorize`] three questions: is the envelope tenant the
//! operation's owner, is the producer in the list for the operation's kind, and is it the one
//! service the operation was dispatched to. The table below holds the second answer, as the closed
//! list of producers a kind may name; `operations.operations.reporter` holds the third, written by
//! [`crate::accept`] when the intake path that mints the operation knows the lane. An AI-archive
//! import is the exception: its reports carry no tenant and no recorded reporter, and Platform's
//! acceptance record binds the owner, the provider and the producer instead.
//!
//! A kind that is not in the table allows no producer. Adding a new operation kind to Platform
//! therefore fails closed: its reports are rejected until a row names who may send them, and the
//! test `every_minted_operation_kind_has_an_authority_row` fails first. A listed kind whose
//! operation recorded no reporter accepts no producer either, and the test
//! `every_mint_site_passes_a_lane_for_a_listed_kind` fails when a mint site passes none.
//!
//! What this does not close, recorded in `docs/ARCHITECTURE.md` section 11.2: the subject is still
//! shared, so a compromised producer can forge a report for another operation of its own lane and
//! tenant, and the X identity can read every tenant's report metadata through its durable.
//! Per-producer report subjects would close both and need a contract change.

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

/// The one producer of a vault connect (workspace change `add-notes-vault-connections`), of a
/// notes write and of a write-mode change (workspace change `add-notes-write-back`).
const NOTES_PRODUCERS: &[&str] = &["ratatoskr-notes"];

/// The one producer of a site release build (workspace change `add-site-publications`).
const SITES_PRODUCERS: &[&str] = &["ratatoskr-sites"];

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
    (
        "notes.connection.connect",
        Producers::Listed(NOTES_PRODUCERS),
    ),
    ("notes.file.write", Producers::Listed(NOTES_PRODUCERS)),
    (
        "notes.connection.write_mode",
        Producers::Listed(NOTES_PRODUCERS),
    ),
    ("sites.release.build", Producers::Listed(SITES_PRODUCERS)),
];

/// The row for `kind`, if the table has one.
#[must_use]
pub fn producers_for(kind: &str) -> Option<&'static Producers> {
    AUTHORITY
        .iter()
        .find(|(row, _)| *row == kind)
        .map(|(_, producers)| producers)
}

/// Whether `producer` may be recorded as the reporter of a new operation of `kind`: the kind has a
/// listed row and names it. A kind bound some other way (`ai_archive.import`) and a kind without a
/// row name no reporter.
#[must_use]
pub fn allows(kind: &str, producer: &str) -> bool {
    matches!(producers_for(kind), Some(Producers::Listed(listed)) if listed.contains(&producer))
}

/// Whether a report from `producer` whose envelope names `envelope_tenant` may be applied to an
/// operation of `kind` owned by `owner` that was dispatched to `reporter`.
///
/// `envelope_tenant` is the wire text of the envelope `tenant_id` (`user:<uuid>`), or `None` when
/// the envelope has none. A kind without a row is refused, and so is a listed kind whose tenant is
/// missing, malformed or not the owner's, whose operation recorded no `reporter`, or whose recorded
/// `reporter` is not `producer`.
///
/// A [`Producers::ProviderBound`] kind is the one place a missing tenant is tolerated, and only
/// when `acceptance_bound` says the caller found Platform's own acceptance record for the
/// operation, naming the same owner. The two AI archive services publish their operation
/// reports with no envelope tenant, and the acceptance record is the stronger evidence: Platform
/// wrote it when it accepted the upload, so it pins the owner, the provider and (through the
/// projection) the subject and producer. A tenant that is present is still checked, because a
/// wrong one is a bug or a forgery whichever kind it arrives on. The caller checks the subject
/// and producer of the binding.
#[must_use]
pub fn authorize(
    kind: &str,
    producer: &str,
    envelope_tenant: Option<&str>,
    owner: Uuid,
    reporter: Option<&str>,
    acceptance_bound: bool,
) -> bool {
    let tenant_ok = match envelope_tenant {
        Some(raw) => {
            TenantRef::parse(raw).is_ok_and(|tenant| tenant == TenantRef::of_user(UserId(owner)))
        }
        None => false,
    };
    match producers_for(kind) {
        Some(Producers::Listed(listed)) => {
            tenant_ok && listed.contains(&producer) && reporter == Some(producer)
        }
        Some(Producers::ProviderBound) => {
            if envelope_tenant.is_some() {
                tenant_ok
            } else {
                acceptance_bound
            }
        }
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
            Some("ratatoskr-channel-digests"),
            false,
        ));
    }

    #[test]
    fn a_listed_kind_needs_a_tenant_even_when_an_acceptance_record_exists() {
        assert!(!authorize(
            "channel_digest.run",
            "ratatoskr-channel-digests",
            None,
            OWNER,
            Some("ratatoskr-channel-digests"),
            true,
        ));
    }

    #[test]
    fn a_provider_bound_kind_without_a_tenant_needs_the_acceptance_record() {
        assert!(authorize(
            "ai_archive.import",
            "ratatoskr-claude",
            None,
            OWNER,
            None,
            true
        ));
        assert!(!authorize(
            "ai_archive.import",
            "ratatoskr-claude",
            None,
            OWNER,
            None,
            false
        ));
    }

    #[test]
    fn a_provider_bound_kind_checks_a_tenant_that_is_present() {
        let other = Uuid::from_u128(0x018f_0000_0000_7000_8000_0000_0000_0c02);
        assert!(authorize(
            "ai_archive.import",
            "ratatoskr-claude",
            Some(&format!("user:{OWNER}")),
            OWNER,
            None,
            false,
        ));
        for raw in [format!("user:{other}"), "user:not-a-uuid".to_owned()] {
            assert!(
                !authorize(
                    "ai_archive.import",
                    "ratatoskr-claude",
                    Some(&raw),
                    OWNER,
                    None,
                    true
                ),
                "{raw:?}"
            );
        }
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
                    OWNER,
                    Some("ratatoskr-channel-digests"),
                    false,
                ),
                "{raw:?}"
            );
        }
    }

    #[test]
    fn a_listed_kind_needs_the_recorded_reporter_to_be_the_producer() {
        let tenant = format!("user:{OWNER}");
        let report = |producer: &str, reporter: Option<&str>| {
            authorize(
                "content.capture.submit",
                producer,
                Some(&tenant),
                OWNER,
                reporter,
                false,
            )
        };
        assert!(report("ratatoskr-x", Some("ratatoskr-x")));
        assert!(!report("ratatoskr-x", Some("ratatoskr-instagram")));
        assert!(!report("ratatoskr-x", None));
    }

    #[test]
    fn allows_names_only_the_producers_of_a_listed_kind() {
        assert!(allows("content.capture.submit", "ratatoskr-threads"));
        assert!(!allows(
            "content.capture.submit",
            "ratatoskr-channel-digests"
        ));
        assert!(!allows("ai_archive.import", "ratatoskr-chatgpt"));
        assert!(!allows("github.sync.requested", "ratatoskr-github"));
    }
}
