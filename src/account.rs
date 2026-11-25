//! Account actions rustypipe doesn't wrap: like/dislike and adding to
//! playlists. These use the same YouTube Music API endpoints as the website,
//! sent signed in through rustypipe's raw request.

use rustypipe::client::{ClientType, RustyPipe, RustyPipeQuery};
use serde::Serialize;
use serde_json::{json, Value};

fn signed(rp: &RustyPipe) -> Result<RustyPipeQuery, String> {
    if !crate::auth::has_session(rp) {
        return Err("Sign in to use this".into());
    }
    Ok(rp.query().authenticated())
}

async fn call(q: &RustyPipeQuery, endpoint: &str, body: Value) -> Result<Value, String> {
    let text = q.raw(ClientType::DesktopMusic, endpoint, &body).await.map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| format!("unexpected response: {e}"))
}

/// First value stored under `key` anywhere in `v`.
fn find<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Object(m) => m.get(key).or_else(|| m.values().find_map(|x| find(x, key))),
        Value::Array(a) => a.iter().find_map(|x| find(x, key)),
        _ => None,
    }
}

fn find_all<'a>(v: &'a Value, key: &str, out: &mut Vec<&'a Value>) {
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                if k == key {
                    out.push(x);
                }
                find_all(x, key, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| find_all(x, key, out)),
        _ => {}
    }
}

/// "like", "dislike" or "none" for a video.
pub async fn rating(rp: &RustyPipe, id: &str) -> Result<String, String> {
    let q = signed(rp)?;
    let next = call(&q, "next", json!({ "videoId": id })).await?;
    Ok(match find(&next, "likeStatus").and_then(Value::as_str) {
        Some("LIKE") => "like",
        Some("DISLIKE") => "dislike",
        _ => "none",
    }
    .into())
}

/// Sets the rating of a video: "like", "dislike" or "none".
pub async fn rate(rp: &RustyPipe, id: &str, rating: &str) -> Result<(), String> {
    let q = signed(rp)?;
    let endpoint = match rating {
        "like" => "like/like",
        "dislike" => "like/dislike",
        "none" => "like/removelike",
        other => return Err(format!("unknown rating {other}")),
    };
    call(&q, endpoint, json!({ "target": { "videoId": id } })).await?;
    Ok(())
}

#[derive(Serialize)]
pub struct PlaylistOption {
    pub id: String,
    pub title: String,
    /// The video is already in this playlist.
    pub contains: bool,
}

/// The playlists you can edit, i.e. the "Save to playlist" list, with whether
/// each already contains the video. Asked as the youtube.com client: only its
/// answer says which playlists contain the video (YouTube Music's doesn't).
/// Liked music / Watch later are left out (the Like button covers likes).
pub async fn playlist_options(rp: &RustyPipe, id: &str) -> Result<Vec<PlaylistOption>, String> {
    let q = signed(rp)?;
    let text = q
        .raw(ClientType::Desktop, "playlist/get_add_to_playlist", &json!({ "videoIds": [id] }))
        .await
        .map_err(|e| e.to_string())?;
    let resp: Value = serde_json::from_str(&text).map_err(|e| format!("unexpected response: {e}"))?;
    let mut opts = Vec::new();
    find_all(&resp, "playlistAddToOptionRenderer", &mut opts);
    Ok(opts
        .into_iter()
        .filter_map(|o| {
            let id = o["playlistId"].as_str()?.to_string();
            let title = o["title"]["runs"][0]["text"].as_str().or(o["title"]["simpleText"].as_str())?.to_string();
            let contains = o["containsSelectedVideos"].as_str() == Some("ALL");
            (!matches!(id.as_str(), "LM" | "WL")).then_some(PlaylistOption { id, title, contains })
        })
        .collect())
}

/// Removes a video from one of your playlists.
pub async fn remove_from_playlist(rp: &RustyPipe, playlist_id: &str, video_id: &str) -> Result<(), String> {
    let q = signed(rp)?;
    let resp = call(
        &q,
        "browse/edit_playlist",
        json!({
            "playlistId": playlist_id,
            "actions": [{ "action": "ACTION_REMOVE_VIDEO_BY_VIDEO_ID", "removedVideoId": video_id }],
        }),
    )
    .await?;
    match resp["status"].as_str() {
        Some("STATUS_SUCCEEDED") => Ok(()),
        other => Err(format!("YouTube Music answered {}", other.unwrap_or("nothing"))),
    }
}

/// Adds a video to a playlist. Returns false if it was already in it.
pub async fn add_to_playlist(rp: &RustyPipe, playlist_id: &str, video_id: &str) -> Result<bool, String> {
    let q = signed(rp)?;
    let resp = call(
        &q,
        "browse/edit_playlist",
        json!({
            "playlistId": playlist_id,
            "actions": [{ "action": "ACTION_ADD_VIDEO", "addedVideoId": video_id, "dedupeOption": "DEDUPE_OPTION_SKIP" }],
        }),
    )
    .await?;
    match resp["status"].as_str() {
        Some("STATUS_SUCCEEDED") => Ok(true),
        // With DEDUPE_OPTION_SKIP a duplicate is reported as a failure.
        Some("STATUS_FAILED") => Ok(false),
        other => Err(format!("YouTube Music answered {}", other.unwrap_or("nothing"))),
    }
}
