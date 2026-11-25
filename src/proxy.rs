//! Local HTTP proxy between GStreamer and googlevideo.com.
//!
//! Upstream requests carry the headers yt-dlp says the URL needs.
//!
//! YouTube answers 403 to stream requests that don't ask for a byte range, and
//! throttles large ones. GStreamer's first request has no range, so it talks to
//! `http://127.0.0.1:<port>/s/<id>` instead, and this proxy fetches the
//! upstream URL in ranged chunks (`&range=a-b`, like yt-dlp does). Seeking
//! works because incoming `Range:` headers are honoured.
//!
//! Upcoming tracks are *warmed*: their first chunk is fetched in the
//! background (waiting out YouTube's ~2 s 403 phase for new URLs) and kept in
//! memory, so they start instantly when played.

use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const CHUNK: u64 = 1 << 20; // 1 MiB per upstream request
const RETRIES: u32 = 12;
const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(500);
/// Streams kept registered (current, prefetched, recently played).
const MAX_STREAMS: usize = 10;

#[derive(Clone)]
struct Upstream {
    url: String,
    headers: Vec<(String, String)>,
    size: u64,
    mime: String,
    /// First bytes of the stream, fetched ahead of time by [`StreamProxy::warm`].
    head: Option<Arc<Vec<u8>>>,
}

#[derive(Default)]
struct Streams {
    map: HashMap<String, Upstream>,
    /// Registration order, oldest first, for eviction.
    order: Vec<String>,
}

#[derive(Clone)]
pub struct StreamProxy {
    port: u16,
    streams: Arc<Mutex<Streams>>,
    http: reqwest::Client,
}

