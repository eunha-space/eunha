//! Mastodon's Chewy indexes (*app/chewy/*): their names, settings,
//! analyzers and mappings, as Chewy sends them when it creates an index.

use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Index {
    Instances,
    Accounts,
    Tags,
    PublicStatuses,
    Statuses,
}

impl Index {
    /// `Mastodon::CLI::Search::INDICES`: smallest first, so that the small
    /// ones are searchable sooner.
    pub const ALL: [Index; 5] = [
        Index::Instances,
        Index::Accounts,
        Index::Tags,
        Index::PublicStatuses,
        Index::Statuses,
    ];

    /// `Scheduler::IndexingScheduler#indexes`: the ones with a queue.
    pub const QUEUED: [Index; 4] = [
        Index::Accounts,
        Index::Tags,
        Index::PublicStatuses,
        Index::Statuses,
    ];

    /// Chewy's `base_name`.
    pub fn base_name(self) -> &'static str {
        match self {
            Index::Instances => "instances",
            Index::Accounts => "accounts",
            Index::Tags => "tags",
            Index::PublicStatuses => "public_statuses",
            Index::Statuses => "statuses",
        }
    }

    /// The class name, which names its queue: `chewy:queue:AccountsIndex`.
    pub fn class_name(self) -> &'static str {
        match self {
            Index::Instances => "InstancesIndex",
            Index::Accounts => "AccountsIndex",
            Index::Tags => "TagsIndex",
            Index::PublicStatuses => "PublicStatusesIndex",
            Index::Statuses => "StatusesIndex",
        }
    }

    /// What `tootctl search deploy --only` calls it.
    pub fn from_option(name: &str) -> Option<Self> {
        Some(match name {
            "instances" => Index::Instances,
            "accounts" => Index::Accounts,
            "tags" => Index::Tags,
            "public_statuses" => Index::PublicStatuses,
            "statuses" => Index::Statuses,
            _ => return None,
        })
    }

    /// The table whose rows are the documents, for `estimate!` and
    /// `clean_up!`.
    pub fn table(self) -> &'static str {
        match self {
            Index::Instances => "instances",
            Index::Accounts => "accounts",
            Index::Tags => "tags",
            Index::PublicStatuses | Index::Statuses => "statuses",
        }
    }

    /// `refresh_interval` and the shard count the index asks for before the
    /// preset adjusts it.
    fn base_options(self) -> (&'static str, Option<u64>) {
        match self {
            Index::PublicStatuses | Index::Statuses => ("30s", Some(5)),
            _ => ("30s", None),
        }
    }

    /// The `refresh_interval` `optimize_for_search!` restores.
    pub fn refresh_interval(self) -> &'static str {
        self.base_options().0
    }

    /// `index_preset(base_options)`: the `index` settings for `ES_PRESET`.
    /// An unknown preset yields none, so the cluster's defaults apply, as
    /// Chewy is given `nil` for it.
    pub fn index_settings(self, preset: Option<&str>) -> Option<Value> {
        let (refresh_interval, shards) = self.base_options();
        let mut settings = serde_json::Map::new();
        settings.insert("refresh_interval".into(), json!(refresh_interval));
        if let Some(shards) = shards {
            settings.insert("number_of_shards".into(), json!(shards));
        }
        match preset {
            None | Some("single_node_cluster") => {
                settings.insert("number_of_replicas".into(), json!(0));
            }
            Some("small_cluster") => {
                settings.insert("number_of_replicas".into(), json!(1));
            }
            Some("large_cluster") => {
                settings.insert("number_of_replicas".into(), json!(1));
                settings.insert("number_of_shards".into(), json!(shards.unwrap_or(1) * 2));
            }
            Some(_) => return None,
        }
        Some(Value::Object(settings))
    }

    /// The `analysis` part of the index settings.
    pub fn analysis(self) -> Option<Value> {
        let english = json!({
            "english_stop": { "type": "stop", "stopwords": "_english_" },
            "english_stemmer": { "type": "stemmer", "language": "english" },
            "english_possessive_stemmer": { "type": "stemmer", "language": "possessive_english" },
        });
        match self {
            Index::Instances => None,
            Index::Accounts => {
                let mut filter = english;
                filter["word_joiner"] = json!({
                    "type": "shingle",
                    "output_unigrams": true,
                    "token_separator": "",
                });
                Some(json!({
                    "filter": filter,
                    "analyzer": {
                        "natural": {
                            "tokenizer": "standard",
                            "filter": [
                                "lowercase", "asciifolding", "cjk_width", "elision",
                                "english_possessive_stemmer", "english_stop", "english_stemmer",
                            ],
                        },
                        "verbatim": {
                            "tokenizer": "standard",
                            "filter": ["lowercase", "asciifolding", "cjk_width"],
                        },
                        "word_join_analyzer": {
                            "type": "custom",
                            "tokenizer": "standard",
                            "filter": ["lowercase", "asciifolding", "cjk_width", "word_joiner"],
                        },
                        "edge_ngram": {
                            "tokenizer": "edge_ngram",
                            "filter": ["lowercase", "asciifolding", "cjk_width"],
                        },
                    },
                    "tokenizer": {
                        "edge_ngram": { "type": "edge_ngram", "min_gram": 1, "max_gram": 15 },
                    },
                }))
            }
            Index::Tags => Some(json!({
                "analyzer": {
                    "content": {
                        "tokenizer": "keyword",
                        "filter": ["word_delimiter_graph", "lowercase", "asciifolding", "cjk_width"],
                    },
                    "edge_ngram": {
                        "tokenizer": "edge_ngram",
                        "filter": ["lowercase", "asciifolding", "cjk_width"],
                    },
                },
                "tokenizer": {
                    "edge_ngram": { "type": "edge_ngram", "min_gram": 2, "max_gram": 15 },
                },
            })),
            Index::PublicStatuses | Index::Statuses => Some(json!({
                "filter": english,
                "analyzer": {
                    "verbatim": { "tokenizer": "uax_url_email", "filter": ["lowercase"] },
                    "content": {
                        "tokenizer": "standard",
                        "filter": [
                            "lowercase", "asciifolding", "cjk_width", "elision",
                            "english_possessive_stemmer", "english_stop", "english_stemmer",
                        ],
                    },
                    "hashtag": {
                        "tokenizer": "keyword",
                        "filter": ["word_delimiter_graph", "lowercase", "asciifolding", "cjk_width"],
                    },
                },
            })),
        }
    }

    /// `settings_hash[:settings]`.
    pub fn settings(self, preset: Option<&str>) -> Value {
        let mut settings = serde_json::Map::new();
        if let Some(index) = self.index_settings(preset) {
            settings.insert("index".into(), index);
        }
        if let Some(analysis) = self.analysis() {
            settings.insert("analysis".into(), analysis);
        }
        Value::Object(settings)
    }

    /// `mappings_hash[:mappings]`.
    pub fn mappings(self) -> Value {
        let text_with = |analyzer: &str, sub: &str, sub_mapping: Value| json!({ "type": "text", "analyzer": analyzer, "fields": { sub: sub_mapping } });
        let edge_ngram = |search_analyzer: &str| json!({ "type": "text", "analyzer": "edge_ngram", "search_analyzer": search_analyzer });
        let properties = match self {
            Index::Instances => json!({
                "domain": { "type": "text", "index_prefixes": { "min_chars": 1, "max_chars": 5 } },
                "accounts_count": { "type": "long" },
            }),
            Index::Accounts => json!({
                "id": { "type": "long" },
                "following_count": { "type": "long" },
                "followers_count": { "type": "long" },
                "properties": { "type": "keyword" },
                "last_status_at": { "type": "date" },
                "display_name": text_with("verbatim", "edge_ngram", edge_ngram("verbatim")),
                "username": text_with("verbatim", "edge_ngram", edge_ngram("verbatim")),
                "text": text_with("verbatim", "stemmed", json!({ "type": "text", "analyzer": "natural" })),
            }),
            Index::Tags => json!({
                "name": text_with("content", "edge_ngram", edge_ngram("content")),
                "reviewed": { "type": "boolean" },
                "usage": { "type": "long" },
                "last_status_at": { "type": "date" },
            }),
            Index::PublicStatuses => json!({
                "id": { "type": "long" },
                "account_id": { "type": "long" },
                "text": text_with("verbatim", "stemmed", json!({ "type": "text", "analyzer": "content" })),
                "tags": { "type": "text", "analyzer": "hashtag" },
                "language": { "type": "keyword" },
                "properties": { "type": "keyword" },
                "created_at": { "type": "date" },
            }),
            Index::Statuses => json!({
                "id": { "type": "long" },
                "account_id": { "type": "long" },
                "text": text_with("verbatim", "stemmed", json!({ "type": "text", "analyzer": "content" })),
                "tags": { "type": "text", "analyzer": "hashtag" },
                "searchable_by": { "type": "long" },
                "language": { "type": "keyword" },
                "properties": { "type": "keyword" },
                "created_at": { "type": "date" },
            }),
        };
        json!({ "date_detection": false, "properties": properties })
    }
}

