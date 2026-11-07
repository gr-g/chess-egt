//! HTTP tablebase API. `/outcome` describes the requested position; `/query`
//! also includes its legal moves, sorted best-first for the root.
//!
//! Root outcomes are from the requested position's side to move. Move outcomes
//! are from the successor's side to move, so a winning move has category `loss`.
//! Returned successor outcomes are not inverted or distance-adjusted. Captures
//! and promotions reset DTC to one ply for ranking only.
//!
//! `dtc` is signed in plies: positive for wins, negative for losses, zero for an
//! already checkmated position. Draws have null `dtc` and `dtc_type`.

mod common;

use axum::{
    Json, Router,
    extract::{Query, Request, State, rejection::QueryRejection},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use chess_egt::{AggregateStats, ConversionType, DtcOutcome, EgtError, EgtProber, EgtStatistics};
use clap::Parser;
use common::parse_byte_size;
use serde::{Deserialize, Serialize};
use shakmaty::{CastlingMode, Chess, Position, PositionError, fen::Fen, san::SanPlus};
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Instant};
use tokio::sync::Semaphore;

const MAX_OPERATIONS: usize = 8;

#[derive(Debug, Parser)]
#[command(about = "gg-EGT API server")]
struct Cli {
    #[arg(long, default_value = "./ggegt")]
    path: PathBuf,

    /// Retained frame-cache memory (bytes, B, KiB, MiB, or GiB).
    #[arg(long, default_value = "256MiB", value_parser = parse_byte_size)]
    cache_size: usize,

    #[arg(long, default_value = "127.0.0.1:44348")]
    bind: SocketAddr,
}

#[derive(Clone)]
struct AppState {
    prober: Arc<EgtProber>,
    statistics: Arc<EgtStatistics>,
    operations: Arc<Semaphore>,
}

impl AppState {
    async fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&Self) -> Result<T, ApiError> + Send + 'static,
    ) -> Result<T, ApiError> {
        let permit = self.operations.clone().try_acquire_owned().map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "server_busy",
                "Server is busy",
            )
        })?;
        let state = self.clone();
        tokio::task::spawn_blocking(move || {
            // Keep the permit until the work finishes, even if the client disconnects.
            let _permit = permit;
            operation(&state)
        })
        .await
        .map_err(|error| {
            eprintln!("tablebase task failed: {error}");
            ApiError::internal()
        })?
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let (prober, statistics) = tokio::task::spawn_blocking(move || {
        let mut prober = EgtProber::new(&cli.path)?;
        prober.set_cache_size(cli.cache_size)?;
        // An empty selection validates the root format without loading any tables.
        let statistics = EgtStatistics::new(&cli.path)?;
        statistics.stats(&[])?;
        Ok::<_, EgtError>((prober, statistics))
    })
    .await??;
    let listener = tokio::net::TcpListener::bind(cli.bind).await?;
    eprintln!("Listening on http://{}", listener.local_addr()?);
    axum::serve(listener, router(prober, statistics))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("failed to install SIGTERM handler");
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.expect("failed to install SIGINT handler"),
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install interrupt handler");
}

fn router(prober: EgtProber, statistics: EgtStatistics) -> Router {
    Router::new()
        .route("/outcome", get(outcome))
        .route("/query", get(query))
        .route("/stats", get(stats))
        .fallback(async || ApiError::new(StatusCode::NOT_FOUND, "not_found", "Unknown route"))
        .method_not_allowed_fallback(async || {
            ApiError::new(
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "Method not allowed",
            )
        })
        .layer(middleware::from_fn(log_request))
        .with_state(AppState {
            prober: Arc::new(prober),
            statistics: Arc::new(statistics),
            operations: Arc::new(Semaphore::new(MAX_OPERATIONS)),
        })
}

