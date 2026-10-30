mod backup;
mod cast;
mod catalog;
mod credits;
mod database;
mod episodes;
mod imdb;
mod maintenance;
mod people;
mod posters;
mod ratings;
mod retention;
mod titles;
mod tmdb;
mod watchlist;

use askama::Template;
use axum::extract::{Form, Path, Query, State};
use axum::{
    Router,
    http::StatusCode,
    response::{Html, Redirect},
    routing::{get, post},
};
use serde::Deserialize;
use std::{env, path::PathBuf};

#[derive(Clone)]
struct AppState {
    client: reqwest::Client,
    poster_client: reqwest::Client,
    photo_limit: std::sync::Arc<tokio::sync::Semaphore>,
    database: PathBuf,
    form_token: String,
    maintenance: std::sync::Arc<maintenance::Manager>,
    tmdb: std::sync::Arc<tmdb::Service>,
}

#[derive(Template)]
#[template(path = "person.html")]
struct PersonPage {
    id: String,
    name: String,
    imdb_name: bool,
    person: Option<tmdb::Person>,
    notice: String,
}
async fn person_page(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Html<String>, StatusCode> {
    use rusqlite::OptionalExtension;
    if !credits::person_id(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    let path = state.database.clone();
    let key = id.clone();
    let name = tokio::task::spawn_blocking(move || -> rusqlite::Result<Option<String>> {
        rusqlite::Connection::open(path)?
            .query_row(
                "SELECT primary_name FROM imdb_people WHERE person_id=?1",
                [key],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .map(Option::flatten)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let (person, notice) = match state.tmdb.person(&id).await {
        Ok(Some(person)) => (Some(person), String::new()),
        Ok(None) => (
            None,
            if state.tmdb.method().is_empty() {
                "Configure TMDB in Options to enable biographies and filmographies."
            } else {
                "No matching person details were found on TMDB."
            }
            .into(),
        ),
        Err(error) => (None, error),
    };
    if name.is_none() && person.is_none() {
        return Err(StatusCode::NOT_FOUND);
    }
    let imdb_name = name.is_some();
    let name = name.unwrap_or_else(|| person.as_ref().map(|p| p.name.clone()).unwrap_or_default());
    PersonPage {
        id,
        name,
        imdb_name,
        person,
        notice,
    }
    .render()
    .map(Html)
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[derive(Template)]
#[template(path = "title_link.html")]
struct TitleLinkPage {
    heading: String,
    source_url: String,
    notice: String,
    retry_url: String,
}
async fn tmdb_title_link(
    State(state): State<AppState>,
    Path((media, id)): Path<(String, u64)>,
) -> Result<axum::response::Response, StatusCode> {
    use axum::response::IntoResponse;
    if !tmdb::valid_media_id(&media, id) {
        return Err(StatusCode::NOT_FOUND);
    }
    let (status, notice) = match state.tmdb.resolve_title(&media, id).await {
        Ok(Some(imdb_id)) => {
            return Ok(Redirect::temporary(&format!("/title/{imdb_id}")).into_response());
        }
        Ok(None) => (
            StatusCode::OK,
            if state.tmdb.method().is_empty() {
                "Configure TMDB in Options to look up the IMDb link for this title."
            } else {
                "TMDB has no IMDb ID for this title, so a local match is unavailable."
            }
            .into(),
        ),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error),
    };
    let html = TitleLinkPage {
        heading: "Local title unavailable".into(),
        source_url: format!("https://www.themoviedb.org/{media}/{id}"),
        notice,
        retry_url: format!("/tmdb-title/{media}/{id}"),
    }
    .render()
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok((status, [("cache-control", "no-store")], Html(html)).into_response())
}

async fn tmdb_person_link(
    State(state): State<AppState>,
    Path(id): Path<u64>,
) -> Result<axum::response::Response, StatusCode> {
    use axum::response::IntoResponse;
    if !(1..=i32::MAX as u64).contains(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    let (status, notice) = match state.tmdb.resolve_person(id).await {
        Ok(Some(imdb_id)) => {
            return Ok(Redirect::temporary(&format!("/person/{imdb_id}")).into_response());
        }
        Ok(None) => (
            StatusCode::OK,
            if state.tmdb.method().is_empty() {
                "Configure TMDB in Options to look up the IMDb link for this person."
            } else {
                "TMDB has no IMDb ID for this person, so a local match is unavailable."
            }
            .into(),
        ),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error),
    };
    let html = TitleLinkPage {
        heading: "Local person unavailable".into(),
        source_url: format!("https://www.themoviedb.org/person/{id}"),
        notice,
        retry_url: format!("/tmdb-person/{id}"),
    }
    .render()
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok((status, [("cache-control", "no-store")], Html(html)).into_response())
}

#[derive(Template)]
#[template(path = "collection.html")]
struct CollectionPage {
    id: u64,
    collection: Option<tmdb::Collection>,
    notice: String,
}
async fn collection_page(
    State(state): State<AppState>,
    Path(id): Path<u64>,
) -> Result<axum::response::Response, StatusCode> {
    use axum::response::IntoResponse;
    if !tmdb::valid_media_id("movie", id) {
        return Err(StatusCode::NOT_FOUND);
    }
    let (status, collection, notice) = match state.tmdb.collection(id).await {
        Ok(Some(collection)) => (StatusCode::OK, Some(collection), String::new()),
        Ok(None) => (
            StatusCode::OK,
            None,
            "Configure TMDB in Options to browse this collection.".into(),
        ),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, None, error),
    };
    let html = CollectionPage {
        id,
        collection,
        notice,
    }
    .render()
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok((status, [("cache-control", "no-store")], Html(html)).into_response())
}

async fn person_photo(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<([(axum::http::header::HeaderName, String); 3], Vec<u8>), StatusCode> {
    use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
    if !credits::person_id(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    let photo = if state.tmdb.method().is_empty() {
        posters::person_placeholder()
    } else {
        let path = state.database.clone();
        let key = format!("person:{id}");
        let cache_key = key.clone();
        let cached = tokio::task::spawn_blocking(move || posters::cached(&path, &cache_key))
            .await
            .ok()
            .and_then(Result::ok)
            .flatten();
        if let Some(photo) = cached {
            photo
        } else {
            // Limit concurrent provider lookups when a People results page loads thumbnails.
            let _permit = state
                .photo_limit
                .acquire()
                .await
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
            let photo = match state.tmdb.person(&id).await {
                Ok(Some(person)) if !person.profile_path.is_empty() => {
                    posters::fetch_profile(&state.poster_client, &person.profile_path)
                        .await
                        .unwrap_or_else(|_| posters::person_placeholder())
                }
                _ => posters::person_placeholder(),
            };
            let path = state.database.clone();
            tokio::task::spawn_blocking(move || {
                let _ = posters::store(&path, &key, &photo);
                photo
            })
            .await
            .unwrap_or_else(|_| posters::person_placeholder())
        }
    };
    Ok((
        [
            (CONTENT_TYPE, photo.content_type),
            (CACHE_CONTROL, format!("public, max-age={}", photo.ttl)),
            (X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
        ],
        photo.bytes,
    ))
}

async fn poster(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<([(axum::http::header::HeaderName, String); 3], Vec<u8>), StatusCode> {
    use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
    if !imdb::valid_title_id(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    let path = state.database.clone();
    let cache_id = id.clone();
    let cached = match tokio::task::spawn_blocking(move || posters::cached(&path, &cache_id)).await
    {
        Ok(Ok(poster)) => poster,
        error => {
            eprintln!("Poster cache unavailable: {error:?}");
            None
        }
    };
    let poster = match cached {
        Some(poster) => poster,
        None => {
            let poster = match posters::fetch(&state.poster_client, &id).await {
                Ok(poster) => poster,
                Err(error) => {
                    eprintln!("Poster unavailable for {id}: {error}");
                    match state.tmdb.supplement(&id, None).await {
                        Ok(Some(details)) if !details.poster_path.is_empty() => {
                            posters::fetch_tmdb(&state.poster_client, &details.poster_path)
                                .await
                                .unwrap_or_else(|_| posters::placeholder())
                        }
                        _ => posters::placeholder(),
                    }
                }
            };
            let path = state.database.clone();
            // Return the image even when a dataset import temporarily blocks cache writes.
            match tokio::task::spawn_blocking(move || {
                if let Err(error) = posters::store(&path, &id, &poster) {
                    eprintln!("Poster cache write failed: {error}");
                }
                poster
            })
            .await
            {
                Ok(poster) => poster,
                Err(_) => posters::placeholder(),
            }
        }
    };
    Ok((
        [
            (CONTENT_TYPE, poster.content_type),
            (
                CACHE_CONTROL,
                format!("public, max-age={}", poster.ttl.min(86400)),
            ),
            (X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
        ],
        poster.bytes,
    ))
}

#[derive(Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    kind: catalog::TitleFilter,
    #[serde(default = "first_page")]
    page: std::num::NonZeroU32,
}

fn first_page() -> std::num::NonZeroU32 {
    std::num::NonZeroU32::MIN
}

#[derive(Deserialize)]
struct TitleQuery {
    #[serde(default)]
    season: String,
    #[serde(default = "first_page")]
    page: std::num::NonZeroU32,
}

fn search_url(query: &str, kind: catalog::TitleFilter, page: u32) -> String {
    let mut url = reqwest::Url::parse("http://localhost/search").expect("Static URL is valid");
    url.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("kind", kind.as_str())
        .append_pair("page", &page.to_string());
    format!("/search?{}", url.query().unwrap_or_default())
}

#[derive(Template)]
#[template(path = "search.html")]
struct SearchPage {
    query: String,
    results: Vec<imdb::SearchTitle>,
    message: String,
    source: &'static str,
    kind: catalog::TitleFilter,
    number: u32,
    previous_url: String,
    next_url: String,
}

#[derive(Template)]
#[template(path = "home.html")]
struct Home;

struct MaintenanceView {
    form_token: String,
    latest: Option<maintenance::Job>,
    running: bool,
    output: String,
    backup_directory: String,
}

#[derive(Template)]
#[template(path = "maintenance.html")]
struct MaintenanceFragment {
    maintenance: MaintenanceView,
}

#[derive(Template)]
#[template(path = "maintenance_page.html")]
struct MaintenancePage {
    maintenance: MaintenanceView,
}

async fn maintenance_page(State(state): State<AppState>) -> Result<Html<String>, StatusCode> {
    let maintenance = maintenance_view(state).await?;
    MaintenancePage { maintenance }
        .render()
        .map(Html)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[derive(Template)]
#[template(path = "options.html")]
struct OptionsPage {
    retention: retention::View,
    schedules: Vec<maintenance::Schedule>,
    form_token: String,
    method: String,
    message: String,
    error: String,
}
#[derive(Default, Deserialize)]
struct OptionsQuery {
    result: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum OptionsAction {
    Save,
    Test,
    Remove,
}
#[derive(Deserialize)]
struct OptionsForm {
    token: String,
    action: OptionsAction,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    read_token: String,
}
async fn options_page(
    State(state): State<AppState>,
    Query(input): Query<OptionsQuery>,
) -> Result<([(&'static str, &'static str); 1], Html<String>), StatusCode> {
    let message = match input.result.as_deref() {
        Some("saved") => {
            "TMDB credentials saved and verified. Supplemental information is enabled."
        }
        Some("retention") => {
            "Backup retention saved. No files were removed. Cleanup runs only after a new verified scheduled backup."
        }
        Some("schedules") => {
            "Maintenance schedules saved. Enabling or changing a schedule starts its interval from now."
        }
        Some("tested") => "The saved TMDB credentials are working.",
        Some("removed") => "TMDB credentials removed. No further TMDB requests will be made.",
        _ => "",
    };
    let schedules = load_schedules(state.maintenance.clone()).await?;
    let retention = load_retention(state.database.clone()).await?;
    let html = OptionsPage {
        retention,
        schedules,
        form_token: state.form_token,
        method: state.tmdb.method(),
        message: message.into(),
        error: String::new(),
    }
    .render()
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(([("cache-control", "no-store")], Html(html)))
}
async fn options_update(
    State(state): State<AppState>,
    Form(input): Form<OptionsForm>,
) -> Result<axum::response::Response, (StatusCode, &'static str)> {
    use axum::response::IntoResponse;
    if input.token != state.form_token {
        return Err((
            StatusCode::FORBIDDEN,
            "This form expired. Reload Options and try again.",
        ));
    }
    let (result, label) = match input.action {
        OptionsAction::Save => (
            state.tmdb.save(&input.api_key, &input.read_token).await,
            "saved",
        ),
        OptionsAction::Test => (state.tmdb.test().await, "tested"),
        OptionsAction::Remove => (state.tmdb.remove(), "removed"),
    };
    match result {
        Ok(()) => Ok(Redirect::to(&format!("/options?result={label}")).into_response()),
        Err(error) => {
            let schedules = load_schedules(state.maintenance.clone())
                .await
                .map_err(|_| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Schedules could not be displayed.",
                    )
                })?;
            let retention = load_retention(state.database.clone()).await.map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Retention settings could not be displayed.",
                )
            })?;
            let html = OptionsPage {
                retention,
                schedules,
                form_token: state.form_token,
                method: state.tmdb.method(),
                message: String::new(),
                error,
            }
            .render()
            .map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Options could not be displayed.",
                )
            })?;
            Ok((
                StatusCode::BAD_REQUEST,
                [("cache-control", "no-store")],
                Html(html),
            )
                .into_response())
        }
    }
}
async fn load_schedules(
    manager: std::sync::Arc<maintenance::Manager>,
) -> Result<Vec<maintenance::Schedule>, StatusCode> {
    tokio::task::spawn_blocking(move || manager.schedules())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}
#[derive(Deserialize)]
struct ScheduleForm {
    token: String,
    backup_days: u32,
    refresh_days: u32,
}
async fn schedules_update(
    State(state): State<AppState>,
    Form(input): Form<ScheduleForm>,
) -> Result<Redirect, (StatusCode, &'static str)> {
    if input.token != state.form_token {
        return Err((
            StatusCode::FORBIDDEN,
            "This form expired. Reload Options and try again.",
        ));
    }
    if ![0, 1, 7, 30].contains(&input.backup_days) || ![0, 1, 7, 30].contains(&input.refresh_days) {
        return Err((
            StatusCode::BAD_REQUEST,
            "Choose a supported schedule interval.",
        ));
    }
    tokio::task::spawn_blocking(move || {
        state
            .maintenance
            .save_schedules(input.backup_days, input.refresh_days)
    })
    .await
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Schedules could not be saved.",
        )
    })?
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Schedules could not be saved.",
        )
    })?;
    Ok(Redirect::to("/options?result=schedules#schedules"))
}

