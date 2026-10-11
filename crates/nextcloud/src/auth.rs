//! Credentials for a Nextcloud account: a user name and an app password, sent as HTTP Basic
//! authentication.
//!
//! An app password (Nextcloud ▸ Personal settings ▸ Security ▸ Devices & sessions) is used, never
//! the account password: it can be revoked on its own and does not need the second factor. The
//! secret is kept out of every `Debug` output so a log line or an error report cannot leak it.

use std::fmt;

/// Why a user name or secret can't be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthError {
    /// Empty, contains `:` (the separator in Basic authentication) or a control character.
    BadUser,
    /// Empty or contains a control character.
    BadSecret,
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthError::BadUser => write!(f, "the user name is empty or contains a `:` or a control character"),
            AuthError::BadSecret => write!(f, "the app password is empty or contains a control character"),
        }
    }
}

impl std::error::Error for AuthError {}

/// A user name and app password.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    user: String,
    secret: String,
}

impl Credentials {
    pub fn new(user: &str, secret: &str) -> Result<Credentials, AuthError> {
        if user.is_empty() || user.chars().any(|c| c == ':' || c.is_control()) {
            return Err(AuthError::BadUser);
        }
        if secret.is_empty() || secret.chars().any(char::is_control) {
            return Err(AuthError::BadSecret);
        }
        Ok(Credentials { user: user.to_string(), secret: secret.to_string() })
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    /// The value of the `Authorization` header: `Basic base64(user:secret)`.
    pub fn authorization(&self) -> String {
        format!("Basic {}", base64(format!("{}:{}", self.user, self.secret).as_bytes()))
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials").field("user", &self.user).field("secret", &"<hidden>").finish()
    }
}

/// Standard Base64 (RFC 4648) with padding.
pub fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let sextet = |n: u32, shift: u32| char::from(ALPHABET.get(((n >> shift) & 63) as usize).copied().unwrap_or(b'='));
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = |i: usize| u32::from(chunk.get(i).copied().unwrap_or(0));
        let n = b(0) << 16 | b(1) << 8 | b(2);
        out.push(sextet(n, 18));
        out.push(sextet(n, 12));
        out.push(if chunk.len() > 1 { sextet(n, 6) } else { '=' });
        out.push(if chunk.len() > 2 { sextet(n, 0) } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc_4648_vectors() {
        for (plain, coded) in
            [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")]
        {
            assert_eq!(base64(plain.as_bytes()), coded, "{plain:?}");
        }
        assert_eq!(base64(&[0xFB, 0xFF, 0xFE]), "+//+", "the + and / of the standard alphabet");
        assert_eq!(base64(&(0..=255u8).collect::<Vec<_>>()).len(), 344);
    }

    #[test]
    fn the_header_is_basic_user_colon_secret() {
        let c = Credentials::new("Aladdin", "open sesame").unwrap();
        assert_eq!(c.authorization(), "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==", "the example of RFC 7617");
        assert_eq!(c.user(), "Aladdin");
        // a secret may contain a colon, a user name may not
        assert!(Credentials::new("dima", "ab:cd").is_ok());
        assert_eq!(Credentials::new("di:ma", "x"), Err(AuthError::BadUser));
    }

    #[test]
    fn bad_credentials_are_refused() {
        assert_eq!(Credentials::new("", "x"), Err(AuthError::BadUser));
        assert_eq!(Credentials::new("a\nb", "x"), Err(AuthError::BadUser));
        assert_eq!(Credentials::new("dima", ""), Err(AuthError::BadSecret));
        assert_eq!(Credentials::new("dima", "x\r\nInjected: 1"), Err(AuthError::BadSecret));
    }

    #[test]
    fn the_secret_never_shows_in_debug_output() {
        let c = Credentials::new("dima", "s3cr3t-app-password").unwrap();
        for text in [format!("{c:?}"), format!("{c:#?}"), format!("{:?}", Some(&c)), format!("{:?}", vec![c.clone()])] {
            assert!(!text.contains("s3cr3t"), "{text}");
            assert!(text.contains("dima") && text.contains("hidden"), "{text}");
        }
    }
}
