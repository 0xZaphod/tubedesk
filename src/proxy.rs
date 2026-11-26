//! Local HTTP proxy between the player and googlevideo.com.
//!
//! YouTube answers 403 to stream requests that don't ask for a byte range, and
//! throttles large ones. The player talks to
//! `http://127.0.0.1:<port>/s/<id>` instead, and this proxy fetches the
//! upstream URL in ranged chunks (`&range=a-b`, like yt-dlp does). Seeking
//! works because incoming `Range:` headers are honoured.
//!
//! Each stream can have two upstream URLs for the *same file*:
//! - the **full** URL from yt-dlp (with the headers it needs). It is refused
//!   (403) for ~3 s after being issued, then serves the whole file.
//! - a **quick** URL from rustypipe's iOS client, available instantly but only
//!   for the first ~500 KB (no PO token). It lets a song start right away
//!   while the full URL is still being looked up / warming up.
//!
//! Upcoming tracks are also *warmed*: their first chunk is fetched in the
//! background and kept in memory, so they start instantly when played.

use futures_util::StreamExt;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const CHUNK: u64 = 1 << 20; // 1 MiB per upstream request
const RETRIES: u32 = 12;
const RETRY_DELAY: Duration = Duration::from_millis(500);
/// Streams kept registered (current, prefetched, recently played).
const MAX_STREAMS: usize = 24; // at most ~24 MiB of warmed starts
/// Quick (iOS) URLs serve bytes below this offset; YouTube refuses more
/// without a PO token (tested limit ~500 KB, kept with a margin).
pub const QUICK_LIMIT: u64 = 450_000;
/// A full URL is refused this long after being issued.
const FULL_WARMUP: Duration = Duration::from_millis(3500);
/// How long a request past the quick part waits for the full URL to arrive.
const FULL_WAIT: Duration = Duration::from_secs(30);
/// Upstream errors in a row (connection drops mid-chunk) before giving up.
const NET_ERRORS: u32 = 4;
/// A stalled upstream read errors out (and is retried) after this long.
/// Only runs while a read is pending, so a paused stream doesn't trip it.
const READ_TIMEOUT: Duration = Duration::from_secs(20);
/// A full URL this close to its expiry is replaced before the next chunk.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

type RefreshFn = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Clone)]
struct Full {
    url: String,
    headers: Vec<(String, String)>,
    registered: Instant,
    expires: Option<Instant>,
}

#[derive(Clone)]
struct Upstream {
    full: Option<Full>,
    quick: Option<String>,
    size: u64,
    mime: String,
    /// First bytes of the stream, fetched ahead of time by [`StreamProxy::warm`].
    head: Option<Arc<Vec<u8>>>,
    /// A `warm` is downloading the head.
    warming: bool,
}

#[derive(Default)]
struct Streams {
    map: HashMap<String, Upstream>,
    /// Registration order, oldest first, for eviction.
    order: Vec<String>,
    /// Streams worth warming now (the next tracks), set by the core.
    upcoming: HashSet<String>,
}

impl Streams {
    fn touch(&mut self, id: &str) {
        self.order.retain(|x| x != id);
        self.order.push(id.to_string());
        while self.order.len() > MAX_STREAMS {
            let old = self.order.remove(0);
            self.map.remove(&old);
        }
    }
}

#[derive(Clone)]
pub struct StreamProxy {
    port: u16,
    streams: Arc<Mutex<Streams>>,
    http: reqwest::Client,
    /// Asks for a new full URL of a stream (set by the core).
    refresh: Arc<Mutex<Option<RefreshFn>>>,
}

