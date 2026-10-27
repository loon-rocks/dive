use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::task::JoinHandle;

const KEPT_LINES: usize = 8;
const LINE_CHARS: usize = 400;
const EXIT_GRACE: Duration = Duration::from_millis(500);

pub(crate) struct StderrTail {
    lines: Arc<Mutex<VecDeque<String>>>,
    reader: Option<JoinHandle<()>>,
}

impl StderrTail {
    pub(crate) fn watch(stderr: Option<impl AsyncRead + Unpin + Send + 'static>) -> Self {
        let lines = Arc::new(Mutex::new(VecDeque::new()));
        let reader = stderr.map(|stderr| {
            let lines = Arc::clone(&lines);
            tokio::spawn(async move {
                let mut stderr = BufReader::new(stderr);
                let mut buffer = Vec::new();
                loop {
                    buffer.clear();
                    match stderr.read_until(b'\n', &mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => keep(&lines, &buffer),
                    }
                }
            })
        });
        Self { lines, reader }
    }

    pub(crate) async fn explain(mut self, error: String) -> String {
        if let Some(reader) = self.reader.as_mut() {
            let _ = tokio::time::timeout(EXIT_GRACE, reader).await;
        }
        let lines = self
            .lines
            .lock()
            .map(|lines| lines.clone())
            .unwrap_or_default();
        if lines.is_empty() {
            return error;
        }
        let tail: Vec<String> = lines.into_iter().collect();
        format!("{error} (server stderr: {})", tail.join(" | "))
    }
}

fn keep(lines: &Mutex<VecDeque<String>>, raw: &[u8]) {
    let line = String::from_utf8_lossy(raw);
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    let line: String = line.chars().take(LINE_CHARS).collect();
    if let Ok(mut lines) = lines.lock() {
        if lines.len() == KEPT_LINES {
            lines.pop_front();
        }
        lines.push_back(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn explain_appends_the_last_lines_the_server_wrote() {
        let written: Vec<u8> = (1..=10)
            .map(|n| format!("line {n}\n"))
            .chain([
                "\n".to_string(),
                "env: node: No such file or directory\n".into(),
            ])
            .collect::<String>()
            .into_bytes();
        let tail = StderrTail::watch(Some(std::io::Cursor::new(written)));
        let message = tail
            .explain("connection closed: initialize response".into())
            .await;
        assert_eq!(
            message,
            "connection closed: initialize response (server stderr: line 4 | line 5 | line 6 | \
             line 7 | line 8 | line 9 | line 10 | env: node: No such file or directory)"
        );
    }

    #[tokio::test]
    async fn explain_leaves_the_error_alone_when_the_server_was_silent() {
        let tail = StderrTail::watch(None::<std::io::Cursor<Vec<u8>>>);
        assert_eq!(tail.explain("closed".into()).await, "closed");
    }
}
