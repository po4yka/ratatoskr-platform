//! Reads the deployed NATS ACL and renders a startable copy of it.
//!
//! `deploy/nats/ratatoskr.conf` carries one `nkey:` placeholder per identity, because the public
//! keys belong to the host the operator generates them on. `nats-server` refuses a placeholder
//! ("Not a valid public nkey for a user"), so no test and no CI step can start a broker from the
//! file as committed. [`render`] closes that gap: it replaces every placeholder with a freshly
//! generated user key and hands back the matching seeds, which is the same procedure
//! `deploy/nats/README.md` gives an operator.
//!
//! [`parse`] reads the permission lists of each stanza, so the static tests can assert against the
//! settings of the file rather than against the prose beside them.
//!
//! This is development tooling. Nothing in production depends on it.

use std::collections::BTreeSet;

use nkeys::KeyPair;

/// The text every placeholder starts with.
pub const PLACEHOLDER_PREFIX: &str = "UREPLACE_ME_WITH_THE_PUBLIC_NKEY_OF_RATATOSKR_";

/// One identity stanza of the ACL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The identity name, for example `EXTRACTOR_BROWSER_WORKER`.
    pub name: String,
    /// The nkey token as written.
    pub placeholder: String,
    /// Publish allow list.
    pub publish_allow: Vec<String>,
    /// Publish deny list.
    pub publish_deny: Vec<String>,
    /// Subscribe allow list.
    pub subscribe_allow: Vec<String>,
}

impl Identity {
    /// The file name of this identity's seed: lowercase, dashes, `.nkey`.
    #[must_use]
    pub fn seed_file_name(&self) -> String {
        seed_file_name(&self.name)
    }
}

/// A generated identity key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedSeed {
    /// The identity name.
    pub name: String,
    /// The public key that replaced the placeholder.
    pub public_key: String,
    /// The matching seed. A secret: never log it.
    pub seed: String,
}

impl GeneratedSeed {
    /// The file name of this seed: lowercase, dashes, `.nkey`.
    #[must_use]
    pub fn file_name(&self) -> String {
        seed_file_name(&self.name)
    }
}

/// A rendered configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    /// The configuration text, identical to the input except for the nkey tokens.
    pub conf: String,
    /// One seed per replaced placeholder, in file order.
    pub seeds: Vec<GeneratedSeed>,
}

/// Why a configuration could not be read or rendered.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProfileError {
    /// An `nkey:` token does not follow the placeholder grammar.
    #[error("the nkey token {0} is not a placeholder of the form {PLACEHOLDER_PREFIX}<NAME>_<X+>")]
    NotAPlaceholder(String),

    /// Two stanzas carry the same placeholder.
    #[error("the placeholder {0} appears more than once")]
    DuplicatePlaceholder(String),

    /// A permission block is missing its closing bracket or brace.
    #[error("the {0} block of {1} is not balanced")]
    Unbalanced(&'static str, String),

    /// A placeholder survived rendering.
    #[error("a placeholder remains after rendering: {0}")]
    PlaceholderRemains(String),

    /// The key generator failed.
    #[error("a key could not be generated: {0}")]
    Key(String),
}

/// The seed file name for an identity name.
#[must_use]
pub fn seed_file_name(name: &str) -> String {
    format!("{}.nkey", name.to_ascii_lowercase().replace('_', "-"))
}

/// The text with every comment replaced by spaces, so offsets in it are offsets in the original.
///
/// A `#` starts a comment unless it is inside a double-quoted string. A comment can itself contain
/// a quote or a brace, which is why this is a scan and not a regular expression over lines.
fn mask_comments(conf: &str) -> String {
    let mut masked = String::with_capacity(conf.len());
    let mut in_string = false;
    let mut in_comment = false;
    for character in conf.chars() {
        if in_comment {
            if character == '\n' {
                in_comment = false;
                masked.push('\n');
            } else {
                // Same byte length as the original character, so offsets stay valid.
                for _ in 0..character.len_utf8() {
                    masked.push(' ');
                }
            }
            continue;
        }
        match character {
            '"' => {
                in_string = !in_string;
                masked.push(character);
            }
            '#' if !in_string => {
                in_comment = true;
                masked.push(' ');
            }
            _ => masked.push(character),
        }
    }
    masked
}

/// One `nkey: TOKEN` occurrence outside a comment.
struct Token {
    /// Byte offset of the token in the original text.
    start: usize,
    /// Byte offset one past the token.
    end: usize,
}

fn find_tokens(masked: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut from = 0;
    while let Some(found) = masked.get(from..).and_then(|rest| rest.find("nkey:")) {
        let after_key = from + found + "nkey:".len();
        let rest = masked.get(after_key..).unwrap_or_default();
        let trimmed = rest.trim_start();
        let start = after_key + (rest.len() - trimmed.len());
        let length = trimmed
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(trimmed.len());
        tokens.push(Token {
            start,
            end: start + length,
        });
        from = start + length.max(1);
    }
    tokens
}

