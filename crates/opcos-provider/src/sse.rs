use bytes::Bytes;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
}

#[derive(Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
    event: String,
    data: Vec<String>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(position) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line = self.buffer.drain(..=position).collect::<Vec<_>>();
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.consume_line(String::from_utf8_lossy(&line).as_ref(), &mut events);
        }
        events
    }

    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if !self.buffer.is_empty() {
            let line = String::from_utf8_lossy(&self.buffer).into_owned();
            self.buffer.clear();
            self.consume_line(&line, &mut events);
        }
        self.emit(&mut events);
        events
    }

    fn consume_line(&mut self, line: &str, events: &mut Vec<SseEvent>) {
        if line.is_empty() {
            self.emit(events);
        } else if line.starts_with(':') {
        } else if let Some(data) = line.strip_prefix("data:") {
            self.data
                .push(data.strip_prefix(' ').unwrap_or(data).to_owned());
        } else if let Some(event) = line.strip_prefix("event:") {
            self.event = event.strip_prefix(' ').unwrap_or(event).to_owned();
        }
    }

    fn emit(&mut self, events: &mut Vec<SseEvent>) {
        if self.data.is_empty() {
            self.event.clear();
            return;
        }
        events.push(SseEvent {
            event: std::mem::take(&mut self.event),
            data: self.data.join("\n"),
        });
        self.data.clear();
    }
}

pub fn parse_json(event: &SseEvent) -> Result<Value, String> {
    serde_json::from_str(&event.data).map_err(|error| error.to_string())
}

pub fn chunk_bytes(chunk: Bytes) -> Vec<u8> {
    chunk.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_events_split_at_every_byte() {
        let source = b"event: message\ndata: {\"text\":\"hello\"}\n\ndata: [DONE]\n\n";
        for split in 0..source.len() {
            let mut decoder = SseDecoder::new();
            let mut events = decoder.push(&source[..split]);
            events.extend(decoder.push(&source[split..]));
            events.extend(decoder.finish());
            assert_eq!(
                events,
                vec![
                    SseEvent {
                        event: "message".into(),
                        data: "{\"text\":\"hello\"}".into()
                    },
                    SseEvent {
                        event: "".into(),
                        data: "[DONE]".into()
                    }
                ],
                "split={split}"
            );
        }
    }

    #[test]
    fn joins_multiline_data_and_ignores_comments() {
        let mut decoder = SseDecoder::new();
        let events = decoder.push(b": keepalive\r\ndata: one\r\ndata: two\r\n\r\n");
        assert_eq!(events[0].data, "one\ntwo");
    }
}