impl StreamProxy {
    pub async fn start() -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let proxy = Self {
            port,
            streams: Arc::default(),
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("http client"),
        };
        let p = proxy.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((sock, _)) => {
                        let p = p.clone();
                        tokio::spawn(async move {
                            if let Err(e) = p.serve(sock).await {
                                log::debug!("proxy connection ended: {e}");
                            }
                        });
                    }
                    Err(e) => log::warn!("proxy accept failed: {e}"),
                }
            }
        });
        log::info!("stream proxy on 127.0.0.1:{port}");
        Ok(proxy)
    }

    /// Registers an upstream stream and returns the local URL for GStreamer.
    /// Re-registering the same URL keeps its warmed head. Only the most recent
    /// [`MAX_STREAMS`] are kept.
    pub fn register(&self, id: &str, s: &crate::resolver::Stream) -> String {
        let mut st = self.streams.lock().unwrap();
        let same = st.map.get(id).is_some_and(|u| u.url == s.url);
        if !same {
            st.map.insert(
                id.to_string(),
                Upstream { url: s.url.clone(), headers: s.headers.clone(), size: s.size, mime: s.mime.clone(), head: None },
            );
        }
        st.order.retain(|x| x != id);
        st.order.push(id.to_string());
        while st.order.len() > MAX_STREAMS {
            let old = st.order.remove(0);
            st.map.remove(&old);
        }
        format!("http://127.0.0.1:{}/s/{}", self.port, id)
    }

    /// Fetches and keeps the first chunk of a registered stream so that
    /// playing it later starts immediately.
    pub async fn warm(&self, id: &str) {
        let Some(up) = self.streams.lock().unwrap().map.get(id).cloned() else { return };
        if up.head.is_some() || up.size == 0 {
            return;
        }
        let end = (CHUNK - 1).min(up.size - 1);
        let url = format!("{}&range=0-{end}", up.url);
        let bytes = match self.fetch(&up, &url).await {
            Ok(Some(resp)) => match resp.bytes().await {
                Ok(b) => b,
                Err(e) => return log::debug!("warming {id} failed: {e}"),
            },
            _ => return log::debug!("warming {id} failed"),
        };
        let mut st = self.streams.lock().unwrap();
        if let Some(u) = st.map.get_mut(id).filter(|u| u.url == up.url) {
            log::debug!("warmed {id}: {} bytes ready", bytes.len());
            u.head = Some(Arc::new(bytes.to_vec()));
        }
    }

    /// GETs one chunk. Freshly issued stream URLs answer 403 for the first
    /// ~2 seconds, so 403 and 5xx are retried for a few seconds.
    async fn fetch(&self, up: &Upstream, url: &str) -> std::io::Result<Option<reqwest::Response>> {
        for attempt in 0..RETRIES {
            let mut req = self.http.get(url);
            for (k, v) in &up.headers {
                req = req.header(k, v);
            }
            let resp = req.send().await.map_err(std::io::Error::other)?;
            let status = resp.status();
            if status.is_success() {
                return Ok(Some(resp));
            }
            if status != reqwest::StatusCode::FORBIDDEN && !status.is_server_error() {
                log::warn!("upstream returned {status}");
                return Ok(None);
            }
            log::debug!("upstream returned {status}, retrying ({})", attempt + 1);
            tokio::time::sleep(RETRY_DELAY).await;
        }
        Ok(None)
    }

    async fn serve(&self, mut sock: TcpStream) -> std::io::Result<()> {
        // Read the request head (GStreamer sends small GET/HEAD requests).
        let mut buf = Vec::with_capacity(2048);
        let mut tmp = [0u8; 1024];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = sock.read(&mut tmp).await?;
            if n == 0 || buf.len() > 16 * 1024 {
                return Ok(());
            }
            buf.extend_from_slice(&tmp[..n]);
        }
        let head = String::from_utf8_lossy(&buf);
        let mut lines = head.lines();
        let request_line = lines.next().unwrap_or_default();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let path = parts.next().unwrap_or_default();
        let range = lines
            .filter_map(|l| l.split_once(':'))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case("range"))
            .map(|(_, v)| v.trim().to_string());

        let id = path.strip_prefix("/s/").unwrap_or_default();
        let Some(up) = self.streams.lock().unwrap().map.get(id).cloned() else {
            sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
            return Ok(());
        };
        if up.size == 0 {
            sock.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
            return Ok(());
        }

        // Parse "bytes=start-[end]".
        let last = up.size - 1;
        let (start, end, partial) = match range.as_deref().and_then(|r| r.strip_prefix("bytes=")) {
            Some(spec) => {
                let (a, b) = spec.split_once('-').unwrap_or((spec, ""));
                let start = a.trim().parse::<u64>().unwrap_or(0);
                let end = b.trim().parse::<u64>().map_or(last, |e| e.min(last));
                (start, end, true)
            }
            None => (0, last, false),
        };
        if start > end {
            let msg = format!(
                "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                up.size
            );
            sock.write_all(msg.as_bytes()).await?;
            return Ok(());
        }

        let len = end - start + 1;
        let mut header = format!(
            "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {len}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n",
            if partial { "206 Partial Content" } else { "200 OK" },
            up.mime,
        );
        if partial {
            header.push_str(&format!("Content-Range: bytes {start}-{end}/{}\r\n", up.size));
        }
        header.push_str("\r\n");
        sock.write_all(header.as_bytes()).await?;
        if method.eq_ignore_ascii_case("HEAD") {
            return Ok(());
        }

        let mut pos = start;
        // Serve what was fetched ahead of time, then continue upstream.
        if let Some(head) = &up.head {
            if (pos as usize) < head.len() {
                let stop = head.len().min(end as usize + 1);
                sock.write_all(&head[pos as usize..stop]).await?;
                pos = stop as u64;
            }
        }
        while pos <= end {
            let chunk_end = (pos + CHUNK - 1).min(end);
            let url = format!("{}&range={pos}-{chunk_end}", up.url);
            log::debug!("upstream {id}: bytes {pos}-{chunk_end}");
            let resp = self.fetch(&up, &url).await?;
            let Some(resp) = resp else {
                log::warn!("upstream kept refusing {id} at byte {pos}");
                return Ok(());
            };
            let mut body = resp.bytes_stream();
            while let Some(bytes) = body.next().await {
                let bytes = bytes.map_err(std::io::Error::other)?;
                sock.write_all(&bytes).await?; // errors when GStreamer seeks away; that's fine
                pos += bytes.len() as u64;
            }
            if pos <= chunk_end {
                return Ok(()); // upstream ended early
            }
        }
        Ok(())
    }
}
