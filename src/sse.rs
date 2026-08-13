//! SSE (Server-Sent Events) parser and stream accumulator for the
//! Anthropic Messages API streaming format.
//!
//! Two main components:
//! - [`SseParser`]: incremental, frame-based SSE parser that converts raw
//!   byte chunks into typed [`StreamEvent`]s.
//! - [`StreamAccumulator`]: consumes `StreamEvent`s and assembles the
//!   complete assistant message with token usage tracking.

use std::collections::HashMap;

use serde::Deserialize;

// ── SSE Frame Parser ──────────────────────────────────────────────

/// Incremental SSE parser. Feed byte chunks via [`push`] and receive
/// parsed [`StreamEvent`]s. Handles frame boundaries (`\n\n` or
/// `\r\n\r\n`), multi-line `data:` payloads, comment lines, `ping`
/// events, and the `[DONE]` sentinel.
#[derive(Debug, Default)]
pub struct SseParser {
    buffer: Vec<u8>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a chunk of bytes into the parser. Returns all complete
    /// events parsed from the accumulated buffer so far.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<StreamEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();

        while let Some(frame) = self.next_frame() {
            if let Some(event) = parse_frame(&frame) {
                events.push(event);
            }
        }

        events
    }

    /// Flush any trailing data in the buffer as a final event.
    pub fn finish(&mut self) -> Vec<StreamEvent> {
        if self.buffer.is_empty() {
            return Vec::new();
        }
        let trailing = std::mem::take(&mut self.buffer);
        let text = String::from_utf8_lossy(&trailing);
        match parse_frame(&text) {
            Some(event) => vec![event],
            None => Vec::new(),
        }
    }

    /// Extract the next complete SSE frame (terminated by `\n\n` or
    /// `\r\n\r\n`) from the buffer, consuming it.
    fn next_frame(&mut self) -> Option<String> {
        // Search for both separators and take whichever appears first.
        // We must check \r\n\r\n before \n\n because \r\n\r\n contains
        // a \n\n subsequence — if we only looked for \n\n we'd split
        // mid-separator on \r\n line endings.
        let crlf = self
            .buffer
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|pos| (pos, 4));
        let lf = self
            .buffer
            .windows(2)
            .position(|w| w == b"\n\n")
            .map(|pos| (pos, 2));

        let separator = match (crlf, lf) {
            (Some(c), Some(l)) => {
                // Take the earlier match. If \r\n\r\n starts at pos P,
                // then \n\n at P+1 is a false match inside it — skip it.
                if l.0 < c.0 {
                    Some(l)
                } else {
                    Some(c)
                }
            }
            (Some(c), None) => Some(c),
            (None, Some(l)) => Some(l),
            (None, None) => None,
        }?;

        let (position, sep_len) = separator;
        let frame_bytes: Vec<u8> = self.buffer.drain(..position + sep_len).collect();
        let frame_len = frame_bytes.len().saturating_sub(sep_len);
        Some(String::from_utf8_lossy(&frame_bytes[..frame_len]).into_owned())
    }
}

/// Parse a single SSE frame into a [`StreamEvent`]. Returns `None` for
/// empty frames, comments, `ping` events, and the `[DONE]` sentinel.
pub fn parse_frame(frame: &str) -> Option<StreamEvent> {
    let trimmed = frame.trim();
    if trimmed.is_empty() {
        return None;
    }

    let mut data_lines: Vec<&str> = Vec::new();
    let mut event_name: Option<&str> = None;

    for line in trimmed.lines() {
        // Comment lines start with ':'
        if line.starts_with(':') {
            continue;
        }
        if let Some(name) = line.strip_prefix("event:") {
            event_name = Some(name.trim());
            continue;
        }
        if let Some(data) = line.strip_prefix("data:") {
            // SSE spec: strip exactly one leading space, not all whitespace
            data_lines.push(data.strip_prefix(' ').unwrap_or(data));
        }
    }

    // Ping events are noise
    if matches!(event_name, Some("ping")) {
        return None;
    }

    if data_lines.is_empty() {
        return None;
    }

    // Multi-line data: join with newline (matches SSE spec)
    let payload = data_lines.join("\n");

    // [DONE] sentinel marks end of stream
    if payload == "[DONE]" {
        return None;
    }

    match serde_json::from_str::<StreamEvent>(&payload) {
        Ok(event) => Some(event),
        Err(e) => {
            tracing::warn!("Failed to parse SSE event: {e} — payload: {payload}");
            None
        }
    }
}