async fn load_retention(path: PathBuf) -> Result<retention::View, StatusCode> {
    tokio::task::spawn_blocking(move || retention::view(&path))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}
#[derive(Deserialize)]
struct RetentionForm {
    token: String,
    keep: u32,
}
async fn retention_update(
    State(state): State<AppState>,
    Form(input): Form<RetentionForm>,
) -> Result<Redirect, (StatusCode, &'static str)> {
    if input.token != state.form_token {
        return Err((
            StatusCode::FORBIDDEN,
            "This form expired. Reload Options and try again.",
        ));
    }
    if ![0, 3, 5, 10, 20].contains(&input.keep) {
        return Err((
            StatusCode::BAD_REQUEST,
            "Choose a supported backup retention limit.",
        ));
    }
    tokio::task::spawn_blocking(move || retention::save(&state.database, input.keep))
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Retention settings could not be saved.",
            )
        })?
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Retention settings could not be saved.",
            )
        })?;
    Ok(Redirect::to("/options?result=retention#retention"))
}

async fn tmdb_logo() -> ([(&'static str, &'static str); 1], &'static [u8]) {
    (
        [("content-type", "image/svg+xml")],
        include_bytes!("../static/tmdb-logo.svg"),
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum MaintenanceAction {
    Refresh,
    Backup,
}

#[derive(Deserialize)]
struct MaintenanceForm {
    token: String,
    action: MaintenanceAction,
    #[serde(default)]
    dataset: maintenance::Dataset,
}

async fn maintenance_view(state: AppState) -> Result<MaintenanceView, StatusCode> {
    tokio::task::spawn_blocking(
        move || -> Result<_, Box<dyn std::error::Error + Send + Sync>> {
            let latest = state.maintenance.latest()?;
            let running = latest.as_ref().is_some_and(|job| job.state == "running");
            let output = match latest.as_ref() {
                Some(job) => state.maintenance.output(job.id)?,
                None => String::new(),
            };
            Ok(MaintenanceView {
                form_token: state.form_token,
                backup_directory: state.maintenance.backup_directory().display().to_string(),
                latest,
                running,
                output,
            })
        },
    )
    .await
    .map_err(|error| {
        eprintln!("Maintenance status task failed: {error}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?
    .map_err(|error| {
        eprintln!("Maintenance status failed: {error}");
        StatusCode::SERVICE_UNAVAILABLE
    })
}

async fn maintenance_status(
    State(state): State<AppState>,
) -> Result<([(&'static str, &'static str); 1], Html<String>), StatusCode> {
    let maintenance = maintenance_view(state).await?;
    let html = MaintenanceFragment { maintenance }
        .render()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(([("cache-control", "no-store")], Html(html)))
}

async fn maintenance_start(
    State(state): State<AppState>,
    Form(input): Form<MaintenanceForm>,
) -> Result<Redirect, (StatusCode, &'static str)> {
    if input.token != state.form_token {
        return Err((
            StatusCode::FORBIDDEN,
            "This form expired. Reload the homepage and try again.",
        ));
    }
    let request = match input.action {
        MaintenanceAction::Refresh => maintenance::Request::Refresh(input.dataset),
        MaintenanceAction::Backup => maintenance::Request::Backup,
    };
    let manager = state.maintenance.clone();
    let task=tokio::task::spawn_blocking(move||manager.reserve(request)).await
        .map_err(|error|{eprintln!("Maintenance queue failed: {error}");(StatusCode::INTERNAL_SERVER_ERROR,"Unable to queue this maintenance job.")})?
        .map_err(|error|match error {
            maintenance::StartError::Busy=>(StatusCode::CONFLICT,"Another maintenance job is running. Wait for it to finish and reload the homepage."),
            maintenance::StartError::Database(error)=>{eprintln!("Maintenance queue database failed: {error}");(StatusCode::SERVICE_UNAVAILABLE,"The maintenance queue is temporarily unavailable.")}
        })?;
    tokio::spawn(state.maintenance.run(task));
    Ok(Redirect::to("/maintenance#maintenance"))
}

async fn maintenance_script() -> ([(&'static str, &'static str); 1], &'static str) {
    (
        [("content-type", "text/javascript; charset=utf-8")],
        include_str!("../static/maintenance.js"),
    )
}

#[derive(Template)]
#[template(path = "title.html")]
struct TitlePage {
    id: String,
    summary: imdb::SearchTitle,
    details: imdb::TitleDetails,
    notice: String,
    dataset_rating: Option<database::Rating>,
    dataset_title: Option<database::TitleBasics>,
    dataset_credits: Option<credits::Credits>,
    dataset_cast: Option<cast::Cast>,
    episode_navigation: episodes::Navigation,
    saved: Option<bool>,
    form_token: String,
    return_season: String,
    return_page: u32,
    supplemental: Option<tmdb::Supplement>,
}

#[derive(Template)]
#[template(path = "watchlist.html")]
struct WatchlistPage {
    total: usize,
    filter: WatchlistFilter,
    items: Vec<watchlist::Item>,
    form_token: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum WatchlistAction {
    Add,
    Remove,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum WatchlistDestination {
    #[default]
    Title,
    List,
}

#[derive(Deserialize)]
struct WatchlistForm {
    token: String,
    action: WatchlistAction,
    #[serde(default)]
    destination: WatchlistDestination,
    #[serde(default)]
    season: String,
    #[serde(default = "first_page")]
    page: std::num::NonZeroU32,
}

#[derive(Default, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum WatchlistFilter {
    #[default]
    All,
    Unwatched,
    Watched,
}
impl WatchlistFilter {
    fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Unwatched => "unwatched",
            Self::Watched => "watched",
        }
    }
    fn includes(self, watched: bool) -> bool {
        match self {
            Self::All => true,
            Self::Unwatched => !watched,
            Self::Watched => watched,
        }
    }
}
#[derive(Default, Deserialize)]
struct WatchlistQuery {
    #[serde(default)]
    filter: WatchlistFilter,
}
#[derive(Deserialize)]
struct WatchlistDetailsForm {
    token: String,
    #[serde(default)]
    watched: Option<String>,
    #[serde(default)]
    notes: String,
    #[serde(default)]
    filter: WatchlistFilter,
}
async fn watchlist_details_update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(input): Form<WatchlistDetailsForm>,
) -> Result<Redirect, (StatusCode, &'static str)> {
    if input.token != state.form_token {
        return Err((
            StatusCode::FORBIDDEN,
            "This form expired. Reload the watchlist and try again.",
        ));
    }
    if !imdb::valid_title_id(&id) {
        return Err((StatusCode::NOT_FOUND, "Title not found."));
    }
    if input.notes.chars().count() > 4000 {
        return Err((
            StatusCode::BAD_REQUEST,
            "Notes must be 4,000 characters or fewer. Go back to shorten your note.",
        ));
    }
    if input.watched.as_deref().is_some_and(|v| v != "yes") {
        return Err((StatusCode::BAD_REQUEST, "Invalid watched status."));
    }
    let watched = input.watched.is_some();
    let path = state.database;
    let changed = tokio::task::spawn_blocking(move || {
        watchlist::save_details(&path, &id, watched, &input.notes)
    })
    .await
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Watchlist details could not be saved.",
        )
    })?
    .map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Watchlist details could not be saved.",
        )
    })?;
    if !changed {
        return Err((
            StatusCode::NOT_FOUND,
            "This title is no longer in the watchlist. Reload the page.",
        ));
    }
    Ok(Redirect::to(&format!(
        "/watchlist?filter={}",
        input.filter.as_str()
    )))
}