async fn log_request(request: Request, next: Next) -> Response {
    let start = Instant::now();
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let response = if request.uri().to_string().len() > 8192 {
        ApiError::new(
            StatusCode::URI_TOO_LONG,
            "uri_too_long",
            "Request URI is too long",
        )
        .into_response()
    } else {
        next.run(request).await
    };
    eprintln!(
        "{method} {path} {} {}ms",
        response.status(),
        start.elapsed().as_millis()
    );
    response
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryParams {
    fen: String,
}

#[derive(Serialize)]
struct OutcomeResponse {
    checkmate: bool,
    stalemate: bool,
    insufficient_material: bool,
    dtc: Option<i32>,
    dtc_type: Option<&'static str>,
    category: &'static str,
}

impl OutcomeResponse {
    fn new(position: &Chess, outcome: DtcOutcome) -> Self {
        let (category, dtc, conversion) = match outcome {
            DtcOutcome::Win(conversion, ply) => ("win", Some(i32::from(ply)), Some(conversion)),
            DtcOutcome::Loss(conversion, ply) => ("loss", Some(-i32::from(ply)), Some(conversion)),
            DtcOutcome::Draw => ("draw", None, None),
        };
        Self {
            checkmate: position.is_checkmate(),
            stalemate: position.is_stalemate(),
            insufficient_material: position.is_insufficient_material(),
            dtc,
            dtc_type: conversion.map(|conversion| match conversion {
                ConversionType::Checkmate => "checkmate",
                ConversionType::Capture => "capture",
                ConversionType::Promotion => "promotion",
            }),
            category,
        }
    }
}

fn parse_position(fen: &str) -> Result<Chess, ApiError> {
    fen.parse::<Fen>()
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid_fen", "Invalid FEN"))?
        .into_position(CastlingMode::Standard)
        .or_else(PositionError::ignore_impossible_check)
        .map_err(|_| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_position",
                "Invalid position",
            )
        })
}

async fn outcome(
    State(state): State<AppState>,
    params: Result<Query<QueryParams>, QueryRejection>,
) -> Result<Json<OutcomeResponse>, ApiError> {
    let Query(params) = params.map_err(ApiError::invalid_query)?;
    state
        .run(move |state| {
            let position = parse_position(&params.fen)?;
            let outcome = state.prober.probe(&position)?;
            Ok(Json(OutcomeResponse::new(&position, outcome)))
        })
        .await
}

#[derive(Serialize)]
struct QueryResponse {
    #[serde(flatten)]
    outcome: OutcomeResponse,
    moves: Vec<MoveResponse>,
}

#[derive(Serialize)]
struct MoveResponse {
    uci: String,
    san: String,
    conversion: bool,
    #[serde(flatten)]
    outcome: OutcomeResponse,
}

async fn query(
    State(state): State<AppState>,
    params: Result<Query<QueryParams>, QueryRejection>,
) -> Result<Json<QueryResponse>, ApiError> {
    let Query(params) = params.map_err(ApiError::invalid_query)?;
    state
        .run(move |state| {
            let position = parse_position(&params.fen)?;
            let mut result = state.prober.probe_with_moves(&position)?;
            // Best for the root first; response outcomes still describe successors.
            result.moves.sort_by_key(|(m, outcome)| {
                std::cmp::Reverse(outcome.before_move(m))
            });
            let moves = result
                .moves
                .into_iter()
                .map(|(m, outcome)| {
                    let mut successor = position.clone();
                    let san = SanPlus::from_move_and_play_unchecked(&mut successor, m).to_string();
                    MoveResponse {
                        uci: m.to_uci(CastlingMode::Standard).to_string(),
                        san,
                        conversion: m.is_capture() || m.is_promotion(),
                        outcome: OutcomeResponse::new(&successor, outcome),
                    }
                })
                .collect();
            Ok(Json(QueryResponse {
                outcome: OutcomeResponse::new(&position, result.outcome),
                moves,
            }))
        })
        .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatsParams {
    tables: String,
}

async fn stats(
    State(state): State<AppState>,
    params: Result<Query<StatsParams>, QueryRejection>,
) -> Result<Json<AggregateStats>, ApiError> {
    let Query(params) = params.map_err(ApiError::invalid_query)?;
    state
        .run(move |state| {
            let selectors: Vec<_> = params.tables.split(',').map(str::trim).collect();
            Ok(Json(state.statistics.stats(&selectors)?))
        })
        .await
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
        }
    }

    fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "Internal tablebase error",
        )
    }

    fn invalid_query(_: QueryRejection) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_query",
            "Missing or invalid query parameters",
        )
    }
}

