//! Reading `claude -p --output-format stream-json --verbose`: one JSON event per line. The
//! stream names the session (`session_id` on every event, first on `system/init`) and ends
//! with a `result` event carrying `subtype`, `is_error`, `structured_output` and
//! `total_cost_usd`. Pure logic, so it is tested with scripted lines.

use serde_json::Value;

use super::{Ended, Notice, SessionReport};

/// Why the runner stopped a session before it ended by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// Kill switch or signal.
    Interrupted,
}

/// What one line changed.
#[derive(Debug, Default, PartialEq)]
pub struct Observed {
    pub notice: Option<Notice>,
    pub stop: Option<Stop>,
}

#[derive(Debug, Default)]
pub struct Stream {
    session_id: Option<String>,
    result: Option<Value>,
}

impl Stream {
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
            observed.notice = Some(Notice::Started {
                session_id: id.to_string(),
            });
        }
        if event["type"] == "result" {
            self.result = Some(event);
        }
        observed
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
        if let Some(Stop::Interrupted) = stop {
            return Ended::Interrupted;
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

    fn run(lines: &[String], stop: Option<Stop>) -> SessionReport {
        let mut stream = Stream::default();
        for line in lines {
            stream.observe(line);
        }
        stream.finish(stop, "exit status: 0")
    }

    #[test]
    fn the_first_session_id_is_announced_once() {
        let mut stream = Stream::default();
        assert_eq!(stream.observe("warning: not json"), Observed::default());
        let first = stream.observe(&init());
        assert_eq!(
            first.notice,
            Some(Notice::Started {
                session_id: "session-a".into()
            })
        );
        assert_eq!(stream.observe(&init()), Observed::default());
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
}
