//! Process HTTP connections on the server.

use async_std::future::{timeout, Future, TimeoutError};
use async_std::io::{self, BufRead, BufReader};
use async_std::io::{Read, Write};
use async_std::prelude::*;
use async_std::task::{Context, Poll};
use futures_core::ready;
use http_types::{Method, Request, Response};
use std::str::FromStr;
use std::time::Duration;

use std::pin::Pin;

use crate::{Exception, MAX_HEADERS};

pub async fn connect<'a, R, W, F, Fut>(
    reader: R,
    mut writer: W,
    callback: F,
) -> Result<(), Exception>
where
    R: Read + Unpin + Send + 'static,
    W: Write + Unpin,
    F: Fn(&mut Request) -> Fut,
    Fut: Future<Output = Result<Response, Exception>>,
{
    // TODO: make configurable
    let timeout_duration = Duration::from_secs(10);
    const MAX_REQUESTS: usize = 200;

    let req = decode(reader).await?;
    let mut num_requests = 0;
    if let Some((mut req, stream)) = req {
        let mut stream: Option<Box<dyn BufRead + Unpin + Send + 'static>> = match stream {
            Some(s) => Some(Box::new(s)),
            None => None,
        };
        loop {
            num_requests += 1;
            if num_requests > MAX_REQUESTS {
                return Ok(());
            }

            // TODO: what to do when the callback returns Err
            let mut res = encode(callback(&mut req).await?).await?;
            let to_decode = match stream {
                None => req.into_body(),
                Some(s) => s,
            };
            io::copy(&mut res, &mut writer).await?;
            let (new_request, new_stream) = match timeout(timeout_duration, decode(to_decode)).await
            {
                Ok(Ok(Some(r))) => r,
                Ok(Ok(None)) | Err(TimeoutError { .. }) => break, /* EOF or timeout */
                Ok(Err(e)) => return Err(e),
            };
            req = new_request;
            stream = match new_stream {
                Some(s) => Some(Box::new(s)),
                None => None,
            };
        }
    }

    Ok(())
}

/// A streaming HTTP encoder.
///
/// This is returned from [`encode`].
#[derive(Debug)]
pub struct Encoder {
    /// Keep track how far we've indexed into the headers + body.
    cursor: usize,
    /// HTTP headers to be sent.
    headers: Vec<u8>,
    /// Check whether we're done sending headers.
    headers_done: bool,
    /// Response containing the HTTP body to be sent.
    response: Response,
    /// Check whether we're done with the body.
    body_done: bool,
    /// Keep track of how many bytes have been read from the body stream.
    body_bytes_read: usize,
}

impl Encoder {
    /// Create a new instance.
    pub(crate) fn new(headers: Vec<u8>, response: Response) -> Self {
        Self {
            response,
            headers,
            cursor: 0,
            headers_done: false,
            body_done: false,
            body_bytes_read: 0,
        }
    }
}

impl Read for Encoder {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        // Send the headers. As long as the headers aren't fully sent yet we
        // keep sending more of the headers.
        let mut bytes_read = 0;
        if !self.headers_done {
            let len = std::cmp::min(self.headers.len() - self.cursor, buf.len());
            let range = self.cursor..self.cursor + len;
            buf[0..len].copy_from_slice(&mut self.headers[range]);
            self.cursor += len;
            if self.cursor == self.headers.len() {
                self.headers_done = true;
            }
            bytes_read += len;
        }

        if !self.body_done {
            let n = ready!(Pin::new(&mut self.response).poll_read(cx, &mut buf[bytes_read..]))?;
            bytes_read += n;
            self.body_bytes_read += n;
            if bytes_read == 0 {
                self.body_done = true;
            }
        }

        Poll::Ready(Ok(bytes_read as usize))
    }
}

/// Encode an HTTP request on the server.
// TODO: return a reader in the response
pub async fn encode(res: Response) -> io::Result<Encoder> {
    let mut buf: Vec<u8> = vec![];

    let reason = res.status().canonical_reason();
    let status = res.status();
    std::io::Write::write_fmt(&mut buf, format_args!("HTTP/1.1 {} {}\r\n", status, reason))?;

    // If the body isn't streaming, we can set the content-length ahead of time. Else we need to
    // send all items in chunks.
    if let Some(len) = res.len() {
        std::io::Write::write_fmt(&mut buf, format_args!("Content-Length: {}\r\n", len))?;
    } else {
        std::io::Write::write_fmt(&mut buf, format_args!("Transfer-Encoding: chunked\r\n"))?;
        panic!("chunked encoding is not implemented yet");
        // See: https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Transfer-Encoding
        //      https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Trailer
    }

    for (header, value) in res.headers().iter() {
        std::io::Write::write_fmt(&mut buf, format_args!("{}: {}\r\n", header.as_str(), value))?
    }

    std::io::Write::write_fmt(&mut buf, format_args!("\r\n"))?;
    Ok(Encoder::new(buf, res))
}

/// Decode an HTTP request on the server.
pub async fn decode<R>(reader: R) -> Result<Option<(Request, Option<BufReader<R>>)>, Exception>
where
    R: Read + Unpin + Send + 'static,
{
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut httparse_req = httparse::Request::new(&mut headers);

    // Keep reading bytes from the stream until we hit the end of the stream.
    loop {
        let bytes_read = reader.read_until(b'\n', &mut buf).await?;
        // No more bytes are yielded from the stream.
        if bytes_read == 0 {
            return Ok(None);
        }

        // We've hit the end delimiter of the stream.
        let idx = buf.len() - 1;
        if idx >= 3 && &buf[idx - 3..=idx] == b"\r\n\r\n" {
            break;
        }
    }

    // Convert our header buf into an httparse instance, and validate.
    let status = httparse_req.parse(&buf)?;
    if status.is_partial() {
        dbg!(String::from_utf8(buf).unwrap());
        return Err("Malformed HTTP head".into());
    }

    // Convert httparse headers + body into a `http::Request` type.
    let method = httparse_req.method.ok_or_else(|| "No method found")?;
    let uri = httparse_req.path.ok_or_else(|| "No uri found")?;
    let uri = url::Url::parse(uri)?;
    let version = httparse_req.version.ok_or_else(|| "No version found")?;
    if version != 1 {
        return Err("Unsupported HTTP version".into());
    }
    let mut req = Request::new(Method::from_str(method)?, uri);
    for header in httparse_req.headers.iter() {
        req = req.set_header(header.name, std::str::from_utf8(header.value)?)?;
    }

    // Process the body if `Content-Length` was passed.
    if let Some(content_length) = httparse_req
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("Content-Length"))
    {
        let length = std::str::from_utf8(content_length.value)
            .ok()
            .and_then(|s| s.parse::<usize>().ok());

        if let Some(len) = length {
            req = req.set_body(reader);
            req = req.set_len(len);

            // Return the request.
            Ok(Some((req, None)))
        } else {
            return Err("Invalid value for Content-Length".into());
        }
    } else {
        Ok(Some((req, Some(reader))))
    }
}
