use tokio::io::AsyncBufRead;
use tokio::io::AsyncBufReadExt;

pub async fn skip_http_headers<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<(), std::io::Error> {
    // Detects two consecutive line endings, which may be \r\n or \n.
    let mut empty_line = false;
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "EOF reached before end of headers",
            ));
        }

        for (i, &byte) in buf.iter().enumerate() {
            if byte == b'\n' {
                if empty_line {
                    reader.consume(i + 1);
                    return Ok(());
                }
                empty_line = true;
            } else if byte != b'\r' {
                empty_line = false;
            }
        }

        let len = buf.len();
        reader.consume(len);
    }
}
