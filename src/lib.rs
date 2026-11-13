//! Localith: a local-first website crawler and knowledge engine.
//!
//! `agent-browser` reads pages, Ollama turns them into screen knowledge, and
//! ChromaDB stores that knowledge for question answering.

pub mod browser;
pub mod cli;
pub mod config;
pub mod crawler;
pub mod knowledge;
pub mod ollama;
pub mod rag;
pub mod robots;
pub mod server;
pub mod types;
pub mod util;
