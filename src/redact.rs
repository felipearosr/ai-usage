//! Redaction for values that are printed locally.
//!
//! `aiu status` and `aiu schedule` echo configuration back at the user —
//! relay URLs, data directories — and their output is what someone pastes
//! into a bug report. A URL may carry credentials in its userinfo
//! (`https://user:token@relay.example`), so that part is masked before it
//! reaches a rendering. This is output hygiene, not the privacy hard rule:
//! nothing here is ever transmitted.

/// Masks the userinfo of any URL in `value`, leaving everything else intact.
pub fn url_userinfo(value: &str) -> String {
    let Some(scheme_end) = value.find("://") else {
        return value.to_string();
    };
    let authority_start = scheme_end + "://".len();
    let authority = &value[authority_start..];
    // Userinfo ends at the first `@`, and only counts inside the authority —
    // an `@` after the path has begun is just part of the path.
    let authority_end = authority.find(['/', '?', '#']).unwrap_or(authority.len());
    let Some(at) = authority[..authority_end].find('@') else {
        return value.to_string();
    };
    format!("{}***@{}", &value[..authority_start], &authority[at + 1..])
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

    #[test]
    fn an_at_sign_in_the_path_is_not_credentials() {
        assert_eq!(
            url_userinfo("https://relay.example/users/@me"),
            "https://relay.example/users/@me"
        );
        assert_eq!(
            url_userinfo("https://relay.example/?to=a@b"),
            "https://relay.example/?to=a@b"
        );
    }

    #[test]
    fn a_plain_path_is_not_a_url() {
        assert_eq!(url_userinfo("/srv/aiu"), "/srv/aiu");
        assert_eq!(url_userinfo(""), "");
    }

    #[test]
    fn a_multibyte_value_is_handled_by_character_not_byte() {
        assert_eq!(url_userinfo("/srv/données"), "/srv/données");
        assert_eq!(
            url_userinfo("https://üser:p@relay.example/données"),
            "https://***@relay.example/données"
        );
    }
}
