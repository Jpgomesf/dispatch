//! Reading `claude -p --output-format stream-json --verbose`: one JSON event per line. The
//! stream names the session (`session_id` on every event, first on `system/init`) and ends
//! with a `result` event carrying `subtype`, `is_error`, `structured_output`,
//! `total_cost_usd` and `api_error_status`. Usage and rate limits show as a `system/api_retry`
//! with `error: "rate_limit"`, a `rate_limit_event` whose `rate_limit_info.status` is
//! `rejected` (with `resetsAt`, unix seconds), an `assistant` message with `error:
//! "rate_limit"`, or a result with `api_error_status: 429`. Pure logic, so it is tested with
//! scripted lines.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::loops::Loops;
use super::{Ended, Limits, Notice, SessionReport};
use crate::durations::format_duration;

/// Error results that are the session's own limits, never a usage limit.
const OWN_LIMITS: [&str; 3] = [
    "error_max_budget_usd",
    "error_max_turns",
    "error_max_structured_output_retries",
];

/// Why the runner stopped a session before it ended by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// Kill switch or signal.
    Interrupted,
    /// The wall-clock limit passed.
    Timeout(Duration),
    /// No stream event for this long.
    Idle(Duration),
    /// `system/init` showed the session started without what it needs.
    Environment(String),
    /// The same tool call over and over.
    Loop(String),
}

/// What one line changed.
#[derive(Debug, Default, PartialEq)]
pub struct Observed {
    pub notices: Vec<Notice>,
    pub stop: Option<Stop>,
}

#[derive(Debug, Default)]
pub struct Stream {
    /// MCP servers that must be connected at `system/init`.
    required_mcp: Vec<String>,
    loops: Loops,
    init_seen: bool,
    session_id: Option<String>,
    result: Option<Value>,
    /// A usage or rate limit was hit; the session may still be retrying.
    rate_limited: bool,
    /// When the limit resets, if an event said.
    resets_at: Option<DateTime<Utc>>,
}

/// What is wrong with the environment `system/init` describes: plugin load errors (the key
/// is omitted when there are none) and required MCP servers that are missing or not
/// `connected` (`failed`, `needs-auth`, `pending`, `disabled`).
fn environment_problems(init: &Value, required_mcp: &[String]) -> Vec<String> {
    let mut problems: Vec<String> = init["plugin_errors"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|error| {
            format!(
                "plugin {} did not load: {}",
                error["plugin"].as_str().unwrap_or("?"),
                error["message"].as_str().unwrap_or("?")
            )
        })
        .collect();
    let servers = init["mcp_servers"].as_array();
    for name in required_mcp {
        let status = servers
            .and_then(|list| list.iter().find(|server| server["name"] == name.as_str()))
            .map(|server| server["status"].as_str().unwrap_or("unknown"));
        match status {
            Some("connected") => {}
            Some(status) => problems.push(format!("MCP server {name} is {status}")),
            None => problems.push(format!("MCP server {name} is missing")),
        }
    }
    problems
}

impl Stream {
    pub fn new(limits: &Limits) -> Self {
        Stream {
            required_mcp: limits.required_mcp.clone(),
            loops: Loops::new(limits.loop_threshold),
            ..Stream::default()
        }
    }

