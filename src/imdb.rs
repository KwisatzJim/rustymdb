use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize)]
pub struct SearchResponse {
    pub d: Vec<SearchTitle>,
}

#[derive(Deserialize, Serialize, Default, Debug)]
pub struct SearchTitle {
    pub id: String,
    #[serde(rename = "l")]
    pub title: String,
    #[serde(rename = "s", default)]
    pub cast: String,
    #[serde(rename = "tl", default)]
    pub description: String,
}

pub fn valid_title_id(id: &str) -> bool {
    id.strip_prefix("tt").is_some_and(|digits| {
        (7..=12).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit())
    })
}

#[derive(Deserialize, Serialize, Default)]
pub struct TitleDetails {
    pub name: String,
    pub plot: String,
    pub released: String,
    pub genres: String,
    pub rating: String,
    pub directors: String,
    pub actors: String,
}

fn decode(text: &str) -> String {
    html_escape::decode_html_entities(text).into_owned()
}

fn parse_details(
    html: &str,
    id: &str,
) -> Result<TitleDetails, Box<dyn std::error::Error + Send + Sync>> {
    let document = scraper::Html::parse_document(html);
    let selector = scraper::Selector::parse("script[type='application/ld+json']").unwrap();
    for script in document.select(&selector) {
        let Ok(value) =
            serde_json::from_str::<serde_json::Value>(&script.text().collect::<String>())
        else {
            continue;
        };
        let mut candidates = vec![&value];
        if let Some(items) = value.as_array() {
            candidates.extend(items);
        }
        if let Some(items) = value.get("@graph").and_then(|v| v.as_array()) {
            candidates.extend(items);
        }
        for item in candidates {
            let url = item["url"].as_str().unwrap_or_default();
            if !url.split('/').any(|part| part == id) {
                continue;
            }
            let Some(name) = item["name"].as_str() else {
                continue;
            };
            let text = |key: &str| decode(item[key].as_str().unwrap_or_default());
            let names = |key: &str| -> String {
                let entries = &item[key];
                let values: Vec<&serde_json::Value> = match entries.as_array() {
                    Some(items) => items.iter().collect(),
                    None => vec![entries],
                };
                values
                    .iter()
                    .filter_map(|v| v["name"].as_str())
                    .map(decode)
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let genres = match item["genre"].as_array() {
                Some(items) => items
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(decode)
                    .collect::<Vec<_>>()
                    .join(", "),
                None => text("genre"),
            };
            let rating = item["aggregateRating"]["ratingValue"]
                .as_number()
                .map(ToString::to_string)
                .or_else(|| {
                    item["aggregateRating"]["ratingValue"]
                        .as_str()
                        .map(str::to_owned)
                })
                .unwrap_or_default();
            return Ok(TitleDetails {
                name: decode(name),
                plot: text("description"),
                released: text("datePublished"),
                genres,
                rating,
                directors: names("director"),
                actors: names("actor"),
            });
        }
    }
    Err("IMDb title structured data unavailable".into())
}

pub async fn title_details(
    client: &reqwest::Client,
    id: &str,
) -> Result<TitleDetails, Box<dyn std::error::Error + Send + Sync>> {
    if !valid_title_id(id) {
        return Err("Invalid IMDb title ID".into());
    }
    let mut response = client
        .get(format!("https://www.imdb.com/title/{id}/"))
        .header("Accept-Language", "en-US,en;q=0.9")
        .send()
        .await?
        .error_for_status()?;
    if response.status() == reqwest::StatusCode::ACCEPTED
        || response.headers().contains_key("x-amzn-waf-action")
    {
        return Err("IMDb returned an automated-access challenge".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > 8 * 1024 * 1024 {
            return Err("IMDb title response too large".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    parse_details(std::str::from_utf8(&bytes)?, id)
}

pub async fn search(
    client: &reqwest::Client,
    query: &str,
) -> Result<SearchResponse, Box<dyn std::error::Error + Send + Sync>> {
    let normalized = query.to_lowercase();
    let first = normalized.chars().next().ok_or("Empty search")?;
    let mut url = reqwest::Url::parse("https://v3.sg.media-imdb.com/")?;
    url.path_segments_mut()
        .map_err(|_| "Invalid base URL")?
        .extend([
            "suggestion",
            &first.to_string(),
            &format!("{normalized}.json"),
        ]);
    let mut response = client.get(url).send().await?.error_for_status()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > 2 * 1024 * 1024 {
            return Err("IMDb response too large".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    let mut results: SearchResponse = serde_json::from_slice(&bytes)?;
    results.d.retain(|item| {
        item.id
            .strip_prefix("tt")
            .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
    });
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_fields_can_be_absent() {
        let results: SearchResponse =
            serde_json::from_str(r#"{"d":[{"id":"tt1375666","l":"Inception"}]}"#).unwrap();
        assert_eq!(results.d[0].title, "Inception");
        assert!(results.d[0].cast.is_empty());
    }

    #[test]
    fn title_ids_reject_paths_and_non_digits() {
        assert!(valid_title_id("tt1375666"));
        assert!(!valid_title_id("tt1375666/../"));
        assert!(!valid_title_id("nm1234567"));
        assert!(!valid_title_id("tt1"));
    }

    #[test]
    fn structured_details_match_requested_title_and_decode_entities() {
        let html = r#"<script type="application/ld+json">{"@type":"Movie","url":"https://www.imdb.com/title/tt1375666/","name":"A &amp; B","description":"A &quot;dream&quot;.","genre":["Action","Sci-Fi"],"director":[{"name":"A Director"}],"actor":[{"name":"An Actor"}],"aggregateRating":{"ratingValue":8.8}}</script>"#;
        let details = parse_details(html, "tt1375666").unwrap();
        assert_eq!(details.name, "A & B");
        assert_eq!(details.plot, "A \"dream\".");
        assert_eq!(details.rating, "8.8");
        assert_eq!(details.genres, "Action, Sci-Fi");
        assert!(parse_details(html, "tt9999999").is_err());
        assert!(parse_details("<html>Challenge</html>", "tt1375666").is_err());
    }
}
