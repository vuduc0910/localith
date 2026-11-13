//! Test helpers: a scriptable HTTP server standing in for Ollama and ChromaDB.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::Response;
use axum::Router;
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct Call {
    pub method: String,
    pub path: String,
    pub body: Value,
}

/// Decides the reply for a request: (status, body).
pub type Responder = Arc<dyn Fn(&Call) -> (u16, String) + Send + Sync>;

pub struct Mock {
    pub url: String,
    calls: Arc<Mutex<Vec<Call>>>,
}

impl Mock {
    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    pub fn calls_to(&self, path_suffix: &str) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|call| call.path.ends_with(path_suffix))
            .collect()
    }
}

/// Starts a server on a free local port that records every request.
pub async fn start_mock(responder: Responder) -> Mock {
    let calls: Arc<Mutex<Vec<Call>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = calls.clone();

    let app = Router::new().fallback(move |request: Request| {
        let recorded = recorded.clone();
        let responder = responder.clone();
        async move {
            let (parts, body) = request.into_parts();
            let bytes = axum::body::to_bytes(body, usize::MAX)
                .await
                .unwrap_or_default();
            let call = Call {
                method: parts.method.to_string(),
                path: parts.uri.path().to_string(),
                body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            };
            let (status, text) = responder(&call);
            recorded.lock().unwrap().push(call);

            let mut response = Response::new(Body::from(text));
            *response.status_mut() = StatusCode::from_u16(status).unwrap();
            response
        }
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    Mock { url, calls }
}

/// An Ollama chat reply streamed as several NDJSON lines.
pub fn chat_stream(content: &str) -> String {
    let middle = content.chars().count() / 2;
    let split = content
        .char_indices()
        .nth(middle)
        .map(|(i, _)| i)
        .unwrap_or(0);
    let (first, second) = content.split_at(split);
    let line = |text: &str, done: bool| {
        serde_json::json!({ "message": { "role": "assistant", "content": text }, "done": done })
            .to_string()
    };
    format!(
        "{}\n{}\n{}\n",
        line(first, false),
        line(second, false),
        line("", true)
    )
}
