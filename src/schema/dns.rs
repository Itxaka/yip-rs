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
    use pretty_assertions::assert_eq;

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

    #[test]
    fn minimal_yaml_only_nameservers() {
        let y = indoc! {r#"
            nameservers: [9.9.9.9]
        "#};
        let dns: DNS = serde_yaml::from_str(y).unwrap();
        assert_eq!(dns.nameservers, vec!["9.9.9.9"]);
        assert!(dns.dns_search.is_empty());
        assert!(dns.dns_options.is_empty());
        assert!(dns.path.is_empty());
    }

    #[test]
    fn yaml_aliases_match_go_tags() {
        // Go uses `search` and `options` (not `dns_search` / `dns_options`).
        let y = indoc! {r#"
            search: [a.example, b.example]
            options: [ndots:1, timeout:3]
        "#};
        let dns: DNS = serde_yaml::from_str(y).unwrap();
        assert_eq!(dns.dns_search, vec!["a.example", "b.example"]);
        assert_eq!(dns.dns_options, vec!["ndots:1", "timeout:3"]);
        // Round-trip must use the YAML keys, not the field names.
        let s = serde_yaml::to_string(&dns).unwrap();
        assert!(s.contains("search"));
        assert!(s.contains("options"));
        assert!(!s.contains("dns_search"));
        assert!(!s.contains("dns_options"));
    }

    #[test]
    fn maximal_roundtrip_all_fields() {
        let dns = DNS {
            nameservers: vec!["1.1.1.1".into(), "8.8.8.8".into(), "9.9.9.9".into()],
            dns_search: vec!["corp".into(), "lan".into()],
            dns_options: vec!["timeout:1".into(), "ndots:2".into()],
            path: "/etc/resolv.conf".into(),
        };
        let s = serde_yaml::to_string(&dns).unwrap();
        let back: DNS = serde_yaml::from_str(&s).unwrap();
        assert_eq!(back, dns);
    }

    #[test]
    fn default_omits_all_keys() {
        let s = serde_yaml::to_string(&DNS::default()).unwrap();
        assert!(!s.contains("nameservers"));
        assert!(!s.contains("search"));
        assert!(!s.contains("options"));
        assert!(!s.contains("path"));
    }

    #[test]
    fn ipv6_nameservers_parse_as_strings() {
        // Edge case: yip treats nameservers as opaque strings, so IPv6
        // (with colons) must parse fine.
        let y = indoc! {r#"
            nameservers:
              - "2001:4860:4860::8888"
              - "fe80::1"
        "#};
        let dns: DNS = serde_yaml::from_str(y).unwrap();
        assert_eq!(
            dns.nameservers,
            vec!["2001:4860:4860::8888", "fe80::1"]
        );
    }
}
