/// Per-instance Redis key namespace.
///
/// The separator is added by Eunha so an operator supplies an opaque tenant
/// name (`example`), while the corresponding Redis ACL pattern is the
/// unambiguous `~example:*`. Colons in the prefix are rejected because they
/// make it too easy to configure overlapping ACL patterns accidentally.
#[derive(Clone, Debug, Default)]
pub struct RedisKeyspace {
    prefix: String,
}

impl RedisKeyspace {
    pub fn new(prefix: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !prefix.contains(':'),
            "redis_key_prefix must not contain ':'"
        );
        anyhow::ensure!(
            prefix
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')),
            "redis_key_prefix may contain only ASCII letters, digits, '-' and '_'"
        );
        Ok(Self {
            prefix: prefix.to_owned(),
        })
    }

    pub fn key(&self, key: impl AsRef<str>) -> String {
        if self.prefix.is_empty() {
            key.as_ref().to_owned()
        } else {
            format!("{}:{}", self.prefix, key.as_ref())
        }
    }

    /// `key` without this keyspace's prefix, or `None` if it is not in the
    /// keyspace.
    pub fn strip<'a>(&self, key: &'a str) -> Option<&'a str> {
        if self.prefix.is_empty() {
            Some(key)
        } else {
            key.strip_prefix(self.prefix.as_str())?.strip_prefix(':')
        }
    }

    pub fn is_shared(&self) -> bool {
        !self.prefix.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::RedisKeyspace;

    #[test]
    fn empty_prefix_preserves_standalone_keys() {
        assert_eq!(
            RedisKeyspace::new("").unwrap().key("feed:home:1"),
            "feed:home:1"
        );
    }

    #[test]
    fn tenant_prefixes_are_disjoint() {
        let a = RedisKeyspace::new("tenant-a").unwrap();
        let b = RedisKeyspace::new("tenant-b").unwrap();
        assert_eq!(a.key("feed:home:1"), "tenant-a:feed:home:1");
        assert_eq!(b.key("feed:home:1"), "tenant-b:feed:home:1");
        assert_ne!(a.key("feed:home:1"), b.key("feed:home:1"));
    }

    #[test]
    fn prefixes_cannot_overlap_by_delimiter() {
        assert!(RedisKeyspace::new("tenant:child").is_err());
        assert!(RedisKeyspace::new("tenant*").is_err());
    }
}
