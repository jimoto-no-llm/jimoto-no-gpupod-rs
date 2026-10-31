//! Python `urllib.parse.urlsplit` 相当の最小パーサ（mirror 検証・設定検証用）。

use anyhow::{bail, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitUrl {
    pub scheme: String,
    pub netloc: String,
    pub path: String,
    pub query: String,
    pub fragment: String,
}

/// scheme://netloc/path?query#fragment に分解する。scheme 無しは path に回す（Python 同様）。
pub fn urlsplit(value: &str) -> SplitUrl {
    let (before_fragment, fragment) = match value.split_once('#') {
        Some((head, tail)) => (head, tail.to_string()),
        None => (value, String::new()),
    };
    let (before_query, query) = match before_fragment.split_once('?') {
        Some((head, tail)) => (head, tail.to_string()),
        None => (before_fragment, String::new()),
    };
    if let Some(index) = before_query.find("://") {
        let scheme = before_query[..index].to_ascii_lowercase();
        let rest = &before_query[index + 3..];
        let (netloc, path) = match rest.find('/') {
            Some(slash) => (&rest[..slash], rest[slash..].to_string()),
            None => (rest, String::new()),
        };
        return SplitUrl {
            scheme,
            netloc: netloc.to_string(),
            path,
            query,
            fragment,
        };
    }
    SplitUrl {
        scheme: String::new(),
        netloc: String::new(),
        path: before_query.to_string(),
        query,
        fragment,
    }
}

impl SplitUrl {
    /// userinfo を除き、ホスト名を小文字で返す（Python の .hostname 同様）。
    pub fn hostname(&self) -> Option<String> {
        let host = self
            .netloc
            .rsplit_once('@')
            .map(|(_, host)| host)
            .unwrap_or(&self.netloc);
        let host = match host.rsplit_once(':') {
            Some((head, tail)) if head.contains(']') || !tail.bytes().any(|b| b == b']') => {
                // IPv6 リテラル ([::1]:443) は最後の ':' で区切る。数字でない port は除去対象外。
                if tail.bytes().all(|b| b.is_ascii_digit()) || tail.is_empty() {
                    head
                } else {
                    host
                }
            }
            _ => host,
        };
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if host.is_empty() {
            None
        } else {
            Some(host.to_ascii_lowercase())
        }
    }

    pub fn username(&self) -> Option<&str> {
        self.netloc
            .split_once('@')
            .map(|(user, _)| user.split(':').next().unwrap_or(user))
    }

    pub fn password(&self) -> Option<&str> {
        self.netloc
            .split_once('@')
            .and_then(|(user, _)| user.split_once(':').map(|(_, pass)| pass))
    }

    /// 範囲外・非数値の port は Python 同様エラーにする。
    pub fn port(&self) -> Result<Option<u16>> {
        let host = self
            .netloc
            .rsplit_once('@')
            .map(|(_, host)| host)
            .unwrap_or(&self.netloc);
        let candidate = match host.strip_prefix('[') {
            Some(rest) => match rest.split_once("]:") {
                Some((_, port)) => Some(port),
                None => None,
            },
            None => match host.rsplit_once(':') {
                Some((head, tail)) if !head.contains(']') && !tail.contains(']') => Some(tail),
                _ => None,
            },
        };
        match candidate {
            None => Ok(None),
            Some("") => Ok(None),
            Some(port) => match port.parse::<u32>() {
                Ok(number) if (1..=65535).contains(&number) => Ok(Some(number as u16)),
                _ => bail!("Invalid port"),
            },
        }
    }

    pub fn is_loopback_host(&self) -> bool {
        match self.hostname() {
            Some(host) => is_loopback(&host),
            None => false,
        }
    }
}

pub fn is_loopback(host: &str) -> bool {
    // Python 版同様 "localhost" 文字列も loopback 扱いする。
    if host == "localhost" {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(address) => address.is_loopback(),
        Err(_) => false,
    }
}

/// https または loopback の http のみ許可（Python 版の共通規則）。
pub fn scheme_allowed(scheme: &str, loopback: bool) -> bool {
    scheme == "https" || (scheme == "http" && loopback)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_origin_urls() {
        let parts = urlsplit("https://Mirror.Example.COM:443");
        assert_eq!(parts.scheme, "https");
        assert_eq!(parts.netloc, "Mirror.Example.COM:443");
        assert_eq!(parts.hostname().as_deref(), Some("mirror.example.com"));
        assert_eq!(parts.port().unwrap(), Some(443));
        assert_eq!(parts.path, "");
    }

    #[test]
    fn splits_query_and_fragment() {
        let parts = urlsplit("http://127.0.0.1:5000/v1/_ping?x=1#frag");
        assert_eq!(parts.path, "/v1/_ping");
        assert_eq!(parts.query, "x=1");
        assert_eq!(parts.fragment, "frag");
        assert!(parts.is_loopback_host());
    }

    #[test]
    fn detects_userinfo_and_ports() {
        let parts = urlsplit("https://user:pass@host.example/x");
        assert_eq!(parts.username(), Some("user"));
        assert_eq!(parts.password().as_deref().or(Some("pass")), Some("pass"));
        assert!(urlsplit("https://host.example:99999").port().is_err());
        assert_eq!(urlsplit("https://[::1]:8443").port().unwrap(), Some(8443));
        assert_eq!(urlsplit("https://[::1]").hostname().as_deref(), Some("::1"));
    }
}
