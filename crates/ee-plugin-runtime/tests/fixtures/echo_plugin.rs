//! Fake native plugin used by integration tests.
//!
//! Reads v2 `HostToPlugin` messages and writes `PluginToHost` messages. The
//! `ECHO_TRANSPORT` environment variable selects newline or Content-Length
//! framing.

use std::io::{self, BufRead, BufReader, Write};

use ee_plugin_api::contribution::{StatusAlignment, StatusItemContribution};
use ee_plugin_api::effect::Effect;
use ee_plugin_api::ids::{ContributionId, ServiceId};
use ee_plugin_api::wire::{
    ContributionEnvelope, EffectEnvelope, HealthReport, HostToPlugin, PluginResponse, PluginToHost,
    ServiceRequestParams, WireError,
};
use serde_json::{Value, json};

#[derive(Clone, Copy)]
enum Framing {
    Newline,
    ContentLength,
}

fn main() {
    let framing = match std::env::var("ECHO_TRANSPORT").as_deref() {
        Ok("content_length") => Framing::ContentLength,
        _ => Framing::Newline,
    };
    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = io::stdout();
    let mut writer = stdout.lock();

    loop {
        let message = match read_message(&mut reader, framing) {
            Ok(Some(message)) => message,
            Ok(None) => break,
            Err(error) => {
                let _ = writeln!(io::stderr(), "echo plugin read error: {error}");
                break;
            }
        };

        match message {
            HostToPlugin::Initialize(params) => {
                write_message(
                    &mut writer,
                    framing,
                    &PluginToHost::Response(PluginResponse {
                        request_id: 0,
                        result: Some(json!({
                            "plugin_id": params.plugin_id.as_str(),
                            "host": params.host.as_str(),
                        })),
                        error: None,
                    }),
                );
            }
            HostToPlugin::Command(params) => match params.id.as_str() {
                "echo.ping" => respond(&mut writer, framing, params.request_id, Ok(params.args)),
                "echo.effect" => {
                    write_message(
                        &mut writer,
                        framing,
                        &PluginToHost::Effect(EffectEnvelope {
                            request_id: Some(params.request_id),
                            effects: vec![Effect::Noop],
                        }),
                    );
                    respond(&mut writer, framing, params.request_id, Ok(json!({"effect": true})));
                }
                "echo.contribution" => {
                    write_message(
                        &mut writer,
                        framing,
                        &PluginToHost::Contribution(ContributionEnvelope {
                            revision: 1,
                            upsert: vec![ee_plugin_api::contribution::Contribution::StatusItem(
                                StatusItemContribution {
                                    key: "echo.status".to_owned(),
                                    text: "echo".to_owned(),
                                    alignment: StatusAlignment::Left,
                                    priority: 0,
                                },
                            )],
                            remove: Vec::new(),
                        }),
                    );
                    respond(
                        &mut writer,
                        framing,
                        params.request_id,
                        Ok(json!({"contribution": true})),
                    );
                }
                "echo.health" => {
                    write_message(
                        &mut writer,
                        framing,
                        &PluginToHost::Health(HealthReport {
                            state: "degraded".to_owned(),
                            reason: Some("test".to_owned()),
                        }),
                    );
                    respond(&mut writer, framing, params.request_id, Ok(json!({"health": true})));
                }
                "echo.request" => {
                    write_message(
                        &mut writer,
                        framing,
                        &PluginToHost::Request(ServiceRequestParams {
                            service: ServiceId::from_static("editor.document@1"),
                            method: "snapshot".to_owned(),
                            request_id: 77,
                            params: json!({}),
                        }),
                    );
                    match read_message(&mut reader, framing) {
                        Ok(Some(HostToPlugin::Response(response))) => {
                            respond(
                                &mut writer,
                                framing,
                                params.request_id,
                                match response.error {
                                    Some(error) => Err(error),
                                    None => Ok(response.result.unwrap_or(Value::Null)),
                                },
                            );
                        }
                        _ => respond(
                            &mut writer,
                            framing,
                            params.request_id,
                            Err(WireError {
                                class: "transport".to_owned(),
                                code: "plugin.transport".to_owned(),
                                message: "missing host response".to_owned(),
                                retryable: false,
                            }),
                        ),
                    }
                }
                "echo.timeout" => {}
                "echo.malformed" => {
                    writer.write_all(b"not json\n").unwrap();
                    writer.flush().unwrap();
                }
                "echo.exit" => std::process::exit(3),
                other => respond(
                    &mut writer,
                    framing,
                    params.request_id,
                    Err(WireError {
                        class: "runtime".to_owned(),
                        code: "plugin.runtime".to_owned(),
                        message: format!("unknown command `{other}`"),
                        retryable: false,
                    }),
                ),
            },
            HostToPlugin::Event(_) => {
                write_message(
                    &mut writer,
                    framing,
                    &PluginToHost::Effect(EffectEnvelope {
                        request_id: None,
                        effects: vec![Effect::Noop],
                    }),
                );
            }
            HostToPlugin::Shutdown(_) => {
                respond(&mut writer, framing, 0, Ok(json!({"shutdown": true})));
                break;
            }
            HostToPlugin::ContributionRequest(params) => {
                write_message(
                    &mut writer,
                    framing,
                    &PluginToHost::Contribution(ContributionEnvelope {
                        revision: params.revision.saturating_add(1),
                        upsert: Vec::new(),
                        remove: vec![ContributionId::new("echo.status").unwrap()],
                    }),
                );
            }
            HostToPlugin::Response(_) => {}
        }
    }
}