    /// Take one stdout line. Lines that are not JSON objects (warnings) are ignored.
    pub fn observe(&mut self, line: &str) -> Observed {
        let Ok(event) = serde_json::from_str::<Value>(line.trim()) else {
            return Observed::default();
        };
        let mut observed = Observed::default();
        if self.session_id.is_none()
            && let Some(id) = event["session_id"].as_str().filter(|id| !id.is_empty())
        {
            self.session_id = Some(id.to_string());
            observed.notices.push(Notice::Started {
                session_id: id.to_string(),
            });
        }
        if !self.init_seen && event["type"] == "system" && event["subtype"] == "init" {
            self.init_seen = true;
            let problems = environment_problems(&event, &self.required_mcp);
            if !problems.is_empty() {
                observed.stop = Some(Stop::Environment(format!(
                    "claude started without its environment: {}",
                    problems.join("; ")
                )));
            }
        }
        let limit = match (event["type"].as_str(), event["subtype"].as_str()) {
            (Some("system"), Some("api_retry")) => (event["error"] == "rate_limit").then_some(None),
            (Some("rate_limit_event"), _) => {
                let info = &event["rate_limit_info"];
                (info["status"] == "rejected").then(|| {
                    info["resetsAt"]
                        .as_i64()
                        .and_then(|secs| DateTime::from_timestamp(secs, 0))
                })
            }
            (Some("assistant"), _) => (event["error"] == "rate_limit").then_some(None),
            _ => None,
        };
        if let Some(resets_at) = limit {
            observed.notices.extend(self.rate_limit(resets_at));
        } else if event["type"] == "assistant" && event["error"].is_null() {
            // The API answered again: a limit hit earlier no longer explains how the session
            // ends (the pause it set stands).
            self.rate_limited = false;
            self.resets_at = None;
        }
        if event["type"] == "assistant"
            && let Some(detail) = self.loops.observe_event(&event)
        {
            observed.stop.get_or_insert(Stop::Loop(detail));
        }
        if event["type"] == "result" {
            if event["api_error_status"] == 429 {
                observed.notices.extend(self.rate_limit(None));
            }
            self.result = Some(event);
        }
        observed
    }

    /// Record a rate limit; a notice the first time, and whenever a later reset is learned.
    fn rate_limit(&mut self, resets_at: Option<DateTime<Utc>>) -> Option<Notice> {
        let first = !self.rate_limited;
        self.rate_limited = true;
        let later = resets_at.filter(|at| self.resets_at.is_none_or(|known| *at > known));
        if later.is_some() {
            self.resets_at = later;
        }
        (first || later.is_some()).then_some(Notice::RateLimited {
            resets_at: self.resets_at,
        })
    }

    /// How the session ended, from what the stream said and why the runner stopped it (if it
    /// did). A valid structured output always wins: the work was finished.
    pub fn finish(self, stop: Option<Stop>, exited: &str) -> SessionReport {
        let cost_usd = self
            .result
            .as_ref()
            .and_then(|r| r["total_cost_usd"].as_f64());
        let ended = self.ended(stop, exited);
        SessionReport {
            ended,
            session_id: self.session_id,
            cost_usd,
        }
    }

    fn ended(&self, stop: Option<Stop>, exited: &str) -> Ended {
        if let Some(output) = self.result.as_ref().and_then(valid_output) {
            return Ended::Output(output.clone());
        }
        match stop {
            Some(Stop::Interrupted) => return Ended::Interrupted,
            Some(Stop::Environment(detail)) => return Ended::Environment(detail),
            Some(Stop::Loop(detail)) => return Ended::Stuck(detail),
            _ => {}
        }
        // A limit hit mid-session explains the failure, unless the session ran into its own
        // budget or turn limit.
        let own_limit = self
            .result
            .as_ref()
            .is_some_and(|r| OWN_LIMITS.contains(&r["subtype"].as_str().unwrap_or("")));
        if self.rate_limited && !own_limit {
            return Ended::RateLimited {
                resets_at: self.resets_at,
            };
        }
        match stop {
            Some(Stop::Timeout(limit)) => {
                return Ended::Timeout(format!("no result within {}", format_duration(limit)));
            }
            Some(Stop::Idle(limit)) => {
                return Ended::Stuck(format!("no stream event for {}", format_duration(limit)));
            }
            Some(Stop::Interrupted | Stop::Environment(_) | Stop::Loop(_)) | None => {}
        }
        match &self.result {
            Some(result) if is_error(result) => Ended::ApiError(error_detail(result)),
            Some(_) => Ended::InvalidOutput("success without structured_output".into()),
            None => Ended::Crash(format!("no result: {exited}")),
        }
    }
}

fn is_error(result: &Value) -> bool {
    result["is_error"] == Value::Bool(true) || result["subtype"] != "success"
}

