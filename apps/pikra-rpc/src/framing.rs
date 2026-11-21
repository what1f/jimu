use serde::Serialize;
use std::io;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Strict LF-only JSONL reader operating on raw bytes.
///
/// Splits records strictly on byte `0x0A` (`\n`) and strips an optional preceding `0x0D` (`\r`).
/// Never splits on Unicode line/paragraph separators (`U+2028` / `U+2029`), which are legal
/// inside JSON strings.
pub struct JsonlReader<R> {
    reader: BufReader<R>,
    buf: Vec<u8>,
}

impl<R: tokio::io::AsyncRead + Unpin> JsonlReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            reader: BufReader::new(inner),
            buf: Vec::with_capacity(4096),
        }
    }

    /// Read the next non-empty LF-delimited record.
    ///
    /// Returns:
    /// - `Ok(None)` on EOF
    /// - `Ok(Some(Ok(line)))` for a valid UTF-8 record
    /// - `Ok(Some(Err(msg)))` for invalid UTF-8 bytes in a record (caller emits a parse error)
    pub async fn next_record(&mut self) -> io::Result<Option<Result<String, String>>> {
        loop {
            self.buf.clear();
            let n = self.reader.read_until(b'\n', &mut self.buf).await?;
            if n == 0 {
                return Ok(None);
            }

            if self.buf.last() == Some(&b'\n') {
                self.buf.pop();
            }
            if self.buf.last() == Some(&b'\r') {
                self.buf.pop();
            }

            // Ignore blank lines composed solely of ASCII spaces/tabs.
            if self.buf.iter().all(|b| matches!(b, b' ' | b'\t')) {
                continue;
            }

            match String::from_utf8(std::mem::take(&mut self.buf)) {
                Ok(line) => return Ok(Some(Ok(line))),
                Err(err) => {
                    return Ok(Some(Err(format!("invalid UTF-8 in JSONL record: {err}"))));
                }
            }
        }
    }
}

/// Serialized async stdout JSONL writer guaranteeing atomic LF-terminated records.
#[derive(Clone)]
pub struct JsonlWriter {
    tx: mpsc::UnboundedSender<String>,
}

pub struct JsonlWriterGuard {
    writer: JsonlWriter,
    handle: JoinHandle<()>,
}

impl JsonlWriterGuard {
    pub fn spawn_stdout() -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let handle = tokio::spawn(async move {
            let mut out = BufWriter::new(tokio::io::stdout());
            while let Some(line) = rx.recv().await {
                if out.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                if out.write_all(b"\n").await.is_err() {
                    break;
                }
                if out.flush().await.is_err() {
                    break;
                }
            }
            let _ = out.flush().await;
        });

        Self {
            writer: JsonlWriter { tx },
            handle,
        }
    }

    pub fn writer(&self) -> JsonlWriter {
        self.writer.clone()
    }

    /// Close the channel and wait for all queued records to flush to stdout.
    pub async fn finish(self) {
        drop(self.writer);
        let _ = self.handle.await;
    }
}

impl JsonlWriter {
    pub fn send_json<T: Serialize>(&self, value: &T) {
        match serde_json::to_string(value) {
            Ok(line) => {
                let _ = self.tx.send(line);
            }
            Err(err) => {
                eprintln!("[pikra-rpc] failed to serialize stdout record: {err}");
            }
        }
    }
}
