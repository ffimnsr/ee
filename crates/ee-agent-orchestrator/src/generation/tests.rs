use super::*;

fn full() -> GenerationOptions {
    GenerationOptions {
        max_output_tokens: Some(4096),
        temperature: Some(0.4),
        top_p: Some(0.9),
        top_k: Some(40),
        frequency_penalty: Some(-0.5),
        presence_penalty: Some(0.25),
        seed: Some(-7),
        stop: Some(vec![String::from("END")]),
    }
}

#[test]
fn empty_options_emit_nothing() {
    let options = GenerationOptions::none();
    assert!(options.is_empty());
    assert!(GenerationOptions::default().is_empty());
    assert_eq!(options.validate(), Ok(()));
    assert!(
        options
            .unsupported(SamplingSupport { temperature: false, ..SamplingSupport::ALL })
            .is_none()
    );
}

#[test]
fn merge_is_per_knob_and_request_wins() {
    let base = full();
    let request =
        GenerationOptions { temperature: Some(1.0), seed: Some(3), ..GenerationOptions::none() };
    let merged = request.merged_over(&base);

    assert_eq!(merged.temperature, Some(1.0));
    assert_eq!(merged.seed, Some(3));
    assert_eq!(merged.top_p, Some(0.9));
    assert_eq!(merged.stop.as_deref(), Some(&[String::from("END")][..]));
}

#[test]
fn empty_stop_list_falls_back_to_the_default() {
    let base = full();
    let request = GenerationOptions { stop: Some(Vec::new()), ..GenerationOptions::none() };
    let merged = request.merged_over(&base);
    assert_eq!(merged.stop.as_deref(), Some(&[String::from("END")][..]));
}

#[test]
fn ranges_are_validated_with_the_knob_name() {
    let cases = [
        (GenerationOptions { temperature: Some(2.5), ..GenerationOptions::none() }, "temperature"),
        (GenerationOptions { top_p: Some(1.5), ..GenerationOptions::none() }, "top_p"),
        (GenerationOptions { top_k: Some(0), ..GenerationOptions::none() }, "top_k"),
        (
            GenerationOptions { frequency_penalty: Some(-3.0), ..GenerationOptions::none() },
            "frequency_penalty",
        ),
        (
            GenerationOptions { presence_penalty: Some(3.0), ..GenerationOptions::none() },
            "presence_penalty",
        ),
    ];
    for (options, knob) in cases {
        let error = options.validate().expect_err("invalid");
        assert!(error.contains(knob), "{error}");
    }
}

#[test]
fn stop_bounds_are_enforced() {
    let too_many = GenerationOptions {
        stop: Some(vec![String::from("a"); MAX_STOP_SEQUENCES + 1]),
        ..GenerationOptions::none()
    };
    assert!(too_many.validate().expect_err("too many").contains("stop"));

    let empty = GenerationOptions { stop: Some(vec![String::new()]), ..GenerationOptions::none() };
    assert!(empty.validate().expect_err("empty").contains("empty"));

    let long = GenerationOptions {
        stop: Some(vec![String::from("x").repeat(MAX_STOP_SEQUENCE_BYTES + 1)]),
        ..GenerationOptions::none()
    };
    assert!(long.validate().expect_err("long").contains("bytes"));
}

#[test]
fn unsupported_reports_the_first_configured_knob() {
    let sampling = SamplingSupport { temperature: false, top_p: true, top_k: false };
    assert_eq!(
        GenerationOptions { temperature: Some(0.2), ..GenerationOptions::none() }
            .unsupported(sampling),
        Some("temperature")
    );
    assert_eq!(
        GenerationOptions { top_k: Some(10), ..GenerationOptions::none() }.unsupported(sampling),
        Some("top_k")
    );
    assert_eq!(
        GenerationOptions { top_p: Some(0.5), ..GenerationOptions::none() }.unsupported(sampling),
        None
    );
}

#[test]
fn declared_sampling_defaults_unset_flags_to_supported() {
    assert_eq!(SamplingSupport::declared(None, None, None), SamplingSupport::ALL);
    assert_eq!(
        SamplingSupport::declared(Some(false), None, Some(false)),
        SamplingSupport { temperature: false, top_p: true, top_k: false }
    );
}

#[test]
fn serde_round_trip_keeps_unset_knobs_absent() {
    let options = GenerationOptions { seed: Some(11), ..GenerationOptions::none() };
    let encoded = serde_json::to_value(&options).expect("serialize");
    assert_eq!(encoded, serde_json::json!({ "seed": 11 }));
    let decoded: GenerationOptions = serde_json::from_value(encoded).expect("deserialize");
    assert_eq!(decoded, options);
}