/// `NAME` out of `UREPLACE_ME_..._RATATOSKR_<NAME>_<X+>`, split at the last underscore.
fn placeholder_name(token: &str) -> Result<String, ProfileError> {
    let invalid = || ProfileError::NotAPlaceholder(token.to_owned());
    let rest = token.strip_prefix(PLACEHOLDER_PREFIX).ok_or_else(invalid)?;
    let (name, padding) = rest.rsplit_once('_').ok_or_else(invalid)?;
    let name_ok = !name.is_empty() && name.chars().all(|c| c.is_ascii_uppercase() || c == '_');
    let padding_ok = !padding.is_empty() && padding.chars().all(|c| c == 'X');
    if name_ok && padding_ok {
        Ok(name.to_owned())
    } else {
        Err(invalid())
    }
}

/// The text inside the first `{ ... }` that follows `key` in `text`, braces matched outside strings.
fn block<'a>(
    text: &'a str,
    key: &str,
    name: &str,
    label: &'static str,
) -> Result<Option<&'a str>, ProfileError> {
    let Some(at) = text.find(&format!("{key}:")) else {
        return Ok(None);
    };
    let after = at + key.len() + 1;
    let rest = text.get(after..).unwrap_or_default();
    let Some(open) = rest.find('{') else {
        return Err(ProfileError::Unbalanced(label, name.to_owned()));
    };
    let mut depth = 0_usize;
    let mut in_string = false;
    for (offset, character) in rest.char_indices().skip_while(|(offset, _)| *offset < open) {
        match character {
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Ok(rest.get(open + 1..offset));
                }
            }
            _ => {}
        }
    }
    Err(ProfileError::Unbalanced(label, name.to_owned()))
}

/// The quoted strings of `key: [ ... ]` inside `text`, in order.
fn list(
    text: &str,
    key: &str,
    name: &str,
    label: &'static str,
) -> Result<Vec<String>, ProfileError> {
    let Some(at) = text.find(&format!("{key}:")) else {
        return Ok(Vec::new());
    };
    let rest = text.get(at + key.len() + 1..).unwrap_or_default();
    let Some(open) = rest.find('[') else {
        return Err(ProfileError::Unbalanced(label, name.to_owned()));
    };
    let Some(close) = rest.get(open..).and_then(|tail| tail.find(']')) else {
        return Err(ProfileError::Unbalanced(label, name.to_owned()));
    };
    let inside = rest.get(open + 1..open + close).unwrap_or_default();
    Ok(inside
        .split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect())
}

/// Parse the identity stanzas.
///
/// A stanza is the text from one `nkey:` token to the next. Comments are ignored, including a
/// commented-out `nkey:` line.
///
/// # Errors
///
/// [`ProfileError::NotAPlaceholder`] for a token outside the placeholder grammar,
/// [`ProfileError::DuplicatePlaceholder`] for a repeated one, and [`ProfileError::Unbalanced`] for
/// a permission block that does not close.
pub fn parse(conf: &str) -> Result<Vec<Identity>, ProfileError> {
    let masked = mask_comments(conf);
    let tokens = find_tokens(&masked);
    let mut seen = BTreeSet::new();
    let mut identities = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        let placeholder = masked.get(token.start..token.end).unwrap_or_default();
        let name = placeholder_name(placeholder)?;
        if !seen.insert(placeholder.to_owned()) {
            return Err(ProfileError::DuplicatePlaceholder(placeholder.to_owned()));
        }
        let stanza_end = tokens
            .get(index + 1)
            .map_or(masked.len(), |next| next.start);
        let stanza = masked.get(token.end..stanza_end).unwrap_or_default();
        let publish = block(stanza, "publish", &name, "publish")?.unwrap_or_default();
        let subscribe = block(stanza, "subscribe", &name, "subscribe")?.unwrap_or_default();
        identities.push(Identity {
            publish_allow: list(publish, "allow", &name, "publish allow")?,
            publish_deny: list(publish, "deny", &name, "publish deny")?,
            subscribe_allow: list(subscribe, "allow", &name, "subscribe allow")?,
            name,
            placeholder: placeholder.to_owned(),
        });
    }
    Ok(identities)
}

/// Render the configuration: every placeholder replaced by a fresh user public key.
///
/// # Errors
///
/// Anything [`parse`] refuses, [`ProfileError::Key`] if a key cannot be generated, and
/// [`ProfileError::PlaceholderRemains`] if a `UREPLACE_ME` token is left outside a comment.
pub fn render(conf: &str) -> Result<Rendered, ProfileError> {
    let identities = parse(conf)?;
    let masked = mask_comments(conf);
    let tokens = find_tokens(&masked);

    let mut conf_out = String::with_capacity(conf.len());
    let mut seeds = Vec::with_capacity(identities.len());
    let mut cursor = 0;
    for (token, identity) in tokens.iter().zip(&identities) {
        let pair = KeyPair::new_user();
        let seed = pair
            .seed()
            .map_err(|error| ProfileError::Key(error.to_string()))?;
        let public_key = pair.public_key();
        conf_out.push_str(conf.get(cursor..token.start).unwrap_or_default());
        conf_out.push_str(&public_key);
        cursor = token.end;
        seeds.push(GeneratedSeed {
            name: identity.name.clone(),
            public_key,
            seed,
        });
    }
    conf_out.push_str(conf.get(cursor..).unwrap_or_default());

    if let Some(at) = mask_comments(&conf_out).find("UREPLACE_ME") {
        let token: String = conf_out
            .get(at..)
            .unwrap_or_default()
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        return Err(ProfileError::PlaceholderRemains(token));
    }
    Ok(Rendered {
        conf: conf_out,
        seeds,
    })
}
