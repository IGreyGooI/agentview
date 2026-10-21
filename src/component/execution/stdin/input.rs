//! Stdin reads must not keep Tokio's blocking pool alive after frontend errors.

use std::{
    io::{self, Read},
    pin::Pin,
    task::{Context, Poll},
    thread,
};

use tokio::{
    io::{AsyncRead, ReadBuf},
    sync::mpsc,
};

pub(super) fn stdin() -> io::Result<impl AsyncRead + Unpin> {
    from_reader(io::stdin())
}

fn from_reader(mut reader: impl Read + Send + 'static) -> io::Result<Input> {
    let (sender, receiver) = mpsc::channel(1);
    // An idle OS read cannot be cancelled. A detached standard thread lets the
    // frontend and Tokio runtime still exit if a request or stdout write fails.
    thread::Builder::new()
        .name("agentview-stdin".to_owned())
        .spawn(move || {
            let mut bytes = [0; 8192];
            loop {
                let result = match reader.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(length) => Ok(bytes[..length].to_vec()),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => Err(error),
                };
                let failed = result.is_err();
                if sender.blocking_send(result).is_err() || failed {
                    break;
                }
            }
        })?;
    Ok(Input {
        receiver,
        bytes: Vec::new(),
        offset: 0,
    })
}

struct Input {
    receiver: mpsc::Receiver<io::Result<Vec<u8>>>,
    bytes: Vec<u8>,
    offset: usize,
}

impl AsyncRead for Input {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let input = self.get_mut();
        if output.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            if input.offset < input.bytes.len() {
                let length = output.remaining().min(input.bytes.len() - input.offset);
                output.put_slice(&input.bytes[input.offset..input.offset + length]);
                input.offset += length;
                return Poll::Ready(Ok(()));
            }
            match input.receiver.poll_recv(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    input.bytes = bytes;
                    input.offset = 0;
                }
                Poll::Ready(Some(Err(error))) => return Poll::Ready(Err(error)),
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::from_reader;
    use std::io::{self, Cursor, Read};
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn small_reads_preserve_large_binary_input_and_eof() {
        let expected = (0..=255).cycle().take(24_577).collect::<Vec<u8>>();
        let mut input = from_reader(Cursor::new(expected.clone())).unwrap();
        let mut actual = Vec::new();
        let mut bytes = [0; 7];
        loop {
            let length = input.read(&mut bytes).await.unwrap();
            if length == 0 {
                break;
            }
            actual.extend_from_slice(&bytes[..length]);
        }
        assert_eq!(actual, expected);
        assert_eq!(input.read(&mut bytes).await.unwrap(), 0);
    }

    struct InterruptedThenError {
        reads: usize,
    }

    impl Read for InterruptedThenError {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            self.reads += 1;
            match self.reads {
                1 => Err(io::ErrorKind::Interrupted.into()),
                2 => {
                    output[..3].copy_from_slice(b"abc");
                    Ok(3)
                }
                _ => Err(io::Error::other("input unavailable")),
            }
        }
    }

    #[tokio::test]
    async fn interrupted_reads_retry_and_input_errors_follow_prior_bytes() {
        let mut input = from_reader(InterruptedThenError { reads: 0 }).unwrap();
        let mut bytes = [0; 3];
        input.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"abc");
        let error = input.read(&mut bytes).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error.to_string(), "input unavailable");
        assert_eq!(input.read(&mut bytes).await.unwrap(), 0);
    }
}
