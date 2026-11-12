use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot, watch};

use super::messages::{Code, Fault, Incoming, Outgoing};

pub(super) const CAPACITY: usize = 128;
const MAX_FRAME_BYTES: usize = 1024 * 1024;
const MAX_BUFFER_BYTES: usize = MAX_FRAME_BYTES + 1;
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) enum Frame {
    Message(Incoming),
    Invalid(String),
    Failed(std::io::Error),
}

pub(super) struct Connection {
    input: mpsc::Receiver<Frame>,
    output: Option<mpsc::Sender<Vec<u8>>>,
    completion: oneshot::Receiver<anyhow::Result<()>>,
    progress: watch::Receiver<Option<Instant>>,
    input_closed: bool,
}

impl Connection {
    pub(super) fn open() -> anyhow::Result<Self> {
        Self::connect(std::io::BufReader::new(std::io::stdin()), std::io::stdout())
    }

    fn connect(
        reader: impl BufRead + Send + 'static,
        writer: impl Write + Send + 'static,
    ) -> anyhow::Result<Self> {
        let (input_sender, input) = mpsc::channel(CAPACITY);
        let (output, receiver) = mpsc::channel(CAPACITY);
        let (finished, completion) = oneshot::channel();
        let (activity, progress) = watch::channel(None);
        std::thread::Builder::new()
            .name("phi-stdin".into())
            .spawn(move || read(reader, input_sender))?;
        // Detached native threads cannot pin runtime teardown on blocked pipes.
        std::thread::Builder::new()
            .name("phi-stdout".into())
            .spawn(move || {
                let _ = finished.send(write(writer, receiver, activity));
            })?;
        Ok(Self {
            input,
            output: Some(output),
            completion,
            progress,
            input_closed: false,
        })
    }

