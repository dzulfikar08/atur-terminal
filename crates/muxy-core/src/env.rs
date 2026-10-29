//! Reads `ATUR_TERM_*` settings, falling back to the `MUXY_*` variable the
//! same setting had before the rename, so existing shell profiles and service
//! units keep working.

use std::env;
use std::ffi::OsString;

const PREFIX: &str = "ATUR_TERM_";
const LEGACY_PREFIX: &str = "MUXY_";

/// The value of `name`, else of its legacy `MUXY_` spelling.
pub fn var_os(name: &str) -> Option<OsString> {
    lookup(name, |name| env::var_os(name))
}

/// Like [`var_os`], for values that are valid UTF-8.
pub fn var(name: &str) -> Option<String> {
    lookup(name, |name| env::var(name).ok())
}

fn lookup<T>(name: &str, get: impl Fn(&str) -> Option<T>) -> Option<T> {
    get(name).or_else(|| {
        let suffix = name.strip_prefix(PREFIX)?;
        get(&format!("{LEGACY_PREFIX}{suffix}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn environment<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    #[test]
    fn the_new_name_wins_over_the_legacy_one() {
        let get = environment(&[("ATUR_TERM_DIR", "new"), ("MUXY_DIR", "old")]);
        assert_eq!(lookup("ATUR_TERM_DIR", get), Some("new".to_owned()));
    }

    #[test]
    fn the_legacy_name_is_read_when_the_new_one_is_unset() {
        let get = environment(&[("MUXY_SSH", "/usr/bin/old-ssh")]);
        assert_eq!(
            lookup("ATUR_TERM_SSH", get),
            Some("/usr/bin/old-ssh".to_owned())
        );
    }

    #[test]
    fn names_without_the_prefix_have_no_fallback() {
        let get = environment(&[("MUXY_HOME", "old")]);
        assert_eq!(lookup("HOME", get), None);
    }

    #[test]
    fn an_unset_setting_is_none() {
        assert_eq!(lookup("ATUR_TERM_DIR", environment(&[])), None);
    }
}