async fn watchlist_page(
    State(state): State<AppState>,
    Query(input): Query<WatchlistQuery>,
) -> Result<Html<String>, StatusCode> {
    let path = state.database.clone();
    let items = tokio::task::spawn_blocking(move || watchlist::list(&path))
        .await
        .map_err(|error| {
            eprintln!("Watchlist task failed: {error}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .map_err(|error| {
            eprintln!("Watchlist read failed: {error}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let total = items.len();
    let items = items
        .into_iter()
        .filter(|item| input.filter.includes(item.watched))
        .collect();
    WatchlistPage {
        total,
        filter: input.filter,
        items,
        form_token: state.form_token,
    }
    .render()
    .map(Html)
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn watchlist_update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Form(input): Form<WatchlistForm>,
) -> Result<Redirect, (StatusCode, &'static str)> {
    if input.token != state.form_token {
        return Err((
            StatusCode::FORBIDDEN,
            "This form expired. Reload the page and try again.",
        ));
    }
    if !imdb::valid_title_id(&id) {
        return Err((StatusCode::NOT_FOUND, "Title not found."));
    }
    let season = episodes::season_value(&input.season)
        .map_err(|_| (StatusCode::BAD_REQUEST, "Invalid season."))?;
    let path = state.database.clone();
    let title_id = id.clone();
    let add = matches!(input.action, WatchlistAction::Add);
    let changed = tokio::task::spawn_blocking(move || watchlist::update(&path, &title_id, add))
        .await
        .map_err(|error| {
            eprintln!("Watchlist task failed: {error}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "The watchlist could not be updated. Try again shortly.",
            )
        })?
        .map_err(|error| {
            eprintln!("Watchlist update failed: {error}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "The watchlist could not be updated. Try again shortly.",
            )
        })?;
    if !changed {
        return Err((
            StatusCode::NOT_FOUND,
            "IMDb information for this title is unavailable, so it could not be saved.",
        ));
    }
    let destination = match input.destination {
        WatchlistDestination::List => "/watchlist".into(),
        WatchlistDestination::Title => format!("/title/{id}?season={season}&page={}", input.page),
    };
    Ok(Redirect::to(&destination))
}

async fn read_cache<T: serde::de::DeserializeOwned>(state: &AppState, key: String) -> Option<T> {
    let path = state.database.clone();
    match tokio::task::spawn_blocking(move || database::cached(&path, &key)).await {
        Ok(Ok(Some(payload))) => serde_json::from_str(&payload).ok(),
        Ok(Ok(None)) => None,
        error => {
            eprintln!("Cache read failed: {error:?}");
            None
        }
    }
}

async fn write_cache<T: serde::Serialize>(state: &AppState, key: String, value: &T) {
    let Ok(payload) = serde_json::to_string(value) else {
        return;
    };
    let path = state.database.clone();
    match tokio::task::spawn_blocking(move || database::store(&path, &key, &payload)).await {
        Ok(Ok(())) => {}
        error => eprintln!("Cache write failed: {error:?}"),
    }
}

async fn title(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(input): Query<TitleQuery>,
) -> Result<(StatusCode, Html<String>), StatusCode> {
    if !imdb::valid_title_id(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    let season = episodes::season_value(&input.season).map_err(|_| StatusCode::BAD_REQUEST)?;
    let number = input.page.get();
    let path = state.database.clone();
    let title_id = id.clone();
    let dataset_title =
        match tokio::task::spawn_blocking(move || database::title_basics(&path, &title_id)).await {
            Ok(Ok(basics)) => basics,
            error => {
                eprintln!("Title basics lookup failed: {error:?}");
                None
            }
        };
    let summary_key = format!("title-summary:{id}");
    let cached_summary = read_cache::<imdb::SearchTitle>(&state, summary_key.clone()).await;
    let mut status = StatusCode::OK;
    let mut notice = String::new();
    let summary = if let Some(basics) = &dataset_title {
        imdb::SearchTitle {
            id: id.clone(),
            title: basics.primary_title.clone(),
            description: basics.years(),
            cast: cached_summary
                .map(|summary| summary.cast)
                .unwrap_or_default(),
        }
    } else {
        match cached_summary {
            Some(summary) => summary,
            None => match imdb::search(&state.client, &id).await {
                Ok(results) => match results.d.into_iter().find(|item| item.id == id) {
                    Some(summary) => {
                        write_cache(&state, summary_key, &summary).await;
                        summary
                    }
                    None => {
                        status = StatusCode::NOT_FOUND;
                        notice = "IMDb did not return this title.".into();
                        imdb::SearchTitle::default()
                    }
                },
                Err(error) => {
                    eprintln!("IMDb title lookup failed: {error}");
                    status = StatusCode::BAD_GATEWAY;
                    notice = "IMDb title information is unavailable right now. Please try again shortly.".into();
                    imdb::SearchTitle::default()
                }
            },
        }
    };
    let details_key = format!("title-details:{id}");
    let details = if status == StatusCode::OK {
        match read_cache::<imdb::TitleDetails>(&state, details_key.clone()).await {
            Some(details) => details,
            None if dataset_title.is_some() => {
                notice = "Storylines are not included in IMDb’s downloadable datasets.".into();
                imdb::TitleDetails::default()
            }
            None => match imdb::title_details(&state.client, &id).await {
                Ok(details) => {
                    write_cache(&state, details_key, &details).await;
                    details
                }
                Err(error) => {
                    eprintln!("IMDb title details failed for {id}: {error}");
                    notice = "Some title details, including the storyline, are temporarily unavailable from IMDb.".into();
                    imdb::TitleDetails::default()
                }
            },
        }
    } else {
        imdb::TitleDetails::default()
    };
    let path = state.database.clone();
    let rating_id = id.clone();
    let dataset_rating =
        match tokio::task::spawn_blocking(move || database::rating(&path, &rating_id)).await {
            Ok(Ok(rating)) => rating,
            error => {
                eprintln!("Ratings lookup failed: {error:?}");
                None
            }
        };
    let path = state.database.clone();
    let credits_id = id.clone();
    let dataset_credits =
        match tokio::task::spawn_blocking(move || credits::lookup(&path, &credits_id)).await {
            Ok(Ok(credits)) => credits,
            error => {
                eprintln!("Credits lookup failed: {error:?}");
                None
            }
        };
    let path = state.database.clone();
    let cast_id = id.clone();
    let dataset_cast =
        match tokio::task::spawn_blocking(move || cast::lookup(&path, &cast_id)).await {
            Ok(Ok(cast)) => cast,
            error => {
                eprintln!("Cast lookup failed: {error:?}");
                None
            }
        };
    let path = state.database.clone();
    let series_id = id.clone();
    let return_season = season.clone();
    let episode_navigation = match tokio::task::spawn_blocking(move || {
        episodes::lookup(&path, &series_id, &season, number)
    })
    .await
    {
        Ok(Ok(navigation)) => navigation,
        error => {
            eprintln!("Episode lookup failed: {error:?}");
            episodes::Navigation::default()
        }
    };
    let path = state.database.clone();
    let saved_id = id.clone();
    let saved =
        match tokio::task::spawn_blocking(move || watchlist::contains(&path, &saved_id)).await {
            Ok(Ok(saved)) => Some(saved),
            error => {
                eprintln!("Watchlist state unavailable: {error:?}");
                None
            }
        };
    let kind = dataset_title.as_ref().map(|data| data.title_type.as_str());
    let expected = match kind {
        Some("tvSeries" | "tvMiniSeries") => Some("tv"),
        Some("tvEpisode") => Some("episode"),
        Some("movie" | "tvMovie" | "short" | "video" | "tvShort" | "tvSpecial") => Some("movie"),
        _ => None,
    };
    let mut supplemental = if status == StatusCode::OK && kind != Some("videoGame") {
        match state.tmdb.supplement(&id, expected).await {
            Ok(data) => data,
            Err(error) => {
                if details.plot.is_empty() {
                    notice = format!("{error} IMDb information remains available.");
                }
                None
            }
        }
    } else {
        None
    };
    if let Some(data) = supplemental.as_mut() {
        data.preserve_imdb(
            !details.plot.is_empty(),
            !details.released.is_empty(),
            dataset_title
                .as_ref()
                .is_some_and(|t| t.runtime_minutes.is_some()),
            dataset_title
                .as_ref()
                .is_some_and(|t| t.genres.as_ref().is_some_and(|g| !g.is_empty()))
                || !details.genres.is_empty(),
        );
        if !data.overview.is_empty() {
            notice.clear();
        }
    }
    TitlePage {
        id,
        summary,
        details,
        notice,
        dataset_rating,
        dataset_title,
        dataset_credits,
        dataset_cast,
        episode_navigation,
        saved,
        form_token: state.form_token,
        return_season,
        return_page: number,
        supplemental,
    }
    .render()
    .map(|html| (status, Html(html)))
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let arguments: Vec<String> = env::args().skip(1).collect();
    let data_dir = PathBuf::from(env::var("RUSTYMDB_DATA_DIR").unwrap_or_else(|_| "data".into()));
    if arguments.first().is_some_and(|command| command == "backup") {
        if arguments.len() != 2 {
            return Err("Usage: rustymdb backup <new-destination.sqlite3>".into());
        }
        let source = data_dir.join("rustymdb.sqlite3");
        let destination = PathBuf::from(&arguments[1]);
        let output = destination.clone();
        println!("Creating a verified backup at {}", output.display());
        let bytes =
            tokio::task::spawn_blocking(move || backup::create(&source, &destination)).await??;
        println!("Backup complete: {} ({bytes} bytes)", output.display());
        return Ok(());
    }
    if !arguments.is_empty()
        && arguments != ["import-ratings"]
        && arguments != ["import-titles"]
        && arguments != ["index-titles"]
        && arguments != ["index-people"]
        && arguments != ["import-credits"]
        && arguments != ["import-cast"]
        && arguments != ["import-episodes"]
    {
        return Err(
            "Usage: rustymdb [import-ratings | import-titles | index-titles | index-people | import-credits | import-cast | import-episodes | backup <new-destination.sqlite3>]"
                .into(),
        );
    }
    std::fs::create_dir_all(&data_dir)?;
    database::initialize(&data_dir.join("rustymdb.sqlite3"))?;

    if arguments == ["import-episodes"] {
        let path = data_dir.join("rustymdb.sqlite3");
        let rows =
            tokio::task::spawn_blocking(move || episodes::download_and_import(&path)).await??;
        println!("Imported {rows} IMDb episode relationships successfully.");
        return Ok(());
    }

    if arguments == ["import-cast"] {
        let path = data_dir.join("rustymdb.sqlite3");
        let rows = tokio::task::spawn_blocking(move || cast::download_and_import(&path)).await??;
        println!("Imported {rows} principal cast entries successfully.");
        return Ok(());
    }

    if arguments == ["import-credits"] {
        let path = data_dir.join("rustymdb.sqlite3");
        let (crew, people) =
            tokio::task::spawn_blocking(move || credits::download_and_import(&path)).await??;
        println!("Imported {crew} crew rows and {people} people successfully.");
        return Ok(());
    }

    if arguments == ["index-people"] {
        let path = data_dir.join("rustymdb.sqlite3");
        println!(
            "Building local People search index; existing data stays available until completion…"
        );
        tokio::task::spawn_blocking(move || people::build_index(&path)).await??;
        println!("People search index ready.");
        return Ok(());
    }
    if arguments == ["index-titles"] {
        let path = data_dir.join("rustymdb.sqlite3");
        println!(
            "Building the local title search index. Existing data stays available until this completes…"
        );
        tokio::task::spawn_blocking(move || catalog::build_index(&path)).await??;
        println!("Local title search index ready.");
        return Ok(());
    }

    if arguments == ["import-ratings"] {
        let path = data_dir.join("rustymdb.sqlite3");
        let rows =
            tokio::task::spawn_blocking(move || ratings::download_and_import(&path)).await??;
        println!(
            "Imported {rows} IMDb ratings successfully. Run this command again to refresh them."
        );
        return Ok(());
    }
    if arguments == ["import-titles"] {
        let path = data_dir.join("rustymdb.sqlite3");
        let rows =
            tokio::task::spawn_blocking(move || titles::download_and_import(&path)).await??;
        println!(
            "Imported {rows} IMDb titles successfully. Run this command again to refresh them."
        );
        return Ok(());
    }

    let address = env::var("RUSTYMDB_BIND").unwrap_or_else(|_| "127.0.0.1:3000".into());
    let mut token_bytes = [0u8; 32];
    getrandom::fill(&mut token_bytes)
        .map_err(|error| format!("Unable to create form token: {error}"))?;
    let form_token = token_bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let maintenance = maintenance::Manager::open(data_dir.clone(), env::current_exe()?)?;
    let tmdb = tmdb::Service::open(&data_dir)?;
    let scheduler = maintenance.clone();
    let app = Router::new()
        .route("/", get(home))
        .route(
            "/maintenance",
            get(maintenance_page).post(maintenance_start),
        )
        .route("/options", get(options_page).post(options_update))
        .route("/options/schedules", post(schedules_update))
        .route("/options/retention", post(retention_update))
        .route("/static/tmdb-logo.svg", get(tmdb_logo))
        .route("/maintenance/status", get(maintenance_status))
        .route("/static/maintenance.js", get(maintenance_script))
        .route("/search", get(search))
        .route("/title/{id}", get(title))
        .route("/collection/{id}", get(collection_page))
        .route("/title/{id}/episodes", get(season_episodes))
        .route("/static/seasons.js", get(seasons_script))
        .route("/tmdb-title/{media}/{id}", get(tmdb_title_link))
        .route("/person/{id}", get(person_page))
        .route("/tmdb-person/{id}", get(tmdb_person_link))
        .route("/person-photo/{id}", get(person_photo))
        .route("/poster/{id}", get(poster))
        .route("/watchlist", get(watchlist_page))
        .route("/watchlist/{id}", post(watchlist_update))
        .route("/watchlist/{id}/details", post(watchlist_details_update))
        .route("/static/style.css", get(stylesheet))
        .route("/static/back-to-top.js", get(back_to_top_script))
        .with_state(AppState {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()?,
            poster_client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(15))
                .build()?,
            photo_limit: std::sync::Arc::new(tokio::sync::Semaphore::new(3)),
            database: data_dir.join("rustymdb.sqlite3"),
            form_token,
            maintenance,
            tmdb,
        });
    let listener = tokio::net::TcpListener::bind(&address).await?;
    tokio::spawn(scheduler.run_scheduler());
    println!("RustyMDb listening on http://{}", listener.local_addr()?);
    println!("Database: {}", data_dir.join("rustymdb.sqlite3").display());
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

async fn search(
    State(state): State<AppState>,
    Query(input): Query<SearchQuery>,
) -> Result<(StatusCode, Html<String>), StatusCode> {
    let query = input.q.trim().to_owned();
    let kind = input.kind;
    let number = input.page.get();
    let mut page = SearchPage {
        query: query.clone(),
        results: vec![],
        message: String::new(),
        source: "Results retrieved from IMDb’s suggestion service.",
        kind,
        number,
        previous_url: String::new(),
        next_url: String::new(),
    };
    let mut status = StatusCode::OK;
    if query.is_empty() || query.chars().count() > 120 {
        page.message = "Enter a title or name between 1 and 120 characters.".into();
        status = StatusCode::BAD_REQUEST;
    } else {
        let path = state.database.clone();
        let local_query = query.clone();
        match tokio::task::spawn_blocking(move || {
            catalog::search(&path, &local_query, kind, number)
        })
        .await
        {
            Ok(Ok(Some(mut results))) => {
                page.source = if kind == catalog::TitleFilter::People {
                    "Searching locally imported IMDb people. No external request is needed."
                } else {
                    "Searching locally imported IMDb titles. No IMDb request is needed."
                };
                if number > 1 {
                    page.previous_url = search_url(&query, kind, number - 1);
                }
                if results.len() > 30
                    && let Some(next) = number.checked_add(1)
                {
                    page.next_url = search_url(&query, kind, next);
                }
                results.truncate(30);
                page.message = if results.is_empty() && number > 1 {
                    "No more results on this page. Return to the previous page or change your search.".into()
                } else if results.is_empty() {
                    if kind == catalog::TitleFilter::People {
                        "No matching people. Try a name, part of a name, or an IMDb person ID (nm…).".into()
                    } else if kind == catalog::TitleFilter::All {
                        "No matching local titles. Try a title word or an IMDb title ID.".into()
                    } else {
                        "No matching titles for this type. Try All titles or different title words."
                            .into()
                    }
                } else {
                    let start = (u64::from(number) - 1) * 30 + 1;
                    format!(
                        "Showing results {start}–{}.",
                        start + results.len() as u64 - 1
                    )
                };
                page.results = results;
                return page
                    .render()
                    .map(|html| (StatusCode::OK, Html(html)))
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR);
            }
            Ok(Ok(None)) if kind == catalog::TitleFilter::All && number == 1 => {}
            Ok(Ok(None)) => {
                page.message =
                    if kind==catalog::TitleFilter::People {"People search is not indexed yet. In Maintenance, choose Rebuild People search index, or refresh credits and people."}else{"Title-type filtering and additional result pages are unavailable until the local catalog is ready."}.into();
                page.source = "Local IMDb catalog unavailable";
                return page
                    .render()
                    .map(|html| (StatusCode::SERVICE_UNAVAILABLE, Html(html)))
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR);
            }
            error => {
                eprintln!("Local search failed: {error:?}");
                page.message =
                    "Local search is unavailable right now. Please try again shortly.".into();
                page.source = "Local IMDb dataset";
                return page
                    .render()
                    .map(|html| (StatusCode::SERVICE_UNAVAILABLE, Html(html)))
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
        let key = format!("search:{}", query.to_lowercase());
        let path = state.database.clone();
        let cache_key = key.clone();
        let cached = tokio::task::spawn_blocking(move || database::cached(&path, &cache_key)).await;
        let cached_results = match cached {
            Ok(Ok(Some(payload))) => serde_json::from_str::<imdb::SearchResponse>(&payload).ok(),
            Ok(Ok(None)) => None,
            error => {
                eprintln!("Cache read failed: {error:?}");
                None
            }
        };
        let results = match cached_results {
            Some(results) => Ok(results),
            None => imdb::search(&state.client, &query).await,
        };
        match results {
            Ok(results) => {
                if let Ok(payload) = serde_json::to_string(&results) {
                    let path = state.database.clone();
                    // Preserve the original expiry on cache hits.
                    let _ = tokio::task::spawn_blocking(move || {
                        if database::cached(&path, &key)?.is_none() {
                            database::store(&path, &key, &payload)?;
                        }
                        Ok::<_, rusqlite::Error>(())
                    })
                    .await
                    .map(|result| {
                        if let Err(error) = result {
                            eprintln!("Cache write failed: {error}");
                        }
                    });
                }
                page.results = results.d;
                if page.results.is_empty() {
                    page.message = "No matching titles found.".into();
                }
            }
            Err(error) => {
                eprintln!("IMDb search failed: {error}");
                status = StatusCode::BAD_GATEWAY;
                page.message =
                    "IMDb search is unavailable right now. Please try again shortly.".into();
            }
        }
    }
    page.render()
        .map(|html| (status, Html(html)))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn home() -> Result<Html<String>, StatusCode> {
    Home.render().map(Html).map_err(|error| {
        eprintln!("Template rendering failed: {error}");
        StatusCode::INTERNAL_SERVER_ERROR
    })
}

#[derive(Template)]
#[template(path = "episode_list.html")]
struct EpisodeList {
    series: episodes::Series,
}
async fn season_episodes(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(input): Query<TitleQuery>,
) -> Result<Html<String>, StatusCode> {
    if !imdb::valid_title_id(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    let season = episodes::season_value(&input.season).map_err(|_| StatusCode::BAD_REQUEST)?;
    if season == "all" {
        return Err(StatusCode::BAD_REQUEST);
    }
    let path = state.database;
    let number = input.page.get();
    let navigation =
        tokio::task::spawn_blocking(move || episodes::lookup(&path, &id, &season, number))
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let series = navigation.series.ok_or(StatusCode::NOT_FOUND)?;
    EpisodeList { series }
        .render()
        .map(Html)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}
async fn seasons_script() -> ([(&'static str, &'static str); 1], &'static str) {
    (
        [("content-type", "text/javascript; charset=utf-8")],
        include_str!("../static/seasons.js"),
    )
}

async fn back_to_top_script() -> ([(&'static str, &'static str); 1], &'static str) {
    (
        [("content-type", "text/javascript; charset=utf-8")],
        include_str!("../static/back-to-top.js"),
    )
}

async fn stylesheet() -> ([(&'static str, &'static str); 1], &'static str) {
    (
        [("content-type", "text/css; charset=utf-8")],
        include_str!("../static/style.css"),
    )
}
