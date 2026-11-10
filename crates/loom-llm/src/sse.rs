use bytes::Bytes;
use futures::{Stream, StreamExt};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
    pending_cr: bool,
    event: Option<String>,
    data: Option<String>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        let mut events = Vec::new();
        for &byte in chunk {
            if self.pending_cr {
                self.pending_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\n' => self.end_line(&mut events),
                b'\r' => {
                    self.pending_cr = true;
                    self.end_line(&mut events);
                }
                _ => self.buffer.push(byte),
            }
        }
        events
    }

    pub fn finish(&mut self) -> Option<SseEvent> {
        let mut events = Vec::new();
        if !self.buffer.is_empty() {
            self.end_line(&mut events);
        }
        self.dispatch(&mut events);
        events.pop()
    }

    fn end_line(&mut self, events: &mut Vec<SseEvent>) {
        let line = String::from_utf8_lossy(&self.buffer).into_owned();
        self.buffer.clear();
        if line.is_empty() {
            self.dispatch(events);
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line.as_str(), ""),
        };
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => match &mut self.data {
                Some(data) => {
                    data.push('\n');
                    data.push_str(value);
                }
                None => self.data = Some(value.to_owned()),
            },
            _ => {}
        }
    }

    fn dispatch(&mut self, events: &mut Vec<SseEvent>) {
        let event = self.event.take();
        if let Some(data) = self.data.take() {
            events.push(SseEvent { event, data });
        }
    }
}

pub fn decode_sse<S, E>(body: S) -> impl Stream<Item = Result<SseEvent, E>>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    futures::stream::unfold(
        (
            body,
            SseDecoder::new(),
            std::collections::VecDeque::new(),
            false,
        ),
        |(mut body, mut decoder, mut queue, mut done)| async move {
            loop {
                if let Some(event) = queue.pop_front() {
                    return Some((Ok(event), (body, decoder, queue, done)));
                }
                if done {
                    return None;
                }
                match body.next().await {
                    Some(Ok(chunk)) => queue.extend(decoder.push(&chunk)),
                    Some(Err(err)) => {
                        done = true;
                        return Some((Err(err), (body, decoder, queue, done)));
                    }
                    None => {
                        done = true;
                        queue.extend(decoder.finish());
                    }
                }
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_events_split_across_chunks() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push(b"event: a\nda").is_empty());
        let events = decoder.push(b"ta: {\"x\":1}\n\ndata: two\r\n\r\n");
        assert_eq!(
            events,
            vec![
                SseEvent {
                    event: Some("a".into()),
                    data: "{\"x\":1}".into()
                },
                SseEvent {
                    event: None,
                    data: "two".into()
                },
            ]
        );
    }

    #[test]
    fn joins_multiline_data_and_skips_comments() {
        let mut decoder = SseDecoder::new();
        let events = decoder.push(b": keepalive\ndata: a\ndata:b\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "a\nb");
    }

    #[test]
    fn handles_cr_split_from_lf() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push(b"data: x\r").is_empty());
        let events = decoder.push(b"\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "x");
    }

    #[test]
    fn flushes_trailing_event_at_eof() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push(b"data: tail").is_empty());
        assert_eq!(decoder.finish().map(|e| e.data), Some("tail".into()));
    }

    #[test]
    fn keeps_utf8_split_across_chunks() {
        let mut decoder = SseDecoder::new();
        let text = "data: 한글\n\n".as_bytes();
        assert!(decoder.push(&text[..8]).is_empty());
        let events = decoder.push(&text[8..]);
        assert_eq!(events[0].data, "한글");
    }

    #[tokio::test]
    async fn decodes_byte_stream() {
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from_static(b"data: 1\n\nda")),
            Ok(Bytes::from_static(b"ta: 2")),
        ];
        let events: Vec<_> = decode_sse(futures::stream::iter(chunks))
            .map(|e| e.unwrap().data)
            .collect()
            .await;
        assert_eq!(events, vec!["1", "2"]);
    }
}