    #[cfg(test)]
    pub(super) fn testing() -> (
        Self,
        mpsc::Sender<Frame>,
        std::sync::mpsc::Receiver<serde_json::Value>,
    ) {
        struct Capture(std::sync::mpsc::Sender<serde_json::Value>);
        impl Write for Capture {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if bytes != b"\n" {
                    let _ = self.0.send(serde_json::from_slice(bytes).unwrap());
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (sender, frames) = std::sync::mpsc::channel();
        let mut connection =
            Self::connect(std::io::Cursor::new(Vec::new()), Capture(sender)).unwrap();
        let (sender, input) = mpsc::channel(CAPACITY);
        connection.input = input;
        (connection, sender, frames)
    }

    pub(super) fn send(&self, message: Outgoing) -> Result<(), Fault> {
        let bytes = serde_json::to_vec(&message)
            .map_err(|error| Fault::new(Code::OutputUnavailable, error))?;
        if bytes.len() > MAX_FRAME_BYTES {
            return Err(Fault::new(
                Code::OutputUnavailable,
                "serialized output exceeds frame limit",
            ));
        }
        self.output
            .as_ref()
            .ok_or_else(|| Fault::new(Code::OutputUnavailable, "output is closed"))?
            .try_send(bytes)
            .map_err(|error| Fault::new(Code::OutputUnavailable, error))
    }

    pub(super) async fn receive(&mut self) -> Result<Option<Frame>, Fault> {
        tokio::select! {
            frame = self.input.recv(), if !self.input_closed => {
                if frame.is_none() { self.input_closed = true; }
                Ok(frame)
            },
            result = writer_status(&mut self.completion, &mut self.progress) => {
                result?;
                Err(Fault::new(Code::OutputUnavailable, "output writer stopped before close"))
            }
        }
    }

    pub(super) async fn flush(mut self) -> Result<(), Fault> {
        self.output.take();
        writer_status(&mut self.completion, &mut self.progress).await
    }
}

async fn writer_status(
    completion: &mut oneshot::Receiver<anyhow::Result<()>>,
    progress: &mut watch::Receiver<Option<Instant>>,
) -> Result<(), Fault> {
    loop {
        let started = *progress.borrow_and_update();
        let deadline = async {
            match started {
                Some(started) => tokio::time::sleep_until((started + WRITE_TIMEOUT).into()).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            result = &mut *completion => return result
                .map_err(|error| Fault::new(Code::OutputUnavailable, error))?
                .map_err(|error| Fault::new(Code::OutputUnavailable, error)),
            _ = deadline => return Err(Fault::new(Code::OutputUnavailable, "output write/flush timed out")),
            changed = progress.changed() => {
                if changed.is_err() {
                    return (&mut *completion).await
                        .map_err(|error| Fault::new(Code::OutputUnavailable, error))?
                        .map_err(|error| Fault::new(Code::OutputUnavailable, error));
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FrameSize {
    WithinLimit,
    Exceeded,
}

fn read(mut reader: impl BufRead, sender: mpsc::Sender<Frame>) {
    let mut frame = Vec::new();
    let mut size = FrameSize::WithinLimit;
    loop {
        let buffer = match reader.fill_buf() {
            Ok(buffer) => buffer,
            Err(error) => {
                let _ = sender.blocking_send(Frame::Failed(error));
                return;
            }
        };
        if buffer.is_empty() {
            if size == FrameSize::Exceeded || !frame.is_empty() {
                let _ = send_frame(&sender, &frame, size);
            }
            return;
        }
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let length = newline.unwrap_or(buffer.len());
        if size == FrameSize::WithinLimit {
            if length > MAX_BUFFER_BYTES - frame.len() {
                size = FrameSize::Exceeded;
                frame.clear();
            } else {
                frame.extend_from_slice(&buffer[..length]);
            }
        }
        reader.consume(length + usize::from(newline.is_some()));
        if newline.is_some() {
            if !send_frame(&sender, &frame, size) {
                return;
            }
            frame.clear();
            size = FrameSize::WithinLimit;
        }
    }
}

fn send_frame(sender: &mpsc::Sender<Frame>, frame: &[u8], size: FrameSize) -> bool {
    let frame = frame.strip_suffix(b"\r").unwrap_or(frame);
    let frame = if size == FrameSize::Exceeded || frame.len() > MAX_FRAME_BYTES {
        Frame::Invalid(format!("input frame exceeds {MAX_FRAME_BYTES} bytes"))
    } else if frame.iter().all(u8::is_ascii_whitespace) {
        return true;
    } else {
        match serde_json::from_slice(frame) {
            Ok(message) => Frame::Message(message),
            Err(error) => Frame::Invalid(error.to_string()),
        }
    };
    sender.blocking_send(frame).is_ok()
}

fn write(
    mut writer: impl Write,
    mut receiver: mpsc::Receiver<Vec<u8>>,
    activity: watch::Sender<Option<Instant>>,
) -> anyhow::Result<()> {
    while let Some(frame) = receiver.blocking_recv() {
        activity.send_replace(Some(Instant::now()));
        writer.write_all(&frame)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        activity.send_replace(None);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_frames_recover_across_chunk_sizes_delimiters_and_eof() {
        for capacity in [1, 7, 8192] {
            for length in [MAX_FRAME_BYTES - 1, MAX_FRAME_BYTES, MAX_FRAME_BYTES + 1] {
                for delimiter in [b"".as_slice(), b"\n", b"\r\n"] {
                    let mut bytes = vec![b'x'; length];
                    bytes.extend_from_slice(delimiter);
                    if !delimiter.is_empty() {
                        bytes.extend_from_slice(b"{\"type\":\"control\",\"control_id\":\"last\",\"request\":{\"kind\":\"interrupt\"}}");
                    }
                    let (sender, mut received) = mpsc::channel(3);
                    read(
                        std::io::BufReader::with_capacity(capacity, std::io::Cursor::new(bytes)),
                        sender,
                    );
                    assert!(matches!(received.blocking_recv(), Some(Frame::Invalid(_))));
                    if !delimiter.is_empty() {
                        assert!(
                            matches!(received.blocking_recv(), Some(Frame::Message(Incoming::Control { control_id, .. })) if control_id == "last")
                        );
                    }
                    assert!(received.blocking_recv().is_none());
                }
            }
        }
    }

    #[test]
    fn whitespace_old_envelopes_and_final_objects_are_unambiguous() {
        let (sender, mut received) = mpsc::channel(3);
        read(std::io::Cursor::new(b" \r\n{\"method\":\"initialize\"}\n{\"type\":\"control\",\"control_id\":\"x\",\"request\":{\"kind\":\"close\",\"mode\":\"drain\"}}"), sender);
        assert!(matches!(received.blocking_recv(), Some(Frame::Invalid(_))));
        assert!(matches!(received.blocking_recv(), Some(Frame::Message(_))));
        assert!(received.blocking_recv().is_none());
    }
    #[test]
    fn valid_maximum_frames_do_not_count_crlf_delimiters() {
        let object =
            b"{\"type\":\"control\",\"control_id\":\"max\",\"request\":{\"kind\":\"interrupt\"}}";
        for capacity in [1, 7, 8192] {
            for delimiter in [b"".as_slice(), b"\n", b"\r\n"] {
                for length in [MAX_FRAME_BYTES - 1, MAX_FRAME_BYTES, MAX_FRAME_BYTES + 1] {
                    let mut bytes = object.to_vec();
                    bytes.resize(length, b' ');
                    bytes.extend_from_slice(delimiter);
                    let (sender, mut received) = mpsc::channel(1);
                    read(
                        std::io::BufReader::with_capacity(capacity, std::io::Cursor::new(bytes)),
                        sender,
                    );
                    assert_eq!(
                        matches!(received.blocking_recv(), Some(Frame::Message(_))),
                        length <= MAX_FRAME_BYTES
                    );
                }
            }
        }
    }

    struct BlockedWriter {
        started: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    }
    impl Write for BlockedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            let _ = self.started.send(());
            self.release
                .recv()
                .map_err(|_| std::io::ErrorKind::BrokenPipe)?;
            Ok(())
        }
    }

    #[tokio::test]
    async fn physical_flush_deadline_and_queue_limits_are_independent_of_input() {
        let (started, waiting) = std::sync::mpsc::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let mut connection = Connection::connect(
            std::io::Cursor::new(Vec::new()),
            BlockedWriter {
                started,
                release: gate,
            },
        )
        .unwrap();
        connection
            .send(Outgoing::notice(Fault::new(Code::InvalidMessage, "test")))
            .unwrap();
        waiting.recv_timeout(WRITE_TIMEOUT).unwrap();
        for _ in 0..CAPACITY {
            connection
                .send(Outgoing::notice(Fault::new(Code::InvalidMessage, "queued")))
                .unwrap();
        }
        assert_eq!(
            connection
                .send(Outgoing::notice(Fault::new(Code::InvalidMessage, "full")))
                .unwrap_err()
                .code,
            Code::OutputUnavailable
        );
        assert_eq!(
            writer_status(&mut connection.completion, &mut connection.progress)
                .await
                .unwrap_err()
                .code,
            Code::OutputUnavailable
        );
        drop(connection);
        drop(release);
    }

    #[test]
    fn serialized_output_is_bounded_before_queue_admission() {
        let connection =
            Connection::connect(std::io::Cursor::new(Vec::new()), std::io::sink()).unwrap();
        let result = connection.send(Outgoing::Notice {
            code: Code::OperationFailed,
            message: "x".repeat(MAX_FRAME_BYTES),
            agent_id: None,
        });
        assert_eq!(result.unwrap_err().code, Code::OutputUnavailable);
    }
}
