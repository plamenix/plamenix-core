//! Plugin event bus (I6.1) — bookkeeping side of the WIT
//! `event-bus` interface.
//!
//! Two cooperating event-bus implementations exist in the Plamenix
//! runtime: the TypeScript `EventBus` in `@plamenix/ui` serves
//! React-side plugins; this Rust bus serves WASM Component-Model
//! plugins. Both share the same topic grammar + subscription
//! semantics; emits on one side bridge to the other via the host
//! wiring landing in I6.2-I6.11 (one bridge per event family).
//!
//! **What this module ships today (I6.1 scope)**:
//!
//! - Topic-pattern matcher (`matches_pattern`) — slash-separated
//!   segments with `*` (single segment) and `**` (zero or more
//!   trailing segments) glob support.
//! - `EventBus` struct holding the subscription registry — plugins
//!   register at manifest-parse time via
//!   `[[contributions.event_subscriptions]]` entries, the host calls
//!   `bus.subscribe(plugin_id, pattern)` for each.
//! - `bus.emit(topic, payload)` returns the list of `(plugin_id,
//!   pattern)` pairs whose subscription matches — the **actual call
//!   back into the plugin's exported `handle-event`** is deferred to
//!   I6.2 (which adds the export to the WIT) + I6.3-I6.11 (which
//!   wire individual emit sites). For I6.1 the bus is the
//!   bookkeeping layer; downstream consumers query it to learn who
//!   to dispatch to.
//! - `bus.unsubscribe_by_plugin(plugin_id)` — drops every
//!   subscription a plugin registered. Called by the supervisor
//!   when a plugin deactivates so its subscriptions don't outlive
//!   its lifetime.
//!
//! **Deferred from I6.1 scope** (explicitly):
//!
//! - WIT `handle-event` plugin export. Adding it triggers
//!   wit-bindgen regen across all 3 WIT dirs + bindings.rs updates +
//!   hello-plugin impl. Lands in I6.2 alongside the topic registry
//!   wiring.
//! - Cross-bus bridging (TS ↔ Rust). Each emit site (I6.3-I6.11)
//!   decides whether the event originates on the Rust side (driver
//!   events) or the TS side (UI events); the bridge handler shape
//!   lands per-emit-site.

use std::sync::Mutex;

/// One subscription entry: a plugin's id + the pattern it
/// registered. Tokenised at registration so per-emit dispatch only
/// pays the segment-walk cost.
#[derive(Clone, Debug)]
struct Subscription {
    plugin_id: String,
    pattern: String,
    segments: Vec<String>,
}

/// Matched subscriber returned by [`EventBus::emit`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MatchedSubscriber {
    /// Plugin id that owns the subscription.
    pub plugin_id: String,
    /// Pattern the plugin registered (useful when the same plugin
    /// holds multiple subscriptions that all match a single emit —
    /// each match surfaces independently).
    pub pattern: String,
}

/// Topic-pattern matcher. Returns `true` when `topic` matches the
/// pattern represented by `pattern_segments` (the pre-split form of
/// a `'/'`-separated pattern with optional `*` / `**` wildcards).
///
/// - `*` matches exactly one topic segment.
/// - `**` matches zero or more trailing segments; MUST be the last
///   pattern segment to be unambiguous.
/// - Literal segments require an exact text match.
///
/// Empty topics or empty pattern segment-lists never match.
#[must_use]
pub fn matches_pattern(pattern_segments: &[String], topic: &str) -> bool {
    if topic.is_empty() {
        return false;
    }
    let topic_segments: Vec<&str> = topic.split('/').collect();
    for (i, p) in pattern_segments.iter().enumerate() {
        if p == "**" {
            return i == pattern_segments.len() - 1;
        }
        let Some(t) = topic_segments.get(i) else {
            return false;
        };
        if p == "*" {
            continue;
        }
        if p != t {
            return false;
        }
    }
    pattern_segments.len() == topic_segments.len()
}

