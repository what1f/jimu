use serde_json::Value;

use super::{items, web_url};
use crate::web::web2md::markdown::{escape, image, link};

pub(super) fn post(item: &Value, fallback_type: &str) -> Option<String> {
    let kind = item["type"].as_str().unwrap_or(fallback_type);
    let alt = item["altText"]
        .as_str()
        .or_else(|| item["alt_text"].as_str())
        .unwrap_or_default();
    let url = item["url"].as_str().and_then(web_url);
    if matches!(kind, "video" | "gif") {
        let poster = item["thumbnail_url"].as_str().and_then(web_url);
        let video = best_mp4(
            items(&item["formats"])
                .iter()
                .chain(items(&item["variants"])),
        )
        .or(url);
        linked_poster(poster, video, alt)
    } else {
        url.map(|url| image(alt, &url))
    }
}

pub(super) fn article(info: &Value, alt: &str) -> Option<String> {
    let alt = info["ext_alt_text"].as_str().unwrap_or(alt);
    if info["__typename"] == "ApiImage"
        || (info["original_img_url"].is_string()
            && info["variants"].is_null()
            && info["video_info"].is_null())
    {
        let url = info["original_img_url"]
            .as_str()
            .or_else(|| info["media_url_https"].as_str())
            .and_then(web_url)?;
        return Some(image(alt, &url));
    }
    let poster = info["preview_image"]["original_img_url"]
        .as_str()
        .or_else(|| info["media_url_https"].as_str())
        .or_else(|| info["original_img_url"].as_str())
        .and_then(web_url);
    let video = best_mp4(
        items(&info["variants"])
            .iter()
            .chain(items(&info["video_info"]["variants"])),
    );
    linked_poster(poster, video, alt)
}

fn linked_poster(poster: Option<String>, video: Option<String>, alt: &str) -> Option<String> {
    match (poster, video) {
        (Some(poster), Some(video)) => Some(link(&image(alt, &poster), &video)),
        (Some(poster), None) => Some(image(alt, &poster)),
        (None, Some(video)) => Some(link(
            &escape(if alt.is_empty() { "Video" } else { alt }),
            &video,
        )),
        _ => None,
    }
}

fn best_mp4<'a>(variants: impl Iterator<Item = &'a Value>) -> Option<String> {
    variants
        .filter_map(|variant| {
            let url = variant["url"].as_str()?;
            let kind = variant["content_type"]
                .as_str()
                .or_else(|| variant["container"].as_str())
                .unwrap_or_default();
            if url.contains(".m3u8")
                || (!kind.is_empty() && !kind.to_ascii_lowercase().contains("mp4"))
            {
                return None;
            }
            let rate = variant["bit_rate"]
                .as_u64()
                .or_else(|| variant["bitrate"].as_u64())
                .unwrap_or_default();
            Some((rate, web_url(url)?))
        })
        .max_by_key(|(rate, _)| *rate)
        .map(|(_, url)| url)
}
