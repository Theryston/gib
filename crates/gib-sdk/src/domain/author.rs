use super::DomainError;
use std::fmt;
use std::str::FromStr;

/// The largest accepted author identity length in UTF-8 bytes.
pub const MAX_AUTHOR_IDENTITY_LENGTH: usize = 512;

/// A validated author identity in the stable `Name <email>` representation.
///
/// The original representation is retained exactly. In particular, the
/// constructor does not trim, case-fold, or otherwise normalize a valid
/// identity.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AuthorIdentity(String);

impl AuthorIdentity {
    /// Creates an author identity after validating its name and email.
    pub fn new(value: impl Into<String>) -> Result<Self, DomainError> {
        let value = value.into();
        validate_identity(&value)?;
        Ok(Self(value))
    }

    /// Returns the exact validated `Name <email>` representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the validated name portion without the separating space or
    /// angle brackets.
    pub fn name(&self) -> &str {
        let separator = self
            .0
            .rfind(" <")
            .unwrap_or_else(|| self.0.len().saturating_sub(1));
        &self.0[..separator]
    }

    /// Returns the validated email portion without angle brackets.
    pub fn email(&self) -> &str {
        let start = self.0.rfind('<').map_or(self.0.len(), |index| index + 1);
        self.0
            .get(start..self.0.len().saturating_sub(1))
            .unwrap_or("")
    }

    /// Consumes the identity and returns its exact representation.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl AsRef<str> for AuthorIdentity {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for AuthorIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for AuthorIdentity {
    type Err = DomainError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl TryFrom<&str> for AuthorIdentity {
    type Error = DomainError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for AuthorIdentity {
    type Error = DomainError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

fn validate_identity(value: &str) -> Result<(), DomainError> {
    if value.is_empty() || value.len() > MAX_AUTHOR_IDENTITY_LENGTH {
        return Err(invalid("must contain 1 to 512 UTF-8 bytes"));
    }
    if value.chars().any(|character| character.is_control()) {
        return Err(invalid("must not contain control characters"));
    }

    let Some(opening_bracket) = value.rfind(" <") else {
        return Err(invalid("must use the `Name <email>` representation"));
    };
    let name = &value[..opening_bracket];
    let email = &value[opening_bracket + 2..];
    if name.is_empty() {
        return Err(invalid("name must not be empty"));
    }
    if !email.ends_with('>') || email.len() <= 1 {
        return Err(invalid("email must be enclosed in angle brackets"));
    }
    let email = &email[..email.len() - 1];

    if value[..opening_bracket].contains(['<', '>'])
        || email.contains(['<', '>'])
        || value[opening_bracket + 2..].contains(" <")
    {
        return Err(invalid("must contain one name and one email"));
    }
    validate_name(name)?;
    validate_email(email)
}

fn validate_name(name: &str) -> Result<(), DomainError> {
    if name.starts_with(' ') || name.ends_with(' ') || name.contains("  ") {
        return Err(invalid("name must use single spaces between words"));
    }

    let mut has_letter = false;
    let mut word_has_letter = false;
    let mut previous_was_word = false;
    for character in name.chars() {
        if character == ' ' {
            if !previous_was_word {
                return Err(invalid("name must contain non-empty words"));
            }
            if !word_has_letter {
                return Err(invalid("name words must contain alphabetic characters"));
            }
            previous_was_word = false;
            word_has_letter = false;
            continue;
        }

        let allowed_punctuation = matches!(character, '\'' | '\u{2019}' | '-' | '.');
        if !character.is_alphabetic() && !allowed_punctuation {
            return Err(invalid("name contains an invalid character"));
        }
        if character.is_alphabetic() {
            has_letter = true;
            word_has_letter = true;
        }
        previous_was_word = true;
    }

    if !has_letter || !previous_was_word || !word_has_letter {
        return Err(invalid(
            "name must contain at least one alphabetic character",
        ));
    }
    Ok(())
}

fn validate_email(email: &str) -> Result<(), DomainError> {
    if email.is_empty()
        || email.len() > 254
        || email.chars().any(|character| character.is_whitespace())
    {
        return Err(invalid(
            "email must be a non-empty address without whitespace",
        ));
    }

    let mut at_signs = email.match_indices('@');
    let Some((at_index, _)) = at_signs.next() else {
        return Err(invalid("email must contain one `@`"));
    };
    if at_signs.next().is_some() {
        return Err(invalid("email must contain one `@`"));
    }

    let local = &email[..at_index];
    let domain = &email[at_index + 1..];
    if local.is_empty() || local.len() > 64 || domain.is_empty() || domain.len() > 253 {
        return Err(invalid("email local and domain parts have invalid lengths"));
    }
    if local.starts_with('.') || local.ends_with('.') || local.contains("..") {
        return Err(invalid("email local part has invalid dots"));
    }
    if !local.bytes().all(is_email_local_byte) {
        return Err(invalid("email local part contains an invalid character"));
    }

    let labels = domain.split('.').collect::<Vec<_>>();
    if labels.len() < 2 || labels.iter().any(|label| label.is_empty()) {
        return Err(invalid("email domain must contain a dot-separated host"));
    }
    for label in labels {
        if label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(invalid("email domain contains an invalid label"));
        }
    }
    Ok(())
}

fn is_email_local_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'.' | b'!'
                | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'/'
                | b'='
                | b'?'
                | b'^'
                | b'_'
                | b'`'
                | b'{'
                | b'|'
                | b'}'
                | b'~'
                | b'-'
        )
}

const fn invalid(reason: &'static str) -> DomainError {
    DomainError::InvalidAuthorIdentity { reason }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_identities_round_trip_without_normalization() {
        for value in [
            "Jane Doe <jane@example.com>",
            "O'Connor-Jones <oconnor.jones+backup@example.co.uk>",
            "Álvaro Núñez <alvaro@example.org>",
        ] {
            let identity = AuthorIdentity::new(value).expect("identity should be valid");
            assert_eq!(identity.as_str(), value);
            assert_eq!(identity.to_string(), value);
        }
    }

    #[test]
    fn invalid_names_are_rejected() {
        for value in [
            " <jane@example.com>",
            "Jane  Doe <jane@example.com>",
            "Jane123 Doe <jane@example.com>",
            "Jane/Doe <jane@example.com>",
            "Jane . <jane@example.com>",
        ] {
            assert!(
                AuthorIdentity::new(value).is_err(),
                "{value} should be invalid"
            );
        }
    }

    #[test]
    fn missing_brackets_and_invalid_emails_are_rejected() {
        for value in [
            "Jane Doe jane@example.com",
            "Jane Doe <jane@example.com",
            "Jane Doe jane@example.com>",
            "Jane Doe <janeexample.com>",
            "Jane Doe <jane@>",
            "Jane Doe <@example.com>",
            "Jane Doe <jane@example>",
            "Jane Doe <jane..doe@example.com>",
        ] {
            assert!(
                AuthorIdentity::new(value).is_err(),
                "{value} should be invalid"
            );
        }
    }
}
