//! YouTube Music home feed (`FEmusic_home`), which rustypipe doesn't wrap.
//!
//! When signed in, this is the personalised home: pinned items ("Speed
//! dial"), Quick picks, Listen again, Mixed for you, recommendations, ...
//! The response is walked as untyped JSON; anything unrecognised is skipped
//! (and logged at debug level) rather than failing the whole page.

use rustypipe::client::{ClientType, RustyPipe};
use rustypipe::model::TrackItem;
use serde::Serialize;
use serde_json::{json, Value};

/// Pages of the home feed to load (YouTube serves ~3–6 sections per page).
const MAX_PAGES: usize = 4;

#[derive(Debug, Serialize)]
pub struct HomeSection {
    pub title: String,
    /// "tracks" (song list) or "cards" (playlists, albums, mixes, ...)
    pub layout: &'static str,
    pub items: Vec<HomeItem>,
}

#[derive(Debug, Serialize)]
pub struct HomeItem {
    /// track | playlist | album | artist | mix
    pub kind: &'static str,
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub thumbnails: Value,
    /// Present for kind == "track" (playable as-is).
    pub track: Option<TrackItem>,
}

pub async fn load(rp: &RustyPipe) -> Result<Vec<HomeSection>, String> {
    // Raw requests are only signed when asked; use the account if there is one.
    let q = rp.query();
    let q = if q.auth_enabled(ClientType::DesktopMusic) { q.authenticated() } else { q };
    let mut sections = Vec::new();
    let first: Value = serde_json::from_str(
        &q.raw(ClientType::DesktopMusic, "browse", &json!({ "browseId": "FEmusic_home" }))
            .await
            .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;

    let list = &first["contents"]["singleColumnBrowseResultsRenderer"]["tabs"][0]["tabRenderer"]["content"]["sectionListRenderer"];
    parse_sections(&list["contents"], &mut sections);
    let mut token = continuation(list);

    for _ in 1..MAX_PAGES {
        let Some(t) = token.take() else { break };
        let page: Value = match q.raw(ClientType::DesktopMusic, "browse", &json!({ "continuation": t })).await {
            Ok(txt) => serde_json::from_str(&txt).map_err(|e| e.to_string())?,
            Err(e) => {
                log::warn!("home continuation failed: {e}");
                break;
            }
        };
        let cont = &page["continuationContents"]["sectionListContinuation"];
        parse_sections(&cont["contents"], &mut sections);
        token = continuation(cont);
    }
    log::debug!(
        "home: {}",
        sections.iter().map(|s| format!("{} ({})", s.title, s.items.len())).collect::<Vec<_>>().join(", ")
    );
    Ok(sections)
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
                log::debug!("home: skipping section type {other}");
                continue;
            }
        };
        let items: Vec<HomeItem> = items.as_array().into_iter().flatten().filter_map(parse_item).collect();
        if items.is_empty() {
            continue;
        }
        let layout = if items.iter().all(|i| i.kind == "track") && items.len() > 4 { "tracks" } else { "cards" };
        out.push(HomeSection { title, layout, items });
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
                log::debug!("home: skipping browse page type {other}");
                return None;
            }
        };
        return Some(HomeItem { kind, id: id.to_string(), title, subtitle, thumbnails: thumbs, track: None });
    }
    let watch = ep.get("watchEndpoint").or_else(|| ep.get("watchPlaylistEndpoint"))?;
    let playlist = watch["playlistId"].as_str();
    match (watch["videoId"].as_str(), playlist) {
        // Mixes / radios are generated playlists (RD…), played as a radio.
        (_, Some(pl)) if pl.starts_with("RD") => {
            Some(HomeItem { kind: "mix", id: pl.to_string(), title, subtitle, thumbnails: thumbs, track: None })
        }
        (Some(vid), _) => Some(track_item(vid, title, subtitle, thumbs, &runs)),
        (None, Some(pl)) => Some(HomeItem { kind: "playlist", id: pl.to_string(), title, subtitle, thumbnails: thumbs, track: None }),
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
    HomeItem { kind: "track", id: id.to_string(), title, subtitle, thumbnails: thumbs, track }
}
