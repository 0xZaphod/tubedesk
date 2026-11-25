//! YouTube Music home feed (`FEmusic_home`), which rustypipe doesn't wrap.
//!
//! When signed in, this is the personalised home: pinned items ("Speed
//! dial"), Quick picks, Listen again, Mixed for you, recommendations, ...
//! The response is walked as untyped JSON; anything unrecognised is skipped
//! (and logged at debug level) rather than failing the whole page.

use rustypipe::client::{ClientType, RustyPipe};
use rustypipe::model::TrackItem;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HomeSection {
    pub title: String,
    /// "tracks" (song list) or "cards" (playlists, albums, mixes, ...)
    pub layout: String,
    pub items: Vec<HomeItem>,
}

/// One page of the home feed plus the token for the next page.
#[derive(Debug, Serialize)]
pub struct HomePage {
    pub sections: Vec<HomeSection>,
    pub next: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HomeItem {
    /// track | playlist | album | artist | mix
    pub kind: String,
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub thumbnails: Value,
    /// Present for kind == "track" (playable as-is).
    pub track: Option<TrackItem>,
}

/// Loads one page: the first one when `token` is None, else the continuation.
/// The UI renders each page as it arrives instead of waiting for all of them.
pub async fn load_page(rp: &RustyPipe, token: Option<String>) -> Result<HomePage, String> {
    // Raw requests are only signed when asked; use the account if there is one.
    let q = rp.query();
    let q = if q.auth_enabled(ClientType::DesktopMusic) { q.authenticated() } else { q };
    let body = match &token {
        None => json!({ "browseId": "FEmusic_home" }),
        Some(t) => json!({ "continuation": t }),
    };
    let page: Value = serde_json::from_str(&q.raw(ClientType::DesktopMusic, "browse", &body).await.map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let list = if token.is_none() {
        &page["contents"]["singleColumnBrowseResultsRenderer"]["tabs"][0]["tabRenderer"]["content"]["sectionListRenderer"]
    } else {
        &page["continuationContents"]["sectionListContinuation"]
    };
    let mut sections = Vec::new();
    parse_sections(&list["contents"], &mut sections);
    log::debug!(
        "home page: {}",
        sections.iter().map(|s| format!("{} ({})", s.title, s.items.len())).collect::<Vec<_>>().join(", ")
    );
    Ok(HomePage { sections, next: continuation(list) })
}

fn continuation(list: &Value) -> Option<String> {
    list["continuations"][0]["nextContinuationData"]["continuation"].as_str().map(String::from)
}

fn text(v: &Value) -> String {
    v["runs"].as_array().map_or_else(
        || v["simpleText"].as_str().unwrap_or_default().to_string(),
        |runs| runs.iter().filter_map(|r| r["text"].as_str()).collect(),
    )
}

fn parse_sections(contents: &Value, out: &mut Vec<HomeSection>) {
    for s in contents.as_array().into_iter().flatten() {
        let Some((kind, r)) = s.as_object().and_then(|o| o.iter().next()) else { continue };
        let (title, items) = match kind.as_str() {
            "musicCarouselShelfRenderer" | "musicImmersiveCarouselShelfRenderer" => {
                let h = &r["header"];
                let header = h
                    .get("musicCarouselShelfBasicHeaderRenderer")
                    .or_else(|| h.get("musicImmersiveCarouselShelfBasicHeaderRenderer"))
                    .unwrap_or(&Value::Null);
                (text(&header["title"]), &r["contents"])
            }
            "gridRenderer" | "musicGridRenderer" => (text(&r["header"]["gridHeaderRenderer"]["title"]), &r["items"]),
            other => {
                log::trace!("home: skipping section type {other}");
                continue;
            }
        };
        let items: Vec<HomeItem> = items.as_array().into_iter().flatten().filter_map(parse_item).collect();
        if items.is_empty() {
            continue;
        }
        let layout = if items.iter().all(|i| i.kind == "track") && items.len() > 4 { "tracks" } else { "cards" };
        out.push(HomeSection { title, layout: layout.into(), items });
    }
}

fn parse_item(v: &Value) -> Option<HomeItem> {
    if let Some(r) = v.get("musicTwoRowItemRenderer") {
        let title_runs = &r["title"];
        let thumbs = r["thumbnailRenderer"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"].clone();
        let subtitle = text(&r["subtitle"]);
        return item_from_endpoint(
            &r["navigationEndpoint"],
            text(title_runs),
            subtitle,
            thumbs,
            r["subtitle"]["runs"].as_array().cloned().unwrap_or_default(),
        );
    }
    if let Some(r) = v.get("musicResponsiveListItemRenderer") {
        let col = |i: usize| &r["flexColumns"][i]["musicResponsiveListItemFlexColumnRenderer"]["text"];
        let title = text(col(0));
        let meta_runs: Vec<Value> = (1..3).flat_map(|i| col(i)["runs"].as_array().cloned().unwrap_or_default()).collect();
        let subtitle = (1..3).map(|i| text(col(i))).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" • ");
        let thumbs = r["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"].clone();
        let video_id = r["playlistItemData"]["videoId"]
            .as_str()
            .or_else(|| {
                r["overlay"]["musicItemThumbnailOverlayRenderer"]["content"]["musicPlayButtonRenderer"]["playNavigationEndpoint"]
                    ["watchEndpoint"]["videoId"]
                    .as_str()
            })
            .or_else(|| col(0)["runs"][0]["navigationEndpoint"]["watchEndpoint"]["videoId"].as_str());
        if let Some(id) = video_id {
            return Some(track_item(id, title, subtitle, thumbs, &meta_runs));
        }
        return item_from_endpoint(&r["navigationEndpoint"], title, subtitle, thumbs, meta_runs);
    }
    None
}

fn item_from_endpoint(ep: &Value, title: String, subtitle: String, thumbs: Value, runs: Vec<Value>) -> Option<HomeItem> {
    if let Some(b) = ep.get("browseEndpoint") {
        let id = b["browseId"].as_str()?;
        let page = b["browseEndpointContextSupportedConfigs"]["browseEndpointContextMusicConfig"]["pageType"]
            .as_str()
            .unwrap_or_default();
        let (kind, id) = match page {
            "MUSIC_PAGE_TYPE_PLAYLIST" => ("playlist", id.strip_prefix("VL").unwrap_or(id)),
            "MUSIC_PAGE_TYPE_ALBUM" | "MUSIC_PAGE_TYPE_AUDIOBOOK" => ("album", id),
            "MUSIC_PAGE_TYPE_ARTIST" | "MUSIC_PAGE_TYPE_USER_CHANNEL" => ("artist", id),
            other => {
                log::trace!("home: skipping browse page type {other}");
                return None;
            }
        };
        return Some(HomeItem { kind: kind.into(), id: id.to_string(), title, subtitle, thumbnails: thumbs, track: None });
    }
    let watch = ep.get("watchEndpoint").or_else(|| ep.get("watchPlaylistEndpoint"))?;
    let playlist = watch["playlistId"].as_str();
    match (watch["videoId"].as_str(), playlist) {
        // A video id means "play this song" — even when it comes with a radio
        // playlist (RDAMVM…) that would merely continue after it. Playing that
        // radio instead would start with some other song.
        (Some(vid), _) => Some(track_item(vid, title, subtitle, thumbs, &runs)),
        // Mixes / radios without a seed song are generated playlists (RD…).
        (None, Some(pl)) if pl.starts_with("RD") => {
            Some(HomeItem { kind: "mix".into(), id: pl.to_string(), title, subtitle, thumbnails: thumbs, track: None })
        }
        (None, Some(pl)) => Some(HomeItem { kind: "playlist".into(), id: pl.to_string(), title, subtitle, thumbnails: thumbs, track: None }),
        (None, None) => None,
    }
}

/// Builds a playable rustypipe `TrackItem` (it is `non_exhaustive`, so via serde).
fn track_item(id: &str, title: String, subtitle: String, thumbs: Value, runs: &[Value]) -> HomeItem {
    let page_type = |r: &Value| {
        r["navigationEndpoint"]["browseEndpoint"]["browseEndpointContextSupportedConfigs"]["browseEndpointContextMusicConfig"]
            ["pageType"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };
    let mut artists: Vec<Value> = runs
        .iter()
        .filter(|r| matches!(page_type(r).as_str(), "MUSIC_PAGE_TYPE_ARTIST" | "MUSIC_PAGE_TYPE_USER_CHANNEL"))
        .map(|r| json!({ "id": r["navigationEndpoint"]["browseEndpoint"]["browseId"], "name": r["text"] }))
        .collect();
    if artists.is_empty() {
        let name = subtitle.split(" • ").find(|s| !matches!(*s, "Song" | "Video" | "Single")).unwrap_or_default();
        if !name.is_empty() {
            artists.push(json!({ "id": null, "name": name }));
        }
    }
    let album = runs
        .iter()
        .find(|r| page_type(r) == "MUSIC_PAGE_TYPE_ALBUM")
        .map(|r| json!({ "id": r["navigationEndpoint"]["browseEndpoint"]["browseId"], "name": r["text"] }));
    let track = serde_json::from_value::<TrackItem>(json!({
        "id": id,
        "name": title,
        "duration": null,
        "cover": if thumbs.is_array() { thumbs.clone() } else { json!([]) },
        "artists": artists,
        "artist_id": artists.first().map(|a| a["id"].clone()),
        "album": album,
        "view_count": null,
        "track_type": "track",
        "track_nr": null,
        "by_va": false,
    }))
    .map_err(|e| log::debug!("home: could not build track {id}: {e}"))
    .ok();
    HomeItem { kind: "track".into(), id: id.to_string(), title, subtitle, thumbnails: thumbs, track }
}