/// `structured_output` of a successful result, when it is an object.
fn valid_output(result: &Value) -> Option<&Value> {
    let output = &result["structured_output"];
    (!is_error(result) && output.is_object()).then_some(output)
}

/// `errors`, else the result text, else the subtype.
fn error_detail(result: &Value) -> String {
    let errors: Vec<&str> = result["errors"]
        .as_array()
        .map(|list| list.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !errors.is_empty() {
        return errors.join("; ");
    }
    let subtype = result["subtype"].as_str().unwrap_or("error");
    match result["result"]
        .as_str()
        .filter(|text| !text.trim().is_empty())
    {
        Some(text) => format!("{subtype}: {text}"),
        None => subtype.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn init() -> String {
        json!({"type": "system", "subtype": "init", "session_id": "session-a",
               "mcp_servers": [], "plugins": []})
        .to_string()
    }

    pub(crate) fn result(overrides: Value) -> String {
        let mut base = json!({
            "type": "result", "subtype": "success", "is_error": false,
            "total_cost_usd": 0.42, "structured_output": {"summary": "ok"},
            "result": "{\"summary\":\"ok\"}", "session_id": "session-a"
        });
        for (key, value) in overrides.as_object().unwrap() {
            base[key] = value.clone();
        }
        base.to_string()
    }

    fn limits_with(required_mcp: Vec<String>) -> Limits {
        Limits {
            timeout: Duration::from_secs(3600),
            idle_timeout: Duration::from_secs(900),
            loop_threshold: 3,
            required_mcp,
        }
    }

    fn tool_use(name: &str, input: Value) -> String {
        json!({"type": "assistant", "session_id": "session-a", "parent_tool_use_id": null,
               "message": {"content": [{"type": "tool_use", "id": "t", "name": name,
                                        "input": input}]}})
        .to_string()
    }

    #[test]
    fn repeating_the_same_tool_call_stops_the_session_as_stuck() {
        let mut stream = Stream::new(&limits_with(Vec::new()));
        stream.observe(&init());
        let call = tool_use("Bash", json!({"command": "cargo test"}));
        assert_eq!(stream.observe(&call).stop, None);
        assert_eq!(stream.observe(&call).stop, None);
        let Some(Stop::Loop(detail)) = stream.observe(&call).stop else {
            panic!("third identical call with a threshold of 3");
        };
        assert_eq!(
            detail,
            "Bash called 3 times with the same input in the last 3 tool calls"
        );
        assert_eq!(
            stream.finish(Some(Stop::Loop(detail.clone())), "").ended,
            Ended::Stuck(detail)
        );
    }

    fn run(lines: &[String], stop: Option<Stop>) -> SessionReport {
        let mut stream = Stream::new(&limits_with(Vec::new()));
        for line in lines {
            stream.observe(line);
        }
        stream.finish(stop, "exit status: 0")
    }

    #[test]
    fn the_first_session_id_is_announced_once() {
        let mut stream = Stream::new(&limits_with(Vec::new()));
        assert_eq!(stream.observe("warning: not json"), Observed::default());
        let first = stream.observe(&init());
        assert_eq!(
            first.notices,
            [Notice::Started {
                session_id: "session-a".into()
            }]
        );
        assert_eq!(stream.observe(&init()), Observed::default());
    }

    fn api_retry(error: &str) -> String {
        json!({"type": "system", "subtype": "api_retry", "attempt": 1, "max_retries": 10,
               "retry_delay_ms": 5000, "error_status": 429, "error": error,
               "session_id": "session-a"})
        .to_string()
    }

    fn rate_limit_event(status: &str, resets_at: i64) -> String {
        json!({"type": "rate_limit_event", "session_id": "session-a",
               "rate_limit_info": {"status": status, "resetsAt": resets_at,
                                   "rateLimitType": "five_hour"}})
        .to_string()
    }

    fn at(secs: i64) -> Option<DateTime<Utc>> {
        DateTime::from_timestamp(secs, 0)
    }

    #[test]
    fn rate_limits_are_announced_with_the_latest_reset() {
        let mut stream = Stream::new(&limits_with(Vec::new()));
        stream.observe(&init());
        let limited = |resets_at| Notice::RateLimited { resets_at };
        assert!(stream.observe(&api_retry("overloaded")).notices.is_empty());
        assert!(
            stream
                .observe(&rate_limit_event("allowed", 1_790_305_200))
                .notices
                .is_empty(),
            "an allowed status is only information"
        );
        assert_eq!(
            stream.observe(&api_retry("rate_limit")).notices,
            [limited(None)]
        );
        assert!(stream.observe(&api_retry("rate_limit")).notices.is_empty());
        let rejected = rate_limit_event("rejected", 1_790_305_200);
        assert_eq!(
            stream.observe(&rejected).notices,
            [limited(at(1_790_305_200))]
        );
        assert!(stream.observe(&rejected).notices.is_empty(), "nothing new");
        let assistant = json!({"type": "assistant", "error": "rate_limit",
                               "message": {"content": []}})
        .to_string();
        assert!(stream.observe(&assistant).notices.is_empty());
        let report = stream.finish(None, "exit status: 1");
        assert_eq!(
            report.ended,
            Ended::RateLimited {
                resets_at: at(1_790_305_200)
            }
        );
    }

    #[test]
    fn a_limit_the_session_recovered_from_explains_nothing() {
        let answer = json!({"type": "assistant", "session_id": "session-a",
                            "message": {"content": [{"type": "text", "text": "Reading."}]}})
        .to_string();
        let hours = Duration::from_secs(3 * 3600);
        let lines = [init(), api_retry("rate_limit"), answer];
        assert_eq!(
            run(&lines, Some(Stop::Timeout(hours))).ended,
            Ended::Timeout("no result within 3h".into()),
            "a counted timeout, not a limit"
        );
    }

    #[test]
    fn a_limit_explains_a_failed_or_stopped_session_but_not_its_own_budget() {
        let assistant = json!({"type": "assistant", "error": "rate_limit",
                               "message": {"content": [{"type": "text",
                                                        "text": "You've hit your limit"}]}})
        .to_string();
        let limited = Ended::RateLimited { resets_at: None };
        // The usage-limit stop that looks like a clean completion: no structured output.
        let success = result(json!({"structured_output": null, "result": "You've hit your limit"}));
        assert_eq!(
            run(&[init(), assistant.clone(), success], None).ended,
            limited
        );
        let http = result(json!({"is_error": true, "api_error_status": 429,
                                 "structured_output": null}));
        assert_eq!(run(&[init(), http], None).ended, limited);
        let hours = Duration::from_secs(3 * 3600);
        assert_eq!(
            run(
                &[init(), api_retry("rate_limit")],
                Some(Stop::Timeout(hours))
            )
            .ended,
            limited,
            "still retrying when the timeout came"
        );
        let budget = result(json!({"is_error": true, "subtype": "error_max_budget_usd",
                                   "errors": ["Reached maximum budget"], "structured_output": null}));
        assert_eq!(
            run(&[init(), api_retry("rate_limit"), budget], None).ended,
            Ended::ApiError("Reached maximum budget".into())
        );
        assert!(matches!(
            run(&[init(), api_retry("rate_limit"), result(json!({}))], None).ended,
            Ended::Output(_)
        ));
        assert_eq!(
            run(&[init(), api_retry("rate_limit")], Some(Stop::Interrupted)).ended,
            Ended::Interrupted
        );
    }

    #[test]
    fn a_successful_result_carries_output_cost_and_session() {
        let report = run(&[init(), "noise".into(), result(json!({}))], None);
        assert_eq!(report.ended, Ended::Output(json!({"summary": "ok"})));
        assert_eq!(report.cost_usd, Some(0.42));
        assert_eq!(report.session_id.as_deref(), Some("session-a"));
    }

    #[test]
    fn error_results_are_api_errors() {
        let budget = result(json!({
            "is_error": true, "subtype": "error_max_budget_usd",
            "errors": ["Reached maximum budget ($0.05)"], "structured_output": null
        }));
        assert_eq!(
            run(&[init(), budget], None).ended,
            Ended::ApiError("Reached maximum budget ($0.05)".into())
        );
        let text = result(json!({"is_error": true, "result": "API Error: 500",
                                 "structured_output": null}));
        assert_eq!(
            run(&[text], None).ended,
            Ended::ApiError("success: API Error: 500".into())
        );
        let subtype = result(json!({"subtype": "error_during_execution", "result": ""}));
        assert_eq!(
            run(&[subtype], None).ended,
            Ended::ApiError("error_during_execution".into())
        );
    }

    #[test]
    fn success_without_structured_output_is_invalid_output() {
        for output in [json!(null), json!("plain text"), json!([1])] {
            let line = result(json!({"structured_output": output}));
            assert!(matches!(run(&[line], None).ended, Ended::InvalidOutput(_)));
        }
    }

    #[test]
    fn no_result_is_a_crash_and_an_interrupt_is_not() {
        let crashed = run(&[init()], None);
        assert_eq!(
            crashed.ended,
            Ended::Crash("no result: exit status: 0".into())
        );
        assert_eq!(crashed.session_id.as_deref(), Some("session-a"));
        assert_eq!(
            run(&[init()], Some(Stop::Interrupted)).ended,
            Ended::Interrupted
        );
        let finished = run(&[init(), result(json!({}))], Some(Stop::Interrupted));
        assert!(
            matches!(finished.ended, Ended::Output(_)),
            "a finished result wins over the stop"
        );
    }

    #[test]
    fn limits_end_as_timeout_or_stuck() {
        let hours = Duration::from_secs(3 * 3600);
        assert_eq!(
            run(&[init()], Some(Stop::Timeout(hours))).ended,
            Ended::Timeout("no result within 3h".into())
        );
        let idle = Duration::from_secs(15 * 60);
        assert_eq!(
            run(&[init()], Some(Stop::Idle(idle))).ended,
            Ended::Stuck("no stream event for 15m".into())
        );
        let error = result(json!({"is_error": true, "structured_output": null}));
        assert!(
            matches!(
                run(&[init(), error], Some(Stop::Timeout(hours))).ended,
                Ended::Timeout(_)
            ),
            "the stop explains a result the stop itself caused"
        );
    }

    fn init_with(plugin_errors: Value, mcp_servers: Value) -> String {
        let mut init: Value = serde_json::from_str(&init()).unwrap();
        init["mcp_servers"] = mcp_servers;
        if !plugin_errors.is_null() {
            init["plugin_errors"] = plugin_errors;
        }
        init.to_string()
    }

    #[test]
    fn the_environment_guard_reads_the_first_init() {
        let required = vec!["linear".to_string(), "github".to_string()];
        let healthy = init_with(
            Value::Null,
            json!([{"name": "linear", "status": "connected"},
                   {"name": "github", "status": "connected"},
                   {"name": "other", "status": "failed"}]),
        );
        let mut stream = Stream::new(&limits_with(required.clone()));
        assert_eq!(
            stream.observe(&healthy).stop,
            None,
            "only required servers matter"
        );

        let broken = init_with(
            json!([{"plugin": "dispatch@inline", "type": "manifest", "message": "bad manifest"}]),
            json!([{"name": "linear", "status": "needs-auth"}]),
        );
        let mut stream = Stream::new(&limits_with(required.clone()));
        let Some(Stop::Environment(detail)) = stream.observe(&broken).stop else {
            panic!("the environment is not what the runner needs");
        };
        assert_eq!(
            detail,
            "claude started without its environment: plugin dispatch@inline did not load: \
             bad manifest; MCP server linear is needs-auth; MCP server github is missing"
        );
        assert_eq!(stream.observe(&broken).stop, None, "checked once");
        assert_eq!(
            stream
                .finish(Some(Stop::Environment(detail.clone())), "")
                .ended,
            Ended::Environment(detail)
        );

        let mut unguarded = Stream::new(&limits_with(Vec::new()));
        let bare = init_with(Value::Null, json!([]));
        assert_eq!(
            unguarded.observe(&bare).stop,
            None,
            "nothing required, no plugin errors"
        );
    }
}
