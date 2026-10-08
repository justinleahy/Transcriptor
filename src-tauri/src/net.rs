//! Small HTTP helpers: bounded response bodies.

use anyhow::{anyhow, Context, Result};
use futures_util::{Stream, StreamExt};

/// Read a response body as text, refusing anything larger than `cap` bytes.
/// Rejects up front when `Content-Length` is over the cap, and otherwise
/// counts bytes while streaming so a server cannot make us buffer unbounded data.
pub async fn read_text_capped(resp: reqwest::Response, cap: u64, what: &str) -> Result<String> {
    if let Some(len) = resp.content_length() {
        if len > cap {
            return Err(anyhow!("{what} response is too large ({len} bytes; limit {cap})"));
        }
    }
    let bytes = collect_capped(resp.bytes_stream(), cap, what).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Collect a byte stream, failing as soon as more than `cap` bytes arrive.
pub async fn collect_capped<S, B, E>(stream: S, cap: u64, what: &str) -> Result<Vec<u8>>
where
    S: Stream<Item = std::result::Result<B, E>>,
    B: AsRef<[u8]>,
    E: std::error::Error + Send + Sync + 'static,
{
    let mut stream = std::pin::pin!(stream);
    let mut buf: Vec<u8> = Vec::new();
    while let Some(item) = stream.next().await {
        let chunk = item.with_context(|| format!("reading {what} response"))?;
        let chunk = chunk.as_ref();
        if buf.len() as u64 + chunk.len() as u64 > cap {
            return Err(anyhow!("{what} response exceeded the {cap}-byte limit"));
        }
        buf.extend_from_slice(chunk);
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks(sizes: &[usize]) -> impl Stream<Item = std::result::Result<Vec<u8>, std::io::Error>> {
        let v: Vec<_> = sizes.iter().map(|&n| Ok(vec![b'x'; n])).collect();
        futures_util::stream::iter(v)
    }

    #[tokio::test]
    async fn collects_under_cap() {
        let out = collect_capped(chunks(&[4, 4, 2]), 10, "test").await.unwrap();
        assert_eq!(out.len(), 10);
    }

    #[tokio::test]
    async fn aborts_over_cap() {
        let err = collect_capped(chunks(&[4, 4, 3]), 10, "test").await.unwrap_err();
        assert!(err.to_string().contains("limit"));
    }

    #[tokio::test]
    async fn propagates_stream_errors() {
        let s = futures_util::stream::iter(vec![
            Ok(vec![1u8]),
            Err(std::io::Error::other("boom")),
        ]);
        assert!(collect_capped(s, 10, "test").await.is_err());
    }
}
