//! DNS settings for the `dns` plugin. Port of yip's `pkg/schema/schema.go::DNS`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DNS {
    #[serde(default, rename = "nameservers", skip_serializing_if = "Vec::is_empty")]
    pub nameservers: Vec<String>,

    /// `DnsSearch` in Go — YAML key is `search`.
    #[serde(default, rename = "search", skip_serializing_if = "Vec::is_empty")]
    pub dns_search: Vec<String>,

    /// `DnsOptions` in Go — YAML key is `options`.
    #[serde(default, rename = "options", skip_serializing_if = "Vec::is_empty")]
    pub dns_options: Vec<String>,

    #[serde(default, rename = "path", skip_serializing_if = "String::is_empty")]
    pub path: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    #[test]
    fn parses_full() {
        let y = indoc! {r#"
            nameservers:
              - 8.8.8.8
              - 1.1.1.1
            search:
              - example.com
            options:
              - timeout:2
            path: /etc/resolv.conf
        "#};
        let dns: DNS = serde_yaml::from_str(y).unwrap();
        assert_eq!(dns.nameservers, vec!["8.8.8.8", "1.1.1.1"]);
        assert_eq!(dns.dns_search, vec!["example.com"]);
        assert_eq!(dns.dns_options, vec!["timeout:2"]);
        assert_eq!(dns.path, "/etc/resolv.conf");
    }

    #[test]
    fn defaults_for_missing_fields() {
        let dns: DNS = serde_yaml::from_str("{}").unwrap();
        assert_eq!(dns, DNS::default());
    }

    #[test]
    fn roundtrip() {
        let dns = DNS {
            nameservers: vec!["8.8.8.8".into()],
            dns_search: vec!["a".into()],
            dns_options: vec!["b".into()],
            path: "/etc/resolv.conf".into(),
        };
        let s = serde_yaml::to_string(&dns).unwrap();
        let back: DNS = serde_yaml::from_str(&s).unwrap();
        assert_eq!(back, dns);
    }
}