/// Tokenises a pattern into slash-separated segments. Empty patterns
/// produce an error; trailing slashes are not stripped (so
/// `'query/'` would not match `'query'`).
///
/// # Errors
///
/// Returns an error when the pattern is empty.
pub fn tokenise_pattern(pattern: &str) -> Result<Vec<String>, String> {
    if pattern.is_empty() {
        return Err("event pattern must be a non-empty string".into());
    }
    Ok(pattern.split('/').map(String::from).collect())
}

/// In-process event bus for WASM-side plugins. Each plugin host
/// owns one instance; the supervisor calls `subscribe` for every
/// `[[contributions.event_subscriptions]]` entry at plugin
/// activation, `emit` from each event-source site, and
/// `unsubscribe_by_plugin` at deactivation.
///
/// Thread-safety: subscription state lives behind a `Mutex` so the
/// bus can be shared via `Arc` across the supervisor + emit sites.
/// Lock holds are short (subscription registration or per-emit
/// matcher walk).
#[derive(Debug, Default)]
pub struct EventBus {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    subscriptions: Vec<Subscription>,
}

impl EventBus {
    /// Returns an empty bus.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a subscription. Returns an error when the pattern
    /// can't be tokenised.
    ///
    /// # Errors
    ///
    /// Returns the error from [`tokenise_pattern`] when `pattern` is
    /// invalid.
    pub fn subscribe(&self, plugin_id: impl Into<String>, pattern: impl Into<String>) -> Result<(), String> {
        let pattern: String = pattern.into();
        let segments = tokenise_pattern(&pattern)?;
        let plugin_id: String = plugin_id.into();
        if plugin_id.is_empty() {
            return Err("plugin_id must be a non-empty string".into());
        }
        let mut guard = self
            .inner
            .lock()
            .map_err(|err| format!("event-bus mutex poisoned: {err}"))?;
        guard.subscriptions.push(Subscription {
            plugin_id,
            pattern,
            segments,
        });
        Ok(())
    }

    /// Returns the list of `(plugin_id, pattern)` pairs whose
    /// subscription matches `topic`. Order: subscription
    /// registration order (FIFO).
    ///
    /// The caller is responsible for dispatching `handle-event` to
    /// each matched plugin — for I6.1 this lookup is the only piece
    /// of the dispatch flow that exists; the WIT export + the
    /// per-plugin instance lookup land in I6.2.
    #[must_use]
    pub fn emit(&self, topic: &str) -> Vec<MatchedSubscriber> {
        let Ok(guard) = self.inner.lock() else {
            return Vec::new();
        };
        guard
            .subscriptions
            .iter()
            .filter(|s| matches_pattern(&s.segments, topic))
            .map(|s| MatchedSubscriber {
                plugin_id: s.plugin_id.clone(),
                pattern: s.pattern.clone(),
            })
            .collect()
    }

    /// Drops every subscription registered by `plugin_id`. Returns
    /// the number of subscriptions removed.
    pub fn unsubscribe_by_plugin(&self, plugin_id: &str) -> usize {
        let Ok(mut guard) = self.inner.lock() else {
            return 0;
        };
        let before = guard.subscriptions.len();
        guard.subscriptions.retain(|s| s.plugin_id != plugin_id);
        before - guard.subscriptions.len()
    }

