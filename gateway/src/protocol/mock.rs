//! The compiled-in mock: no connection, a reply that is a pure function of the call, so free tests can
//! drive both a text turn and a tool turn, unary or streamed.
//!
//! A call that declares tools and does not end in a tool result gets a tool turn: one call to the first
//! declared tool (`mock-call-1`, args `{}`), stop `tool_use`, no text. Anything else gets the fixed text
//! `mock provider response`. Usage is always 20 in, 30 out. The stream sends the same text as
//! word-sized deltas, then a stop carrying the rest of the reply.

use super::{Call, Event, Reply, Stop};
use crate::wire::ReplyToolCall;
use serde_json::value::RawValue;
use std::collections::VecDeque;

/// The fixed text of a text turn.
pub const TEXT: &str = "mock provider response";

/// The unary reply for `call`, served by endpoint `provider`.
pub fn reply(provider: &str, call: &Call) -> Reply {
    let tool_turn = !call.tools.is_empty() && call.messages.last().is_none_or(|m| m.role != "tool");
    let mut r = Reply {
        input_tokens: 20,
        output_tokens: 30,
        provider: provider.to_string(),
        ..Reply::default()
    };
    match call.tools.first().filter(|_| tool_turn) {
        Some(tool) => {
            r.stop_reason = "tool_use".into();
            if let Ok(args) = RawValue::from_string("{}".into()) {
                r.tool_calls.push(ReplyToolCall {
                    id: "mock-call-1".into(),
                    name: tool.name.clone(),
                    invalid_arguments: false,
                    args,
                });
            }
        }
        None => TEXT.clone_into(&mut r.text),
    }
    r
}

/// The stream for `call`: the reply's text as word-sized deltas, then its stop.
pub struct Stream {
    events: VecDeque<Event>,
}

impl Stream {
    /// Mock streams carry no partial usage before stop.
    #[allow(clippy::unused_self)]
    pub fn metered_usage(&self) -> super::MeteredUsage {
        super::MeteredUsage::default()
    }

    /// Open the stream (nothing can fail: there is no connection).
    pub fn open(provider: &str, call: &Call) -> Self {
        let r = reply(provider, call);
        let mut events: VecDeque<Event> = r
            .text
            .split_inclusive(' ')
            .map(|w| Event::Delta(w.to_string()))
            .collect();
        events.push_back(Event::Stop(Stop {
            reason: if r.stop_reason.is_empty() {
                "end_turn".into()
            } else {
                r.stop_reason
            },
            input_tokens: r.input_tokens,
            output_tokens: r.output_tokens,
            cache_read_tokens: r.cache_read_tokens,
            cache_write_tokens: r.cache_write_tokens,
            charged: None,
            provider: r.provider,
            tool_calls: r.tool_calls,
        }));
        Self { events }
    }

    /// The next event; `None` after the stop.
    pub fn next(&mut self) -> Option<Event> {
        self.events.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Tool;
    use crate::wire::Message;

    fn call(tools: &[&str], last_role: &str) -> Call {
        Call {
            model: "mock-echo".into(),
            system: String::new(),
            messages: vec![Message {
                role: last_role.into(),
                content: serde_json::json!("hi"),
                ..Message::default()
            }],
            tools: tools
                .iter()
                .map(|n| Tool {
                    name: (*n).into(),
                    description: String::new(),
                    schema: None,
                })
                .collect(),
            tool_choice: None,
            max_tokens: 0,
            request_id: String::new(),
        }
    }

    fn drain(mut s: Stream) -> (String, Option<Stop>) {
        let mut text = String::new();
        while let Some(ev) = s.next() {
            match ev {
                Event::Delta(d) => text.push_str(&d),
                Event::Stop(stop) => return (text, Some(stop)),
                Event::UpstreamError(_) => break,
            }
        }
        (text, None)
    }

    #[test]
    fn text_turn_streams_the_unary_reply() {
        let c = call(&[], "user");
        let r = reply("mock", &c);
        assert_eq!((r.text.as_str(), r.stop_reason.as_str()), (TEXT, ""));
        let (text, stop) = drain(Stream::open("mock", &c));
        let stop = stop.unwrap_or_default();
        assert_eq!(text, r.text);
        assert_eq!(
            (
                stop.reason.as_str(),
                stop.input_tokens,
                stop.output_tokens,
                stop.provider.as_str()
            ),
            ("end_turn", 20, 30, "mock")
        );
        assert!(stop.tool_calls.is_empty());
    }

    #[test]
    fn tool_turn_calls_the_first_tool_until_a_result_arrives() {
        let c = call(&["fs.read", "other"], "user");
        let r = reply("mock", &c);
        assert_eq!((r.text.as_str(), r.stop_reason.as_str()), ("", "tool_use"));
        let (text, stop) = drain(Stream::open("mock", &c));
        let stop = stop.unwrap_or_default();
        assert_eq!(text, "");
        let got: Vec<_> = stop
            .tool_calls
            .iter()
            .map(|t| (t.id.as_str(), t.name.as_str(), t.args.get()))
            .collect();
        assert_eq!(got, vec![("mock-call-1", "fs.read", "{}")]);
        assert_eq!(stop.reason, "tool_use");
        assert_eq!(reply("mock", &call(&["fs.read"], "tool")).text, TEXT);
    }
}
