//! Lowers portable sampling knobs into each dialect's body fields.
//!
//! The mapping is the reference agent's: Chat Completions takes every knob,
//! OpenAI Responses takes `temperature`/`top_p`/`seed`, and Anthropic Messages
//! takes `temperature`/`top_p`/`top_k`/`stop_sequences`
//! (`packages/ai/src/protocols/openai-chat.ts:905-907`,
//! `.../open-responses.ts:823-826`, `.../anthropic-messages.ts:990-992`).
//!
//! Only configured knobs are written. An unset knob leaves the body untouched,
//! so a strict provider never sees a zero-valued field EE invented.
//!
//! `maxTokens` is deliberately not handled here: it is the output bound, which
//! every codec already fits against the catalog window through
//! [`EndpointProfile::fitted_output_limit_with`].
//!
//! [`EndpointProfile::fitted_output_limit_with`]: crate::profile::EndpointProfile::fitted_output_limit_with

use ee_agent_orchestrator::GenerationOptions;
use serde_json::{Value, json};

/// Writes the Chat Completions sampling subset.
pub fn apply_generation_chat(body: &mut Value, generation: &GenerationOptions) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    for (field, value) in [
        ("temperature", generation.temperature.map(Value::from)),
        ("top_p", generation.top_p.map(Value::from)),
        ("top_k", generation.top_k.map(Value::from)),
        ("frequency_penalty", generation.frequency_penalty.map(Value::from)),
        ("presence_penalty", generation.presence_penalty.map(Value::from)),
        ("seed", generation.seed.map(Value::from)),
    ] {
        if let Some(value) = value {
            object.insert(String::from(field), value);
        }
    }
    if let Some(stop) = generation.stop.as_ref().filter(|stop| !stop.is_empty()) {
        object.insert(String::from("stop"), json!(stop));
    }
}

/// Writes the OpenAI Responses sampling subset.
pub fn apply_generation_responses(body: &mut Value, generation: &GenerationOptions) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    for (field, value) in [
        ("temperature", generation.temperature.map(Value::from)),
        ("top_p", generation.top_p.map(Value::from)),
        ("seed", generation.seed.map(Value::from)),
    ] {
        if let Some(value) = value {
            object.insert(String::from(field), value);
        }
    }
}

/// Writes the Anthropic Messages sampling subset.
pub fn apply_generation_messages(body: &mut Value, generation: &GenerationOptions) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    for (field, value) in [
        ("temperature", generation.temperature.map(Value::from)),
        ("top_p", generation.top_p.map(Value::from)),
        ("top_k", generation.top_k.map(Value::from)),
    ] {
        if let Some(value) = value {
            object.insert(String::from(field), value);
        }
    }
    if let Some(stop) = generation.stop.as_ref().filter(|stop| !stop.is_empty()) {
        object.insert(String::from("stop_sequences"), json!(stop));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> GenerationOptions {
        GenerationOptions {
            temperature: Some(0.3),
            top_p: Some(0.8),
            top_k: Some(20),
            frequency_penalty: Some(0.1),
            presence_penalty: Some(-0.2),
            seed: Some(42),
            stop: Some(vec![String::from("END")]),
            ..GenerationOptions::none()
        }
    }

    #[test]
    fn nothing_configured_writes_nothing() {
        for apply in [
            apply_generation_chat as fn(&mut Value, &GenerationOptions),
            apply_generation_responses,
            apply_generation_messages,
        ] {
            let mut body = json!({ "model": "m" });
            apply(&mut body, &GenerationOptions::none());
            assert_eq!(body, json!({ "model": "m" }));
        }
    }

    #[test]
    fn chat_takes_every_knob_with_native_types() {
        let mut body = json!({ "model": "m" });
        apply_generation_chat(&mut body, &full());
        assert_eq!(
            body,
            json!({
                "model": "m",
                "temperature": 0.3,
                "top_p": 0.8,
                "top_k": 20,
                "frequency_penalty": 0.1,
                "presence_penalty": -0.2,
                "seed": 42,
                "stop": ["END"]
            })
        );
        assert!(body["seed"].is_i64(), "seed stays an integer");
    }

    #[test]
    fn responses_takes_only_its_subset() {
        let mut body = json!({ "model": "m" });
        apply_generation_responses(&mut body, &full());
        assert_eq!(body, json!({ "model": "m", "temperature": 0.3, "top_p": 0.8, "seed": 42 }));
    }

    #[test]
    fn messages_renames_stop_to_stop_sequences() {
        let mut body = json!({ "model": "m" });
        apply_generation_messages(&mut body, &full());
        assert_eq!(
            body,
            json!({
                "model": "m",
                "temperature": 0.3,
                "top_p": 0.8,
                "top_k": 20,
                "stop_sequences": ["END"]
            })
        );
    }
}