/// Every scalar as a string, the way the cluster echoes settings back
/// (`"min_gram": "1"`), so that what was sent and what is there compare.
pub fn stringify(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            Value::Object(map.iter().map(|(k, v)| (k.clone(), stringify(v))).collect())
        }
        Value::Array(items) => Value::Array(items.iter().map(stringify).collect()),
        Value::String(s) => Value::String(s.clone()),
        Value::Null => Value::Null,
        other => Value::String(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_decide_replicas_and_shards() {
        let single = Index::Statuses.index_settings(None).unwrap();
        assert_eq!(single["number_of_replicas"], 0);
        assert_eq!(single["number_of_shards"], 5);
        let large = Index::Statuses
            .index_settings(Some("large_cluster"))
            .unwrap();
        assert_eq!(large["number_of_replicas"], 1);
        assert_eq!(large["number_of_shards"], 10);
        let large_accounts = Index::Accounts
            .index_settings(Some("large_cluster"))
            .unwrap();
        assert_eq!(large_accounts["number_of_shards"], 2);
        assert!(Index::Accounts.index_settings(Some("galactic")).is_none());
        assert_eq!(
            Index::Tags.index_settings(Some("small_cluster")).unwrap()["refresh_interval"],
            "30s"
        );
    }

    #[test]
    fn mappings_match_chewy() {
        let accounts = Index::Accounts.mappings();
        assert_eq!(accounts["date_detection"], false);
        assert_eq!(
            accounts["properties"]["username"]["fields"]["edge_ngram"]["search_analyzer"],
            "verbatim"
        );
        assert_eq!(
            Index::Statuses.mappings()["properties"]["searchable_by"]["type"],
            "long"
        );
        assert!(Index::PublicStatuses.mappings()["properties"]
            .get("searchable_by")
            .is_none());
        assert!(Index::Instances.analysis().is_none());
    }

    #[test]
    fn settings_compare_as_the_cluster_echoes_them() {
        let sent = Index::Accounts.analysis().unwrap();
        let echoed = stringify(&sent);
        assert_eq!(echoed["tokenizer"]["edge_ngram"]["min_gram"], "1");
        assert_eq!(echoed["filter"]["word_joiner"]["output_unigrams"], "true");
        assert_eq!(stringify(&echoed), echoed);
    }
}