    /// Total subscription count across all plugins. Exposed for the
    /// supervisor's "X subscriptions held" debug summary + tests.
    #[must_use]
    pub fn subscription_count(&self) -> usize {
        self.inner.lock().map(|g| g.subscriptions.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segs(pattern: &str) -> Vec<String> {
        tokenise_pattern(pattern).expect("valid pattern")
    }

    #[test]
    fn matches_exact_topic() {
        assert!(matches_pattern(&segs("query/executed"), "query/executed"));
        assert!(!matches_pattern(&segs("query/executed"), "query/failed"));
    }

    #[test]
    fn single_star_matches_one_segment() {
        assert!(matches_pattern(&segs("query/*"), "query/executed"));
        assert!(matches_pattern(&segs("query/*"), "query/failed"));
        assert!(!matches_pattern(&segs("query/*"), "query/executed/extra"));
    }

    #[test]
    fn double_star_matches_zero_or_more_trailing_segments() {
        // `**` as the only segment matches everything.
        assert!(matches_pattern(&segs("**"), "anything"));
        assert!(matches_pattern(&segs("**"), "anything/deeper"));
        // `connection/**` matches every connection-family event.
        assert!(matches_pattern(&segs("connection/**"), "connection/opened"));
        assert!(matches_pattern(&segs("connection/**"), "connection/health-changed"));
        // But not unrelated topics.
        assert!(!matches_pattern(&segs("connection/**"), "tab/opened"));
    }

    #[test]
    fn empty_topic_never_matches() {
        assert!(!matches_pattern(&segs("**"), ""));
    }

    #[test]
    fn pattern_longer_than_topic_does_not_match() {
        assert!(!matches_pattern(&segs("a/b/c"), "a/b"));
    }

    #[test]
    fn tokenise_rejects_empty_pattern() {
        assert!(tokenise_pattern("").is_err());
    }

    #[test]
    fn subscribe_then_emit_returns_matching_plugin() {
        let bus = EventBus::new();
        bus.subscribe("com.example.audit", "query/executed").unwrap();
        let matches = bus.emit("query/executed");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].plugin_id, "com.example.audit");
        assert_eq!(matches[0].pattern, "query/executed");
    }

    #[test]
    fn emit_returns_empty_when_no_subscribers_match() {
        let bus = EventBus::new();
        bus.subscribe("com.example.audit", "query/executed").unwrap();
        assert!(bus.emit("query/failed").is_empty());
    }

    #[test]
    fn wildcard_subscription_matches_multiple_emits() {
        let bus = EventBus::new();
        bus.subscribe("com.example.observability", "query/*").unwrap();
        assert_eq!(bus.emit("query/executed").len(), 1);
        assert_eq!(bus.emit("query/failed").len(), 1);
        assert!(bus.emit("connection/opened").is_empty());
    }

    #[test]
    fn multiple_plugins_each_get_matched_independently() {
        let bus = EventBus::new();
        bus.subscribe("com.example.a", "**").unwrap();
        bus.subscribe("com.example.b", "query/*").unwrap();
        let matches = bus.emit("query/executed");
        assert_eq!(matches.len(), 2);
        let ids: Vec<&str> = matches.iter().map(|m| m.plugin_id.as_str()).collect();
        assert!(ids.contains(&"com.example.a"));
        assert!(ids.contains(&"com.example.b"));
    }

    #[test]
    fn unsubscribe_by_plugin_drops_all_subs_for_plugin() {
        let bus = EventBus::new();
        bus.subscribe("com.example.a", "query/*").unwrap();
        bus.subscribe("com.example.a", "connection/*").unwrap();
        bus.subscribe("com.example.b", "query/*").unwrap();
        assert_eq!(bus.subscription_count(), 3);
        let dropped = bus.unsubscribe_by_plugin("com.example.a");
        assert_eq!(dropped, 2);
        assert_eq!(bus.subscription_count(), 1);
        // Only com.example.b's subscription remains.
        let matches = bus.emit("query/executed");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].plugin_id, "com.example.b");
    }

    #[test]
    fn empty_plugin_id_rejected() {
        let bus = EventBus::new();
        assert!(bus.subscribe("", "query/*").is_err());
    }

    #[test]
    fn emit_preserves_subscription_registration_order() {
        let bus = EventBus::new();
        bus.subscribe("first", "**").unwrap();
        bus.subscribe("second", "**").unwrap();
        bus.subscribe("third", "**").unwrap();
        let matches = bus.emit("anything");
        let ids: Vec<&str> = matches.iter().map(|m| m.plugin_id.as_str()).collect();
        assert_eq!(ids, vec!["first", "second", "third"]);
    }
}
