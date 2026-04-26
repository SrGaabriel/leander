use tokio::io::{self, AsyncBufReadExt, AsyncReadExt};

pub async fn read_message<R>(r: &mut R) -> io::Result<Option<(Vec<u8>, Vec<u8>)>>
where
    R: AsyncBufReadExt + Unpin,
{
    let mut header = Vec::new();
    let mut content_length: Option<usize> = None;

    loop {
        let start = header.len();
        let n = r.read_until(b'\n', &mut header).await?;
        if n == 0 {
            return if header.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "EOF inside LSP headers",
                ))
            };
        }
        let line = &header[start..];
        if line == b"\r\n" || line == b"\n" {
            break;
        }
        if let Some(rest) = line.strip_prefix(b"Content-Length:") {
            let s = std::str::from_utf8(rest).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "non-utf8 Content-Length")
            })?;
            content_length = Some(s.trim().parse().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid Content-Length")
            })?);
        }
    }

    let len = content_length.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length header")
    })?;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(Some((header, body)))
}

pub fn encode_frame(body: &[u8]) -> Vec<u8> {
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body);
    out
}