impl From<EgtError> for ApiError {
    fn from(error: EgtError) -> Self {
        match error {
            EgtError::InvalidEndgameName { .. } => Self::new(
                StatusCode::BAD_REQUEST,
                "invalid_selector",
                "Invalid table selection",
            ),
            EgtError::UnsupportedPosition(reason) => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unsupported_position",
                reason,
            ),
            EgtError::FileNotFound(_) => Self::new(
                StatusCode::NOT_FOUND,
                "table_unavailable",
                "Required tablebase data is unavailable",
            ),
            other => {
                eprintln!("tablebase error: {other}");
                if matches!(other, EgtError::Storage { .. }) {
                    Self::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "storage_unavailable",
                        "Tablebase storage is unavailable",
                    )
                } else {
                    Self::internal()
                }
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct ErrorBody {
            code: &'static str,
            message: &'static str,
        }
        #[derive(Serialize)]
        struct Envelope {
            error: ErrorBody,
        }
        (
            self.status,
            Json(Envelope {
                error: ErrorBody {
                    code: self.code,
                    message: self.message,
                },
            }),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use chess_egt::EgtGenerator;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    async fn request(app: &Router, uri: &str, status: StatusCode) -> String {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{uri}");
        assert_eq!(response.headers()["content-type"], "application/json");
        String::from_utf8(
            to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }

    fn query_uri(fen: &str) -> String {
        format!(
            "/query?{}",
            url::form_urlencoded::Serializer::new(String::new())
                .append_pair("fen", fen)
                .finish()
        )
    }

    #[test]
    fn response_distances_conversions_and_field_order() {
        let position = Chess::default();
        for (conversion, name) in [
            (ConversionType::Capture, "capture"),
            (ConversionType::Promotion, "promotion"),
            (ConversionType::Checkmate, "checkmate"),
        ] {
            for (outcome, dtc, category) in [
                (DtcOutcome::Win(conversion, 9), "9", "win"),
                (DtcOutcome::Loss(conversion, 8), "-8", "loss"),
            ] {
                assert_eq!(
                    serde_json::to_string(&OutcomeResponse::new(&position, outcome)).unwrap(),
                    format!(
                        r#"{{"checkmate":false,"stalemate":false,"insufficient_material":false,"dtc":{dtc},"dtc_type":"{name}","category":"{category}"}}"#
                    )
                );
            }
        }
        let cli = Cli::try_parse_from(["chess-egt-server"]).unwrap();
        assert_eq!(cli.cache_size, 256 * 1024 * 1024);
        assert_eq!(cli.bind, "127.0.0.1:44348".parse::<SocketAddr>().unwrap());
    }

    #[tokio::test]
    async fn routes_probe_aggregate_and_reject_bad_requests() {
        struct TestDir(PathBuf);
        impl Drop for TestDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let dir =
            TestDir(std::env::temp_dir().join(format!("chess-egt-server-{}", std::process::id())));
        std::fs::create_dir(&dir.0).unwrap();
        let path = dir.0.clone();
        let prober = tokio::task::spawn_blocking(move || {
            let mut generator = EgtGenerator::new(&path).unwrap();
            generator.set_threads(2);
            generator.set_verify(false);
            generator
                .generate_selected(&["K_K", "KQ_K", "KR_K", "KB_K", "KN_K", "KP_K"])
                .unwrap();
            let mut prober = EgtProber::new(path).unwrap();
            prober.set_cache_size(0).unwrap();
            prober
        })
        .await
        .unwrap();
        let app = router(prober, EgtStatistics::new(&dir.0).unwrap());
        for (fen, expected) in [
            (
                "8/8/8/8/8/8/3K4/k7 w - - 0 1",
                json!({"checkmate":false,"stalemate":false,"insufficient_material":true,"dtc":null,"dtc_type":null,"category":"draw"}),
            ),
            (
                "7k/6Q1/5K2/8/8/8/8/8 b - - 100 1",
                json!({"checkmate":true,"stalemate":false,"insufficient_material":false,"dtc":0,"dtc_type":"checkmate","category":"loss"}),
            ),
            (
                "7k/5K2/6Q1/8/8/8/8/8 b - - 0 1",
                json!({"checkmate":false,"stalemate":true,"insufficient_material":false,"dtc":null,"dtc_type":null,"category":"draw"}),
            ),
        ] {
            let uri = query_uri(fen);
            let body = request(&app, &uri.replace("/query?", "/outcome?"), StatusCode::OK).await;
            assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), expected);
            let body = request(&app, &uri, StatusCode::OK).await;
            let mut actual: Value = serde_json::from_str(&body).unwrap();
            let moves = actual.as_object_mut().unwrap().remove("moves").unwrap();
            assert_eq!(actual, expected);
            assert_eq!(
                moves.as_array().unwrap().len(),
                parse_position(fen).unwrap().legal_moves().len()
            );
        }
        for fen in [
            "7k/8/5KQ1/8/8/8/8/8 w - - 0 1",
            "8/7P/8/8/8/8/8/k1K5 w - - 0 1",
            "8/8/8/8/8/8/1Q6/k2K4 b - - 0 1",
        ] {
            let position = parse_position(fen).unwrap();
            let body = request(&app, &query_uri(fen), StatusCode::OK).await;
            let response: Value = serde_json::from_str(&body).unwrap();
            let moves = response["moves"].as_array().unwrap();
            assert_eq!(moves.len(), position.legal_moves().len());
            let mut keys = Vec::new();
            for entry in moves {
                let m = position
                    .legal_moves()
                    .into_iter()
                    .find(|m| m.to_uci(CastlingMode::Standard).to_string() == entry["uci"])
                    .unwrap();
                assert_eq!(
                    entry["san"],
                    SanPlus::from_move(position.clone(), m).to_string()
                );
                assert_eq!(entry["conversion"], m.is_capture() || m.is_promotion());
                let successor = position.clone().play(m).unwrap();
                let fen =
                    Fen::from_position(&successor, shakmaty::EnPassantMode::Legal).to_string();
                let uri = query_uri(&fen).replace("/query?", "/outcome?");
                let body = request(&app, &uri, StatusCode::OK).await;
                let expected: Value = serde_json::from_str(&body).unwrap();
                for (field, value) in expected.as_object().unwrap() {
                    assert_eq!(&entry[field], value);
                }
                let rank = match entry["category"].as_str().unwrap() {
                    "loss" => 0,
                    "draw" => 1,
                    "win" => 2,
                    _ => unreachable!(),
                };
                let dtc = entry["dtc"].as_i64().unwrap_or(0);
                let distance = if entry["conversion"] == true { 0 } else { -dtc };
                keys.push((rank, distance));
            }
            assert!(keys.windows(2).all(|pair| pair[0] <= pair[1]));
        }
        let body = request(&app, "/stats?tables=KQ_K,KQ_K", StatusCode::OK).await;
        let stats: AggregateStats = serde_json::from_str(&body).unwrap();
        assert_eq!((stats.selected_tables, stats.available_tables), (1, 1));
        let body = request(&app, "/stats?tables=4/2v2", StatusCode::OK).await;
        let stats: AggregateStats = serde_json::from_str(&body).unwrap();
        assert!(stats.selected_tables > 0);
        assert_eq!(stats.available_tables, 0);
        assert_eq!(stats.unique_positions, None);
        for (uri, status, code) in [
            (
                "/query".to_owned(),
                StatusCode::BAD_REQUEST,
                "invalid_query",
            ),
            (
                "/query?fen=bad".to_owned(),
                StatusCode::BAD_REQUEST,
                "invalid_fen",
            ),
            (
                query_uri("8/8/8/8/8/8/8/k7 w - - 0 1"),
                StatusCode::BAD_REQUEST,
                "invalid_position",
            ),
            (
                query_uri("r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1"),
                StatusCode::UNPROCESSABLE_ENTITY,
                "unsupported_position",
            ),
            (
                query_uri("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w - - 0 1"),
                StatusCode::UNPROCESSABLE_ENTITY,
                "unsupported_position",
            ),
            (
                query_uri("8/8/8/8/8/2K5/3RR3/7k w - - 0 1"),
                StatusCode::NOT_FOUND,
                "table_unavailable",
            ),
            (
                "/stats".to_owned(),
                StatusCode::BAD_REQUEST,
                "invalid_query",
            ),
            (
                "/stats?tables=".to_owned(),
                StatusCode::BAD_REQUEST,
                "invalid_selector",
            ),
            (
                "/stats?tables=../secret".to_owned(),
                StatusCode::BAD_REQUEST,
                "invalid_selector",
            ),
            (
                "/stats?tables=3,KQ_K".to_owned(),
                StatusCode::BAD_REQUEST,
                "invalid_selector",
            ),
            ("/unknown".to_owned(), StatusCode::NOT_FOUND, "not_found"),
        ] {
            for uri in [uri.clone(), uri.replace("/query", "/outcome")] {
                let body = request(&app, &uri, status).await;
                let error: Value = serde_json::from_str(&body).unwrap();
                assert_eq!(error["error"]["code"], code);
                assert!(!body.contains(dir.0.to_str().unwrap()));
            }
        }
        let state = AppState {
            prober: Arc::new(EgtProber::new(&dir.0).unwrap()),
            statistics: Arc::new(EgtStatistics::new(&dir.0).unwrap()),
            operations: Arc::new(Semaphore::new(0)),
        };
        assert_eq!(state.run(|_| Ok(())).await.unwrap_err().code, "server_busy");
        assert!(!dir.0.join("4/2v2/stats.json").exists());
    }
}
