//! Redaction for values that are printed locally.
//!
//! `aiu status` and `aiu schedule` echo configuration back at the user —
//! relay URLs, data directories — and their output is what someone pastes
//! into a bug report. A URL may carry credentials in its userinfo
//! (`https://user:token@relay.example`), so that part is masked before it
//! reaches a rendering. This is output hygiene, not the privacy hard rule:
//! nothing here is ever transmitted.

/// Masks the userinfo of every URL in `value`, leaving everything else
/// intact.
///
/// The input is free text — a relay error may name several URLs, or a path
/// URL before the credentialed one — so every `://` is examined, not just the
/// first. Within an authority the *last* `@` wins, since a password may
/// itself contain one, and the authority ends at whitespace or punctuation so
/// that an `@` in the prose after a URL is never mistaken for its userinfo.
pub fn url_userinfo(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;

    while let Some(scheme_end) = rest.find("://") {
        let authority_start = scheme_end + "://".len();
        out.push_str(&rest[..authority_start]);

        let authority = &rest[authority_start..];
        let authority_end = authority.find(is_authority_end).unwrap_or(authority.len());
        match authority[..authority_end].rfind('@') {
            Some(at) => {
                out.push_str("***@");
                out.push_str(&authority[at + 1..authority_end]);
            }
            None => out.push_str(&authority[..authority_end]),
        }
        rest = &authority[authority_end..];
    }

    out.push_str(rest);
    out
}

/// Characters that cannot appear in a URL authority, so the first one ends it.
fn is_authority_end(c: char) -> bool {
    c.is_whitespace() || matches!(c, '/' | '?' | '#' | ',' | ')' | '"' | '\'' | '<' | '>')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_without_credentials_is_untouched() {
        assert_eq!(
            url_userinfo("https://relay.aiu.sh/v1"),
            "https://relay.aiu.sh/v1"
        );
    }

    #[test]
    fn credentials_in_a_url_are_masked() {
        assert_eq!(
            url_userinfo("https://user:s3cret@relay.example/v1"),
            "https://***@relay.example/v1"
        );
        assert_eq!(
            url_userinfo("https://token@relay.example"),
            "https://***@relay.example"
        );
    }

    /// A relay error may name a path URL before the credentialed one. Bailing
    /// at the first `://` let the secret through untouched.
    #[test]
    fn a_earlier_url_without_an_authority_does_not_stop_the_scan() {
        assert_eq!(
            url_userinfo("failed to reach file:///srv/aiu and https://user:tok@relay.example"),
            "failed to reach file:///srv/aiu and https://***@relay.example"
        );
    }

    #[test]
    fn every_url_in_the_value_is_masked_not_only_the_first() {
        assert_eq!(
            url_userinfo("a https://u:p1@h1 and https://u:p2@h2"),
            "a https://***@h1 and https://***@h2"
        );
    }

    /// A password may contain an `@`; the userinfo runs to the *last* one.
    #[test]
    fn an_at_sign_inside_the_password_does_not_expose_the_tail() {
        assert_eq!(
            url_userinfo("https://user:p@ss@relay.example/v1"),
            "https://***@relay.example/v1"
        );
    }

    #[test]
    fn an_at_sign_in_the_path_or_prose_is_not_credentials() {
        assert_eq!(
            url_userinfo("https://relay.example/users/@me"),
            "https://relay.example/users/@me"
        );
        assert_eq!(
            url_userinfo("https://relay.example/?to=a@b"),
            "https://relay.example/?to=a@b"
        );
        assert_eq!(
            url_userinfo("https://relay.example refused, mail a@b"),
            "https://relay.example refused, mail a@b"
        );
    }

    #[test]
    fn a_plain_path_is_not_a_url() {
        assert_eq!(url_userinfo("/srv/aiu"), "/srv/aiu");
        assert_eq!(url_userinfo(""), "");
    }

    /// Slicing is by byte offset from `find`, which lands on char boundaries;
    /// multi-byte input must round-trip rather than panic.
    #[test]
    fn multibyte_values_are_preserved() {
        assert_eq!(url_userinfo("/srv/données"), "/srv/données");
        assert_eq!(
            url_userinfo("https://üser:p@relay.example/données"),
            "https://***@relay.example/données"
        );
    }
}