// ── Stream Event Types ────────────────────────────────────────────

/// A typed SSE event from the Anthropic Messages API streaming format.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    MessageStart {
        message: MessageStartData,
    },
    ContentBlockStart {
        index: u32,
        content_block: ContentBlock,
    },
    ContentBlockDelta {
        index: u32,
        delta: ContentDelta,
    },
    ContentBlockStop {
        index: u32,
    },
    MessageDelta {
        delta: MessageDeltaData,
        #[serde(default)]
        usage: Option<UsageData>,
    },
    MessageStop {},
    Ping {},
    Error {
        error: serde_json::Value,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessageStartData {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub usage: Option<UsageData>,
    #[serde(default)]
    pub content: Option<Vec<ContentBlock>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessageDeltaData {
    #[serde(default)]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub stop_sequence: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        #[serde(default)]
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: serde_json::Value,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
        #[serde(default)]
        signature: Option<String>,
    },
    RedactedThinking {
        #[serde(default)]
        data: serde_json::Value,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentDelta {
    TextDelta { text: String },
    InputJsonDelta { partial_json: String },
    ThinkingDelta { thinking: String },
    SignatureDelta { signature: String },
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UsageData {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

// ── Stream Accumulator ────────────────────────────────────────────

/// State machine that consumes [`StreamEvent`]s and assembles the
/// complete assistant response content and token usage. Handles:
/// - Text delta accumulation
/// - Tool use input JSON fragment reassembly
/// - Thinking block accumulation
/// - Token usage tracking from message_start and message_delta
#[derive(Debug, Default)]
pub struct StreamAccumulator {
    /// Content blocks indexed by their stream index.
    /// Each entry is a JSON value representing the finished block.
    blocks: HashMap<u32, AccBlock>,
    /// Whether message_stop has been received
    complete: bool,
    /// Token usage from message_start
    pub input_tokens: u64,
    /// Token usage from message_delta (final output count)
    pub output_tokens: u64,
    /// Stop reason
    pub stop_reason: Option<String>,
}

/// Internal per-block accumulation state.
#[derive(Debug)]
enum AccBlock {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        json_fragments: String,
    },
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
    RedactedThinking(serde_json::Value),
}

impl StreamAccumulator {
    pub fn new() -> Self {
        Self {
            blocks: HashMap::new(),
            complete: false,
            input_tokens: 0,
            output_tokens: 0,
            stop_reason: None,
        }
    }

    /// Process a single stream event.
    pub fn process_event(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::MessageStart { message } => {
                if let Some(usage) = &message.usage {
                    self.input_tokens = usage.input_tokens;
                    self.output_tokens = usage.output_tokens;
                }
                // message_start may include initial content blocks
                if let Some(blocks) = &message.content {
                    for (i, block) in blocks.iter().enumerate() {
                        self.init_block(i as u32, block);
                    }
                }
            }

            StreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                self.init_block(*index, content_block);
            }

            StreamEvent::ContentBlockDelta { index, delta } => {
                self.apply_delta(*index, delta);
            }

            StreamEvent::ContentBlockStop { .. } => {
                // Block finalization is handled when building the output
            }

            StreamEvent::MessageDelta { delta, usage } => {
                if let Some(usage) = usage {
                    // message_delta's output_tokens is the final count
                    self.output_tokens = usage.output_tokens;
                }
                if delta.stop_reason.is_some() {
                    self.stop_reason.clone_from(&delta.stop_reason);
                }
            }

            StreamEvent::MessageStop {} => {
                self.complete = true;
            }

            StreamEvent::Ping {} | StreamEvent::Error { .. } => {}
        }
    }

    /// Is the stream complete?
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// Build the final assistant message content blocks as a JSON array.
    /// Returns `None` if no content was accumulated.
    pub fn into_content_blocks(self) -> Option<Vec<serde_json::Value>> {
        if self.blocks.is_empty() {
            return None;
        }

        // Sort by index to preserve order
        let mut indices: Vec<u32> = self.blocks.keys().copied().collect();
        indices.sort_unstable();

        let mut result = Vec::with_capacity(indices.len());
        for idx in indices {
            if let Some(block) = self.blocks.get(&idx) {
                if let Some(json) = block.to_json() {
                    result.push(json);
                }
            }
        }

        if result.is_empty() {
            None
        } else {
            Some(result)
        }
    }

    fn init_block(&mut self, index: u32, block: &ContentBlock) {
        let acc = match block {
            ContentBlock::Text { text } => AccBlock::Text(text.clone()),
            ContentBlock::ToolUse { id, name, .. } => AccBlock::ToolUse {
                id: id.clone(),
                name: name.clone(),
                json_fragments: String::new(),
            },
            ContentBlock::Thinking {
                thinking,
                signature,
            } => AccBlock::Thinking {
                thinking: thinking.clone(),
                signature: signature.clone(),
            },
            ContentBlock::RedactedThinking { data } => AccBlock::RedactedThinking(data.clone()),
        };
        self.blocks.insert(index, acc);
    }

    fn apply_delta(&mut self, index: u32, delta: &ContentDelta) {
        match delta {
            ContentDelta::TextDelta { text } => {
                match self.blocks.get_mut(&index) {
                    Some(AccBlock::Text(buf)) => buf.push_str(text),
                    // If block wasn't started, create it
                    None => {
                        self.blocks.insert(index, AccBlock::Text(text.clone()));
                    }
                    _ => {}
                }
            }
            ContentDelta::InputJsonDelta { partial_json } => match self.blocks.get_mut(&index) {
                Some(AccBlock::ToolUse { json_fragments, .. }) => {
                    json_fragments.push_str(partial_json);
                }
                _ => {
                    tracing::warn!(
                        "input_json_delta for index {index} without matching tool_use block"
                    );
                }
            },
            ContentDelta::ThinkingDelta { thinking } => match self.blocks.get_mut(&index) {
                Some(AccBlock::Thinking { thinking: buf, .. }) => {
                    buf.push_str(thinking);
                }
                None => {
                    self.blocks.insert(
                        index,
                        AccBlock::Thinking {
                            thinking: thinking.clone(),
                            signature: None,
                        },
                    );
                }
                _ => {}
            },
            ContentDelta::SignatureDelta { signature } => {
                if let Some(AccBlock::Thinking { signature: sig, .. }) = self.blocks.get_mut(&index)
                {
                    match sig {
                        Some(s) => s.push_str(signature),
                        None => *sig = Some(signature.clone()),
                    }
                }
            }
        }
    }
}

impl AccBlock {
    /// Convert accumulated block state to a JSON value suitable for
    /// the Messages API conversation history.
    fn to_json(&self) -> Option<serde_json::Value> {
        match self {
            AccBlock::Text(text) => {
                if text.is_empty() {
                    return None;
                }
                Some(serde_json::json!({
                    "type": "text",
                    "text": text,
                }))
            }
            AccBlock::ToolUse {
                id,
                name,
                json_fragments,
            } => {
                // Parse accumulated JSON fragments into a Value.
                // If parsing fails, store as raw string to avoid data loss.
                let input = if json_fragments.is_empty() {
                    serde_json::json!({})
                } else {
                    serde_json::from_str(json_fragments).unwrap_or_else(|e| {
                        tracing::warn!("Failed to parse tool_use input JSON for {name}: {e}");
                        serde_json::json!({"_raw": json_fragments})
                    })
                };
                Some(serde_json::json!({
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": input,
                }))
            }
            AccBlock::Thinking {
                thinking,
                signature,
            } => Some(serde_json::json!({
                "type": "thinking",
                "thinking": thinking,
                "signature": signature,
            })),
            AccBlock::RedactedThinking(data) => Some(serde_json::json!({
                "type": "redacted_thinking",
                "data": data,
            })),
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── SseParser tests ───────────────────────────────────────────

    #[test]
    fn parse_single_text_delta() {
        let frame = concat!(
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,",
            "\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
        );
        let event = parse_frame(frame);
        assert!(event.is_some());
        match event.unwrap() {
            StreamEvent::ContentBlockDelta { index, delta } => {
                assert_eq!(index, 0);
                match delta {
                    ContentDelta::TextDelta { text } => assert_eq!(text, "Hello"),
                    _ => panic!("expected TextDelta"),
                }
            }
            _ => panic!("expected ContentBlockDelta"),
        }
    }

    #[test]
    fn chunked_streaming() {
        let mut parser = SseParser::new();

        // First chunk: incomplete frame
        let chunk1 = b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel";
        assert!(parser.push(chunk1).is_empty());

        // Second chunk: completes the frame
        let chunk2 = b"lo\"}}\n\n";
        let events = parser.push(chunk2);
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamEvent::ContentBlockDelta { delta, .. } => match delta {
                ContentDelta::TextDelta { text } => assert_eq!(text, "Hello"),
                _ => panic!("expected TextDelta"),
            },
            _ => panic!("expected ContentBlockDelta"),
        }
    }

    #[test]
    fn filters_ping_and_done() {
        let mut parser = SseParser::new();
        let payload = concat!(
            ": keepalive comment\n\n",
            "event: ping\n",
            "data: {\"type\":\"ping\"}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
            "data: [DONE]\n\n",
        );
        let events = parser.push(payload.as_bytes());
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], StreamEvent::MessageStop {}));
    }

    #[test]
    fn multi_line_data() {
        let frame = concat!(
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\n",
            "data: \"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
        );
        let event = parse_frame(frame);
        assert!(event.is_some());
        match event.unwrap() {
            StreamEvent::ContentBlockDelta { delta, .. } => match delta {
                ContentDelta::TextDelta { text } => assert_eq!(text, "Hi"),
                _ => panic!("expected TextDelta"),
            },
            _ => panic!("expected ContentBlockDelta"),
        }
    }

    #[test]
    fn crlf_separator() {
        let mut parser = SseParser::new();
        let payload = b"event: message_stop\r\ndata: {\"type\":\"message_stop\"}\r\n\r\n";
        let events = parser.push(payload);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], StreamEvent::MessageStop {}));
    }

    #[test]
    fn multiple_events_in_one_push() {
        let mut parser = SseParser::new();
        let payload = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"A\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        );
        let events = parser.push(payload.as_bytes());
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], StreamEvent::ContentBlockStart { .. }));
        assert!(matches!(events[1], StreamEvent::ContentBlockDelta { .. }));
        assert!(matches!(events[2], StreamEvent::ContentBlockStop { .. }));
    }

    // ── StreamAccumulator tests ───────────────────────────────────

    #[test]
    fn accumulates_text_deltas() {
        let mut acc = StreamAccumulator::new();

        acc.process_event(&StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlock::Text {
                text: String::new(),
            },
        });
        acc.process_event(&StreamEvent::ContentBlockDelta {
            index: 0,
            delta: ContentDelta::TextDelta {
                text: "Hello ".to_string(),
            },
        });
        acc.process_event(&StreamEvent::ContentBlockDelta {
            index: 0,
            delta: ContentDelta::TextDelta {
                text: "world".to_string(),
            },
        });
        acc.process_event(&StreamEvent::ContentBlockStop { index: 0 });
        acc.process_event(&StreamEvent::MessageStop {});

        assert!(acc.is_complete());
        let blocks = acc.into_content_blocks().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "Hello world");
    }

    #[test]
    fn accumulates_tool_use_json_fragments() {
        let mut acc = StreamAccumulator::new();

        acc.process_event(&StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlock::ToolUse {
                id: "tu_1".to_string(),
                name: "bash".to_string(),
                input: serde_json::json!({}),
            },
        });
        // JSON fragments come as partial strings
        acc.process_event(&StreamEvent::ContentBlockDelta {
            index: 0,
            delta: ContentDelta::InputJsonDelta {
                partial_json: "{\"com".to_string(),
            },
        });
        acc.process_event(&StreamEvent::ContentBlockDelta {
            index: 0,
            delta: ContentDelta::InputJsonDelta {
                partial_json: "mand\":".to_string(),
            },
        });
        acc.process_event(&StreamEvent::ContentBlockDelta {
            index: 0,
            delta: ContentDelta::InputJsonDelta {
                partial_json: "\"ls -la\"}".to_string(),
            },
        });
        acc.process_event(&StreamEvent::ContentBlockStop { index: 0 });
        acc.process_event(&StreamEvent::MessageStop {});

        let blocks = acc.into_content_blocks().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "tool_use");
        assert_eq!(blocks[0]["id"], "tu_1");
        assert_eq!(blocks[0]["name"], "bash");
        assert_eq!(blocks[0]["input"]["command"], "ls -la");
    }

    #[test]
    fn mixed_text_and_tool_use() {
        let mut acc = StreamAccumulator::new();

        // Text at index 0
        acc.process_event(&StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlock::Text {
                text: String::new(),
            },
        });
        acc.process_event(&StreamEvent::ContentBlockDelta {
            index: 0,
            delta: ContentDelta::TextDelta {
                text: "Let me check.".to_string(),
            },
        });
        acc.process_event(&StreamEvent::ContentBlockStop { index: 0 });

        // Tool use at index 1
        acc.process_event(&StreamEvent::ContentBlockStart {
            index: 1,
            content_block: ContentBlock::ToolUse {
                id: "tu_2".to_string(),
                name: "read_file".to_string(),
                input: serde_json::json!({}),
            },
        });
        acc.process_event(&StreamEvent::ContentBlockDelta {
            index: 1,
            delta: ContentDelta::InputJsonDelta {
                partial_json: "{\"path\":\"/tmp/test.txt\"}".to_string(),
            },
        });
        acc.process_event(&StreamEvent::ContentBlockStop { index: 1 });

        acc.process_event(&StreamEvent::MessageStop {});

        let blocks = acc.into_content_blocks().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "Let me check.");
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["input"]["path"], "/tmp/test.txt");
    }

    #[test]
    fn tracks_usage_from_message_events() {
        let mut acc = StreamAccumulator::new();

        acc.process_event(&StreamEvent::MessageStart {
            message: MessageStartData {
                id: Some("msg_1".to_string()),
                model: Some("claude-sonnet-4-6".to_string()),
                usage: Some(UsageData {
                    input_tokens: 1500,
                    output_tokens: 0,
                    ..Default::default()
                }),
                content: None,
            },
        });

        // message_delta carries the final output_tokens count
        acc.process_event(&StreamEvent::MessageDelta {
            delta: MessageDeltaData {
                stop_reason: Some("end_turn".to_string()),
                stop_sequence: None,
            },
            usage: Some(UsageData {
                input_tokens: 0,
                output_tokens: 250,
                ..Default::default()
            }),
        });

        acc.process_event(&StreamEvent::MessageStop {});

        assert_eq!(acc.input_tokens, 1500);
        assert_eq!(acc.output_tokens, 250);
        assert_eq!(acc.stop_reason.as_deref(), Some("end_turn"));
        assert!(acc.is_complete());
    }

    #[test]
    fn thinking_block_accumulation() {
        let mut acc = StreamAccumulator::new();

        acc.process_event(&StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlock::Thinking {
                thinking: String::new(),
                signature: None,
            },
        });
        acc.process_event(&StreamEvent::ContentBlockDelta {
            index: 0,
            delta: ContentDelta::ThinkingDelta {
                thinking: "step 1, ".to_string(),
            },
        });
        acc.process_event(&StreamEvent::ContentBlockDelta {
            index: 0,
            delta: ContentDelta::ThinkingDelta {
                thinking: "step 2".to_string(),
            },
        });
        acc.process_event(&StreamEvent::ContentBlockDelta {
            index: 0,
            delta: ContentDelta::SignatureDelta {
                signature: "sig_abc".to_string(),
            },
        });
        acc.process_event(&StreamEvent::ContentBlockStop { index: 0 });
        acc.process_event(&StreamEvent::MessageStop {});

        let blocks = acc.into_content_blocks().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["thinking"], "step 1, step 2");
        assert_eq!(blocks[0]["signature"], "sig_abc");
    }

    #[test]
    fn empty_accumulator_returns_none() {
        let acc = StreamAccumulator::new();
        assert!(acc.into_content_blocks().is_none());
    }

    #[test]
    fn finish_flushes_trailing_frame() {
        let mut parser = SseParser::new();
        // Push a frame without the trailing \n\n
        parser.push(b"event: message_stop\ndata: {\"type\":\"message_stop\"}");
        // finish() should still parse it
        let events = parser.finish();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], StreamEvent::MessageStop {}));
    }
}