fn respond(
    writer: &mut impl Write,
    framing: Framing,
    request_id: u64,
    result: Result<Value, WireError>,
) {
    let (result, error) = match result {
        Ok(value) => (Some(value), None),
        Err(error) => (None, Some(error)),
    };
    write_message(
        writer,
        framing,
        &PluginToHost::Response(PluginResponse { request_id, result, error }),
    );
}

fn read_message(
    reader: &mut impl BufRead,
    framing: Framing,
) -> Result<Option<HostToPlugin>, String> {
    let payload = match framing {
        Framing::Newline => {
            let mut buffer = Vec::new();
            loop {
                buffer.clear();
                let read =
                    reader.read_until(b'\n', &mut buffer).map_err(|error| error.to_string())?;
                if read == 0 {
                    return Ok(None);
                }
                while matches!(buffer.last(), Some(b'\n' | b'\r')) {
                    buffer.pop();
                }
                if !buffer.is_empty() {
                    break buffer;
                }
            }
        }
        Framing::ContentLength => {
            let mut length = None;
            loop {
                let mut header = String::new();
                let read = reader.read_line(&mut header).map_err(|error| error.to_string())?;
                if read == 0 {
                    return Ok(None);
                }
                let header = header.trim_end_matches(['\r', '\n']);
                if header.is_empty() {
                    break;
                }
                if let Some(value) = header.strip_prefix("Content-Length:") {
                    length = Some(value.trim().parse::<usize>().map_err(|_| "bad length")?);
                }
            }
            let mut payload = vec![0_u8; length.ok_or("missing length")?];
            reader.read_exact(&mut payload).map_err(|error| error.to_string())?;
            payload
        }
    };

    serde_json::from_slice(&payload)
        .map(Some)
        .map_err(|error| format!("malformed message: {error}"))
}

fn write_message(writer: &mut impl Write, framing: Framing, message: &PluginToHost) {
    let payload = serde_json::to_vec(message).expect("plugin message serializes");
    match framing {
        Framing::Newline => {
            writer.write_all(&payload).unwrap();
            writer.write_all(b"\n").unwrap();
        }
        Framing::ContentLength => {
            writer
                .write_all(format!("Content-Length: {}\r\n\r\n", payload.len()).as_bytes())
                .unwrap();
            writer.write_all(&payload).unwrap();
        }
    }
    writer.flush().unwrap();
}
