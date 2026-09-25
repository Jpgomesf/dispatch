//! Loop detection: the same tool call (name and input, key order ignored) `threshold` times
//! within the last `2 * threshold` calls of one agent context (the main thread, or one
//! subagent by its `parent_tool_use_id`) means the session is going round in circles.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};

use serde_json::Value;

/// `value` with object keys sorted at every level, so equal inputs print alike.
fn canonical(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let fields: Vec<String> = entries
                .into_iter()
                .map(|(key, value)| format!("{}:{}", Value::String(key.clone()), canonical(value)))
                .collect();
            format!("{{{}}}", fields.join(","))
        }
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(canonical).collect();
            format!("[{}]", items.join(","))
        }
        other => other.to_string(),
    }
}

fn fingerprint(name: &str, input: &Value) -> u64 {
    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    canonical(input).hash(&mut hasher);
    hasher.finish()
}

#[derive(Debug, Default)]
pub struct Loops {
    threshold: usize,
    /// Recent fingerprints per agent context.
    windows: HashMap<String, VecDeque<u64>>,
}

impl Loops {
    /// `threshold` 0 switches detection off.
    pub fn new(threshold: u32) -> Self {
        Loops {
            threshold: usize::try_from(threshold).unwrap_or(usize::MAX),
            windows: HashMap::new(),
        }
    }

    /// One tool call in `context`; why the session is looping, if this call shows it.
    pub fn observe(&mut self, context: &str, name: &str, input: &Value) -> Option<String> {
        if self.threshold == 0 {
            return None;
        }
        let print = fingerprint(name, input);
        let window = self.windows.entry(context.to_string()).or_default();
        window.push_back(print);
        if window.len() > self.threshold.saturating_mul(2) {
            window.pop_front();
        }
        let repeats = window.iter().filter(|p| **p == print).count();
        (repeats >= self.threshold).then(|| {
            format!(
                "{name} called {repeats} times with the same input in the last {} tool calls",
                window.len()
            )
        })
    }

    /// Every `tool_use` of an `assistant` event; the first loop found, if any.
    pub fn observe_event(&mut self, event: &Value) -> Option<String> {
        let context = event["parent_tool_use_id"].as_str().unwrap_or("");
        event["message"]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|block| block["type"] == "tool_use")
            .find_map(|block| {
                let name = block["name"].as_str().unwrap_or("?");
                self.observe(context, name, &block["input"])
            })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_threshold_th_identical_call_in_the_window_is_a_loop() {
        let mut loops = Loops::new(3);
        let read = json!({"file_path": "/tmp/example/a.rs", "limit": 10});
        assert_eq!(loops.observe("", "Read", &read), None);
        assert_eq!(loops.observe("", "Bash", &json!({"command": "ls"})), None);
        assert_eq!(loops.observe("", "Read", &read), None);
        let reordered = json!({"limit": 10, "file_path": "/tmp/example/a.rs"});
        assert_eq!(
            loops.observe("", "Read", &reordered).as_deref(),
            Some("Read called 3 times with the same input in the last 4 tool calls"),
            "key order does not matter"
        );
    }

    #[test]
    fn calls_spread_beyond_the_window_or_across_agents_are_not_a_loop() {
        let mut loops = Loops::new(2);
        let call = json!({"command": "git status"});
        assert_eq!(loops.observe("", "Bash", &call), None);
        for n in 0..4 {
            assert_eq!(loops.observe("", "Bash", &json!({"command": n})), None);
        }
        assert_eq!(loops.observe("", "Bash", &call), None, "the first fell out");
        assert_eq!(
            loops.observe("toolu_1", "Bash", &call),
            None,
            "another agent"
        );
        assert!(loops.observe("", "Bash", &call).is_some());
        let mut off = Loops::new(0);
        for _ in 0..50 {
            assert_eq!(off.observe("", "Bash", &call), None);
        }
    }

    #[test]
    fn assistant_events_are_read_per_context() {
        let mut loops = Loops::new(2);
        let event = |parent: Value| {
            json!({"type": "assistant", "parent_tool_use_id": parent,
            "message": {"content": [
                {"type": "text", "text": "checking"},
                {"type": "tool_use", "id": "t", "name": "Grep", "input": {"pattern": "x"}}
            ]}})
        };
        assert_eq!(loops.observe_event(&event(Value::Null)), None);
        assert_eq!(loops.observe_event(&event(json!("toolu_9"))), None);
        assert!(loops.observe_event(&event(Value::Null)).is_some());
        assert_eq!(loops.observe_event(&json!({"type": "assistant"})), None);
    }
}
