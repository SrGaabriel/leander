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
    let mut out = Vec::with_capacity(32 + body.len());
    write_header(&mut out, body.len());
    out.extend_from_slice(body);
    out
}

pub fn write_header(out: &mut Vec<u8>, content_length: usize) {
    out.extend_from_slice(b"Content-Length: ");
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    let mut n = content_length;
    if n == 0 {
        i -= 1;
        buf[i] = b'0';
    } else {
        while n > 0 {
            i -= 1;
            buf[i] = b'0' + u8::try_from(n % 10).expect("digit fits in u8");
            n /= 10;
        }
    }
    out.extend_from_slice(&buf[i..]);
    out.extend_from_slice(b"\r\n\r\n");
}
