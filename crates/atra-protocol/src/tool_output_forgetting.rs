use std::{
    collections::{BTreeMap, HashMap},
    sync::LazyLock,
};

use regex::Regex;

use crate::{EventSequence, ModelRequestKind, ThreadEventData, ToolResultEvent};

static DIRECTIVE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?s)<forget_output\s+call_id\s*=\s*(?:"(?P<double>[^"]+)"|'(?P<single>[^']+)')\s*>(?P<summary>.*?)</forget_output>"#,
    )
    .expect("forget_output regex is valid")
});

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolOutputForgetting {
    forgotten: BTreeMap<EventSequence, String>,
    projected_messages: BTreeMap<EventSequence, String>,
    request_batches: BTreeMap<EventSequence, Vec<EventSequence>>,
    current_batch: Vec<EventSequence>,
}

impl ToolOutputForgetting {
    pub fn summary(&self, result: EventSequence) -> Option<&str> {
        self.forgotten.get(&result).map(String::as_str)
    }

    pub fn projected_message(&self, message: EventSequence) -> Option<&str> {
        self.projected_messages.get(&message).map(String::as_str)
    }

    pub fn request_batch(&self, request: EventSequence) -> &[EventSequence] {
        self.request_batches
            .get(&request)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub fn current_batch(&self) -> &[EventSequence] {
        &self.current_batch
    }
}

pub fn project_tool_output_forgetting<'a>(
    events: impl IntoIterator<Item = (EventSequence, &'a ThreadEventData)>,
) -> ToolOutputForgetting {
    let events = events.into_iter().collect::<Vec<_>>();
    let mut projection = ToolOutputForgetting::default();
    let mut pending_results = Vec::new();
    let mut active_eligible = HashMap::<String, Option<EventSequence>>::new();

    for (sequence, data) in &events {
        match data {
            ThreadEventData::ModelRequest(request)
                if request.kind == ModelRequestKind::Response =>
            {
                projection
                    .request_batches
                    .insert(*sequence, pending_results.clone());
                active_eligible = eligible_by_call_id(&pending_results, &events);
                pending_results.clear();
            }
            ThreadEventData::ToolResult(_) => pending_results.push(*sequence),
            ThreadEventData::AssistantMessage(message) => {
                let mut projected = String::with_capacity(message.content.len());
                let mut through = 0;
                let mut changed = false;
                for captures in DIRECTIVE.captures_iter(&message.content) {
                    let matched = captures.get(0).expect("regex match exists");
                    let summary = captures
                        .name("summary")
                        .expect("summary capture exists")
                        .as_str()
                        .trim();
                    let call_id = captures
                        .name("double")
                        .or_else(|| captures.name("single"))
                        .expect("call_id capture exists")
                        .as_str();
                    let target = active_eligible.get(call_id).copied().flatten();
                    let valid = !summary.is_empty()
                        && !summary.contains("<forget_output")
                        && target.is_some();
                    if !valid {
                        continue;
                    }
                    projected.push_str(&message.content[through..matched.start()]);
                    through = matched.end();
                    changed = true;
                    projection
                        .forgotten
                        .insert(target.expect("valid target exists"), summary.to_owned());
                }
                if changed {
                    projected.push_str(&message.content[through..]);
                    projection.projected_messages.insert(*sequence, projected);
                }
            }
            _ => {}
        }
    }
    projection.current_batch = pending_results;
    projection
}

fn eligible_by_call_id(
    sequences: &[EventSequence],
    events: &[(EventSequence, &ThreadEventData)],
) -> HashMap<String, Option<EventSequence>> {
    let mut eligible = HashMap::new();
    for sequence in sequences {
        let Some(call_id) = events.iter().find_map(|(candidate, data)| {
            (*candidate == *sequence).then(|| match data {
                ThreadEventData::ToolResult(
                    ToolResultEvent::Custom { call_id, .. }
                    | ToolResultEvent::Function { call_id, .. },
                ) => Some(call_id),
                _ => None,
            })?
        }) else {
            continue;
        };
        eligible
            .entry(call_id.clone())
            .and_modify(|value| *value = None)
            .or_insert(Some(*sequence));
    }
    eligible
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{AssistantMessageEvent, AssistantMessagePhase, ModelRequestEvent, ThreadEvent};

    fn request(sequence: i64) -> ThreadEvent {
        ThreadEvent {
            sequence: EventSequence(sequence),
            data: ThreadEventData::ModelRequest(ModelRequestEvent {
                kind: ModelRequestKind::Response,
                context_window: None,
            }),
        }
    }

    fn result(sequence: i64, call_id: &str) -> ThreadEvent {
        ThreadEvent {
            sequence: EventSequence(sequence),
            data: ThreadEventData::ToolResult(ToolResultEvent::Function {
                name: "tool".to_owned(),
                call_id: call_id.to_owned(),
                result: json!({"output": "large"}),
                artifacts: Vec::new(),
            }),
        }
    }

    fn message(sequence: i64, content: &str) -> ThreadEvent {
        ThreadEvent {
            sequence: EventSequence(sequence),
            data: ThreadEventData::AssistantMessage(AssistantMessageEvent {
                content: content.to_owned(),
                phase: AssistantMessagePhase::Commentary,
                todos: Vec::new(),
            }),
        }
    }

    fn project(events: &[ThreadEvent]) -> ToolOutputForgetting {
        project_tool_output_forgetting(events.iter().map(|event| (event.sequence, &event.data)))
    }

    #[test]
    fn forgets_only_results_eligible_for_the_response() {
        let events = vec![
            request(1),
            result(2, "old"),
            request(3),
            message(
                4,
                "before <forget_output call_id='old'> needed fact </forget_output> after",
            ),
            result(5, "new"),
        ];

        let projection = project(&events);

        assert_eq!(
            projection.request_batch(EventSequence(3)),
            &[EventSequence(2)]
        );
        assert_eq!(projection.current_batch(), &[EventSequence(5)]);
        assert_eq!(projection.summary(EventSequence(2)), Some("needed fact"));
        assert_eq!(
            projection.projected_message(EventSequence(4)),
            Some("before  after")
        );
    }

    #[test]
    fn keeps_malformed_empty_nested_and_ineligible_directives_as_prose() {
        let content = concat!(
            "<forget_output call_id=\"eligible\"></forget_output>",
            "<forget_output call_id=\"missing\">summary</forget_output>",
            "<forget_output call_id=\"eligible\">outer ",
            "<forget_output call_id=\"eligible\">inner</forget_output>",
            "</forget_output>",
        );
        let events = vec![result(1, "eligible"), request(2), message(3, content)];

        let projection = project(&events);

        assert_eq!(projection.summary(EventSequence(1)), None);
        assert_eq!(projection.projected_message(EventSequence(3)), None);
    }

    #[test]
    fn accepts_whitespace_quotes_multiline_and_uses_last_summary() {
        let events = vec![
            result(1, "call"),
            request(2),
            message(
                3,
                concat!(
                    "<forget_output call_id = \"call\">\nfirst\n</forget_output>",
                    "middle",
                    "<forget_output   call_id='call'>\nsecond\n</forget_output>",
                ),
            ),
        ];

        let projection = project(&events);

        assert_eq!(projection.summary(EventSequence(1)), Some("second"));
        assert_eq!(
            projection.projected_message(EventSequence(3)),
            Some("middle")
        );
    }

    #[test]
    fn repeated_call_ids_in_different_requests_resolve_to_their_event_occurrence() {
        let events = vec![
            result(1, "same"),
            request(2),
            message(3, "<forget_output call_id=\"same\">first</forget_output>"),
            result(4, "same"),
            request(5),
            message(6, "<forget_output call_id=\"same\">second</forget_output>"),
        ];

        let projection = project(&events);

        assert_eq!(projection.summary(EventSequence(1)), Some("first"));
        assert_eq!(projection.summary(EventSequence(4)), Some("second"));
    }

    #[test]
    fn ambiguous_call_ids_in_one_batch_are_not_forgotten() {
        let events = vec![
            result(1, "same"),
            result(2, "same"),
            request(3),
            message(4, "<forget_output call_id=\"same\">summary</forget_output>"),
        ];

        let projection = project(&events);

        assert_eq!(projection.summary(EventSequence(1)), None);
        assert_eq!(projection.summary(EventSequence(2)), None);
        assert_eq!(projection.projected_message(EventSequence(4)), None);
    }

    #[test]
    fn a_later_request_consumes_the_previous_eligibility() {
        let events = vec![
            result(1, "call"),
            request(2),
            request(3),
            message(
                4,
                "<forget_output call_id=\"call\">too late</forget_output>",
            ),
        ];

        let projection = project(&events);

        assert_eq!(projection.summary(EventSequence(1)), None);
        assert_eq!(projection.projected_message(EventSequence(4)), None);
    }
}