impl StreamProxy {
    pub async fn start() -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let proxy = Self {
            port,
            streams: Arc::default(),
            refresh: Arc::default(),
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .read_timeout(READ_TIMEOUT)
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

    /// Sets how to ask for a new full URL when one expired or is refused.
    pub fn on_refresh(&self, f: impl Fn(&str) + Send + Sync + 'static) {
        *self.refresh.lock().unwrap() = Some(Arc::new(f));
    }

    /// Drops `full` (if still current) and asks for a new full URL; requests
    /// then wait for it in `wait_full`. Once per connection (`done`).
    fn request_refresh(&self, id: &str, full: &Full, done: &mut bool) -> bool {
        if *done {
            return false;
        }
        *done = true;
        if let Some(u) = self.streams.lock().unwrap().map.get_mut(id) {
            if u.full.as_ref().is_some_and(|f| f.url == full.url) {
                u.full = None;
            }
        }
        let f = self.refresh.lock().unwrap().clone();
        match f {
            Some(f) => {
                f(id);
                true
            }
            None => false,
        }
    }

    fn local_url(&self, id: &str) -> String {
        format!("http://127.0.0.1:{}/s/{}", self.port, id)
    }

    /// Registers the full (yt-dlp) URL of a stream and returns the local URL
    /// for the player. Re-registering the same URL keeps its warmed head; a
    /// quick URL registered earlier is kept if the file sizes match.
    pub fn register(&self, id: &str, s: &crate::resolver::Stream) -> String {
        let mut st = self.streams.lock().unwrap();
        let full = Full { url: s.url.clone(), headers: s.headers.clone(), registered: Instant::now(), expires: s.expires };
        match st.map.get_mut(id) {
            Some(u) if u.full.as_ref().is_some_and(|f| f.url == s.url) => {}
            Some(u) => {
                if u.size != s.size {
                    // Not the same file as the quick URL: never mix their bytes.
                    log::warn!("{id}: quick and full streams differ in size ({} vs {}); dropping quick", u.size, s.size);
                    u.quick = None;
                    u.head = None;
                    u.size = s.size;
                    u.mime = s.mime.clone();
                }
                if u.full.is_some() {
                    u.head = None; // a different full URL: its warmed head is stale
                }
                u.full = Some(full);
            }
            None => {
                st.map.insert(
                    id.to_string(),
                    Upstream { full: Some(full), quick: None, size: s.size, mime: s.mime.clone(), head: None, warming: false },
                );
            }
        }
        st.touch(id);
        self.local_url(id)
    }

    /// Registers a quick (iOS) URL for the start of a stream, so playback can
    /// begin before the full URL is ready. Returns the local URL.
    pub fn register_quick(&self, id: &str, url: String, size: u64, mime: String) -> String {
        let mut st = self.streams.lock().unwrap();
        match st.map.get_mut(id) {
            Some(u) if u.size == size => u.quick = Some(url),
            Some(_) => log::warn!("{id}: quick stream size differs from the full one; not using it"),
            None => {
                st.map.insert(id.to_string(), Upstream { full: None, quick: Some(url), size, mime, head: None, warming: false });
            }
        }
        st.touch(id);
        self.local_url(id)
    }

    /// The streams worth warming now; a warm whose stream left this set
    /// while it waited out the warm-up is dropped.
    pub fn set_upcoming(&self, ids: Vec<String>) {
        self.streams.lock().unwrap().upcoming = ids.into_iter().collect();
    }

    /// Forgets a stream (e.g. its full URL could not be resolved), so
    /// requests waiting for its full URL give up right away.
    pub fn forget(&self, id: &str) {
        let mut st = self.streams.lock().unwrap();
        st.map.remove(id);
        st.order.retain(|x| x != id);
    }

    /// Fetches and keeps the first chunk of a registered stream so that
    /// playing it later starts immediately. Does nothing if it is already
    /// warm or being warmed.
    pub async fn warm(&self, id: &str) {
        let (full, size) = {
            let mut st = self.streams.lock().unwrap();
            let Some(u) = st.map.get_mut(id) else { return };
            let Some(full) = u.full.clone() else { return };
            if u.head.is_some() || u.warming || u.size == 0 {
                return;
            }
            u.warming = true;
            (full, u.size)
        };
        // A fresh URL is refused at first: wait instead of retrying into 403s.
        tokio::time::sleep(FULL_WARMUP.saturating_sub(full.registered.elapsed())).await;
        {
            let mut st = self.streams.lock().unwrap();
            if !st.upcoming.contains(id) {
                if let Some(u) = st.map.get_mut(id) {
                    u.warming = false;
                }
                return log::debug!("not warming {id}: no longer upcoming");
            }
        }
        let end = (CHUNK - 1).min(size - 1);
        let url = format!("{}&range=0-{end}", full.url);
        let bytes = match self.fetch(&full.headers, &url, RETRIES).await {
            Ok(Some(resp)) => resp.bytes().await.map_err(|e| e.to_string()),
            Ok(None) => Err("refused".to_string()),
            Err(e) => Err(e.to_string()),
        };
        let mut st = self.streams.lock().unwrap();
        let Some(u) = st.map.get_mut(id) else { return };
        u.warming = false;
        match bytes {
            Ok(b) if u.full.as_ref().is_some_and(|f| f.url == full.url) => {
                log::debug!("warmed {id}: {} bytes ready", b.len());
                u.head = Some(Arc::new(b.to_vec()));
            }
            Ok(_) => {} // a different full URL was registered meanwhile
            Err(e) => log::debug!("warming {id} failed: {e}"),
        }
    }

    /// GETs one chunk. Freshly issued full URLs answer 403 for the first ~3 s,
    /// so 403 and 5xx are retried up to `retries` times.
    async fn fetch(&self, headers: &[(String, String)], url: &str, retries: u32) -> std::io::Result<Option<reqwest::Response>> {
        for attempt in 0..retries {
            let mut req = self.http.get(url);
            for (k, v) in headers {
                req = req.header(k, v);
            }
            let resp = match req.send().await {
                Ok(r) => r,
                // A brief network problem: try again like a refusal.
                Err(e) if attempt + 1 < retries => {
                    log::debug!("upstream request failed ({e}), retrying ({})", attempt + 1);
                    tokio::time::sleep(RETRY_DELAY).await;
                    continue;
                }
                Err(e) => return Err(std::io::Error::other(e)),
            };
            let status = resp.status();
            if status.is_success() {
                return Ok(Some(resp));
            }
            if status != reqwest::StatusCode::FORBIDDEN && !status.is_server_error() {
                log::warn!("upstream returned {status}");
                return Ok(None);
            }
            if attempt + 1 < retries {
                log::debug!("upstream returned {status}, retrying ({})", attempt + 1);
                tokio::time::sleep(RETRY_DELAY).await;
            }
        }
        Ok(None)
    }

    /// The full URL of `id`, waiting (up to [`FULL_WAIT`]) for it to arrive.
    async fn wait_full(&self, id: &str) -> Option<Full> {
        let until = Instant::now() + FULL_WAIT;
        loop {
            match self.streams.lock().unwrap().map.get(id) {
                None => return None, // forgotten
                Some(u) => {
                    if let Some(f) = &u.full {
                        return Some(f.clone());
                    }
                }
            }
            if Instant::now() > until {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn serve(&self, mut sock: TcpStream) -> std::io::Result<()> {
        // Read the request head (small GET/HEAD requests).
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

        let id = path.strip_prefix("/s/").unwrap_or_default().to_string();
        let Some(up) = self.streams.lock().unwrap().map.get(&id).cloned() else {
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
        let mut refreshed = false;
        let mut net_errors = 0;
        while pos <= end {
            // Re-read each round: the full URL or a warmed head may arrive meanwhile.
            let Some(up) = self.streams.lock().unwrap().map.get(&id).cloned() else { return Ok(()) };

            // 1. Bytes fetched ahead of time.
            if let Some(head) = &up.head {
                if (pos as usize) < head.len() {
                    let stop = head.len().min(end as usize + 1);
                    sock.write_all(&head[pos as usize..stop]).await?;
                    pos = stop as u64;
                    continue;
                }
            }

            // 2. The quick URL, for the start of the file while the full URL
            //    is missing or still in its warm-up.
            let full_open = up.full.as_ref().is_some_and(|f| f.registered.elapsed() > FULL_WARMUP);
            if pos < QUICK_LIMIT && !full_open {
                if let Some(quick) = &up.quick {
                    let chunk_end = (QUICK_LIMIT - 1).min(end);
                    log::debug!("upstream {id}: bytes {pos}-{chunk_end} (quick)");
                    let url = format!("{quick}&range={pos}-{chunk_end}");
                    match self.fetch(&[], &url, 1).await {
                        Ok(Some(resp)) => match pipe(&mut sock, resp, &mut pos).await? {
                            // Dropped mid-way: go on from `pos` with the full URL.
                            Some(e) => {
                                log::debug!("{id}: quick URL failed at {pos} ({e}); using the full URL");
                                if let Some(u) = self.streams.lock().unwrap().map.get_mut(&id) {
                                    u.quick = None;
                                }
                            }
                            None if pos <= chunk_end => return Ok(()), // upstream ended early
                            None => {}
                        },
                        Ok(None) | Err(_) => {
                            log::debug!("{id}: quick URL refused at {pos}; using the full URL");
                            if let Some(u) = self.streams.lock().unwrap().map.get_mut(&id) {
                                u.quick = None;
                            }
                        }
                    }
                    continue;
                }
            }

            // 3. The full URL (waiting for it if it isn't known yet).
            let Some(full) = self.wait_full(&id).await else {
                log::warn!("{id}: no full stream URL arrived");
                return Ok(());
            };
            // E.g. after a long pause: YouTube would refuse it.
            if full.expires.is_some_and(|t| t <= Instant::now() + EXPIRY_MARGIN) {
                log::info!("{id}: stream URL expired; asking for a new one");
                if !self.request_refresh(&id, &full, &mut refreshed) {
                    return Ok(());
                }
                continue;
            }
            let warmup = FULL_WARMUP.saturating_sub(full.registered.elapsed());
            if !warmup.is_zero() {
                log::debug!("{id}: full URL still warming up; waiting {} ms", warmup.as_millis());
                tokio::time::sleep(warmup).await;
            }
            let chunk_end = (pos + CHUNK - 1).min(end);
            let url = format!("{}&range={pos}-{chunk_end}", full.url);
            log::debug!("upstream {id}: bytes {pos}-{chunk_end}");
            let Some(resp) = self.fetch(&full.headers, &url, RETRIES).await? else {
                log::warn!("upstream kept refusing {id} at byte {pos}");
                if self.request_refresh(&id, &full, &mut refreshed) {
                    continue;
                }
                return Ok(());
            };
            // Socket errors (the player seeked away or stopped) end the connection.
            if let Some(e) = pipe(&mut sock, resp, &mut pos).await? {
                net_errors += 1;
                if net_errors >= NET_ERRORS {
                    log::warn!("{id}: upstream keeps failing at byte {pos}: {e}");
                    return Ok(());
                }
                log::debug!("{id}: upstream failed at byte {pos} ({e}); retrying from there");
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
            if pos <= chunk_end {
                return Ok(()); // upstream ended early
            }
            net_errors = 0;
        }
        Ok(())
    }
}

/// Copies an upstream body to the socket, advancing `pos`. A socket error is
/// returned as `Err` (end the connection); an upstream error mid-body as
/// `Ok(Some(_))`, so the caller can continue from `pos`.
async fn pipe(sock: &mut TcpStream, resp: reqwest::Response, pos: &mut u64) -> std::io::Result<Option<reqwest::Error>> {
    let mut body = resp.bytes_stream();
    while let Some(bytes) = body.next().await {
        match bytes {
            Ok(b) => {
                sock.write_all(&b).await?;
                *pos += b.len() as u64;
            }
            Err(e) => return Ok(Some(e)),
        }
    }
    Ok(None)
}
