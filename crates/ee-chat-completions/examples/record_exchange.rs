//! Records one real Chat Completions exchange into a replayable cassette.
//!
//! Run against a provider whose response shape changed (a new model revision, a
//! different route, an external agent) to freeze the exchange, then commit the
//! cassette and replay it in a regression test:
//!
//! ```text
//! EE_RECORD_ENDPOINT=https://generativelanguage.googleapis.com/v1beta/openai/chat/completions \
//! EE_RECORD_MODEL=gemini-2.5-flash \
//! EE_RECORD_API_KEY=... \
//! EE_RECORD_OUT=crates/ee-chat-completions/tests/fixtures/gemini-chat-stream.json \
//! EE_RECORD_STREAM=1 \
//! cargo run -p ee-chat-completions --example record_exchange
//! ```
//!
//! The credential is read at send time and never printed or persisted: the
//! recorder redacts `Authorization` and provider key headers before writing.

use std::path::PathBuf;
use std::sync::Arc;

use ee_chat_completions::{
    BearerToken, ChatCompletionsClient, EndpointProfile, EndpointTransport, StreamDelta,
    TokenSource, TrustedEndpoint,
};
use serde_json::json;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), String> {
    let endpoint = required("EE_RECORD_ENDPOINT")?;
    let model = required("EE_RECORD_MODEL")?;
    let key_var = optional("EE_RECORD_API_KEY_ENV", "EE_RECORD_API_KEY");
    let out = PathBuf::from(required("EE_RECORD_OUT")?);
    let prompt = optional("EE_RECORD_PROMPT", "Reply with the single word: pong");
    let label = optional("EE_RECORD_LABEL", "recorded-chat-completions");
    let streaming =
        matches!(std::env::var("EE_RECORD_STREAM").as_deref(), Ok("1" | "true" | "yes"));

    let token: TokenSource = {
        let key_var = key_var.clone();
        Arc::new(move || {
            std::env::var(&key_var)
                .map(BearerToken::new)
                .map_err(|_| format!("{key_var} is not set; export it before recording"))
        })
    };
    let profile = EndpointProfile::new(
        label,
        key_var,
        TrustedEndpoint::parse(&endpoint).map_err(|error| error.to_string())?,
        model,
    );
    let (transport, recorder) =
        EndpointTransport::recording(profile, token).map_err(|error| error.to_string())?;
    let client = ChatCompletionsClient::from_transport(transport);
    let messages = vec![json!({ "role": "user", "content": prompt })];

    let turn = if streaming {
        let mut sink = |delta: StreamDelta| {
            if let StreamDelta::Text(text) = &delta {
                print!("{text}");
            }
            Ok(())
        };
        client
            .complete_streaming(&messages, &[], &mut sink)
            .await
            .map_err(|error| error.to_string())?
    } else {
        client.complete(&messages, &[]).await.map_err(|error| error.to_string())?
    };

    println!(
        "\nfinish_reason={} content={}",
        turn.finish_reason.as_deref().unwrap_or("unknown"),
        turn.content
    );
    let cassette = recorder.cassette();
    cassette.save(&out)?;
    println!("recorded {} exchange(s) to {}", recorder.len(), out.display());
    Ok(())
}

/// Reads a required environment variable, naming it but never its value.
fn required(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set"))
}

/// Reads an optional environment variable with a default.
fn optional(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}
