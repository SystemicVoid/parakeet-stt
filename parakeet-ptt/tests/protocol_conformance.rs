use std::collections::BTreeSet;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

#[allow(dead_code)]
#[path = "../src/protocol.rs"]
mod protocol;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("parakeet-ptt should live under the repository root")
        .to_path_buf()
}

fn fixture_dir() -> PathBuf {
    repo_root().join("docs/protocol/fixtures")
}

fn schema_path() -> PathBuf {
    repo_root().join("docs/protocol/schema/messages.schema.json")
}

fn fixture_paths() -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut paths = fs::read_dir(fixture_dir())?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    paths.retain(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"));
    paths.sort();
    Ok(paths)
}

fn load_json(path: &Path) -> Result<Value, Box<dyn Error>> {
    Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
}

fn message_type(fixture: &Value) -> &str {
    fixture
        .get("type")
        .and_then(Value::as_str)
        .expect("fixture should include a string type")
}

fn is_client_message(message_type: &str) -> bool {
    matches!(
        message_type,
        "start_session" | "stop_session" | "abort_session"
    )
}

fn sample_value_for_ref(reference: &str) -> Value {
    match reference
        .strip_prefix("#/$defs/")
        .expect("schema refs should target local $defs")
    {
        "uuid" | "nullable_uuid" => {
            Value::String("00000000-0000-4000-8000-000000000001".to_string())
        }
        "timestamp" => Value::String("2026-05-18T17:00:00Z".to_string()),
        "nullable_string" => Value::String("value".to_string()),
        "non_negative_integer" | "nullable_non_negative_integer" => Value::from(1),
        "nullable_number" => Value::from(1.5),
        "nullable_boolean" => Value::Bool(true),
        other => panic!("no sample value for schema ref {other}"),
    }
}

fn sample_value_for_type(type_name: &str) -> Value {
    match type_name {
        "string" => Value::String("value".to_string()),
        "integer" => Value::from(1),
        "number" => Value::from(1.5),
        "boolean" => Value::Bool(true),
        other => panic!("no sample value for JSON type {other}"),
    }
}

fn sample_value_for_property(property_schema: &Value) -> Value {
    if let Some(value) = property_schema.get("const") {
        return value.clone();
    }
    if let Some(values) = property_schema.get("enum").and_then(Value::as_array) {
        return values
            .iter()
            .find(|value| !value.is_null())
            .expect("enum should contain at least one non-null sample")
            .clone();
    }
    if let Some(reference) = property_schema.get("$ref").and_then(Value::as_str) {
        return sample_value_for_ref(reference);
    }
    match property_schema.get("type") {
        Some(Value::String(type_name)) => sample_value_for_type(type_name),
        Some(Value::Array(type_names)) => type_names
            .iter()
            .filter_map(Value::as_str)
            .find(|type_name| *type_name != "null")
            .map(sample_value_for_type)
            .expect("nullable type should include a non-null sample"),
        _ => panic!("property schema should declare const, enum, $ref, or type"),
    }
}

fn schema_message_definitions(schema: &Value) -> Vec<(&'static str, &Value)> {
    let defs = schema
        .get("$defs")
        .and_then(Value::as_object)
        .expect("schema should include $defs");
    [
        "StartSession",
        "StopSession",
        "AbortSession",
        "SessionStarted",
        "FinalResult",
        "ErrorMessage",
        "StatusMessage",
        "InterimStateMessage",
        "InterimTextMessage",
        "AudioLevelMessage",
        "SessionEndedMessage",
        "SessionWarningMessage",
    ]
    .into_iter()
    .map(|name| {
        (
            name,
            defs.get(name)
                .unwrap_or_else(|| panic!("schema should include {name}")),
        )
    })
    .collect()
}

fn status_runtime_truth_fields(schema: &Value) -> BTreeSet<String> {
    let groups = schema
        .pointer("/$defs/StatusMessage/x-runtime-truth-field-groups")
        .and_then(Value::as_object)
        .expect("schema should declare StatusMessage runtime truth groups");

    groups
        .values()
        .flat_map(|fields| {
            fields
                .as_array()
                .expect("runtime truth groups should be arrays")
                .iter()
                .map(|field| {
                    field
                        .as_str()
                        .expect("runtime truth fields should be strings")
                        .to_string()
                })
        })
        .collect()
}

fn synthetic_message_from_schema(definition: &Value) -> Value {
    let properties = definition
        .get("properties")
        .and_then(Value::as_object)
        .expect("message schema definition should include properties");
    Value::Object(
        properties
            .iter()
            .map(|(name, property_schema)| {
                (name.clone(), sample_value_for_property(property_schema))
            })
            .collect::<Map<_, _>>(),
    )
}

fn round_trip_through_rust_codec(fixture: Value) -> Result<Value, Box<dyn Error>> {
    if is_client_message(message_type(&fixture)) {
        let decoded: protocol::ClientMessage = serde_json::from_value(fixture)?;
        Ok(serde_json::to_value(decoded)?)
    } else {
        let decoded: protocol::ServerMessage = serde_json::from_value(fixture)?;
        Ok(serde_json::to_value(decoded)?)
    }
}

#[test]
fn protocol_conformance_status_runtime_truth_fields_round_trip() -> Result<(), Box<dyn Error>> {
    let schema = load_json(&schema_path())?;
    let status_definition = schema
        .pointer("/$defs/StatusMessage")
        .expect("schema should include StatusMessage");
    let synthetic = synthetic_message_from_schema(status_definition);
    let encoded = round_trip_through_rust_codec(synthetic)?;
    let actual = encoded
        .as_object()
        .expect("encoded status should be an object")
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();

    assert_eq!(actual, status_runtime_truth_fields(&schema));
    Ok(())
}

#[test]
fn protocol_conformance_fixtures_have_expected_inventory() -> Result<(), Box<dyn Error>> {
    let expected = BTreeSet::from([
        "client_abort_session.json".to_string(),
        "client_start_session.json".to_string(),
        "client_stop_session.json".to_string(),
        "server_audio_level.json".to_string(),
        "server_error.json".to_string(),
        "server_final_result.json".to_string(),
        "server_interim_state.json".to_string(),
        "server_interim_text.json".to_string(),
        "server_session_ended.json".to_string(),
        "server_session_started.json".to_string(),
        "server_session_warning.json".to_string(),
        "status_default.json".to_string(),
        "status_offline.json".to_string(),
        "status_stream_fallback.json".to_string(),
        "status_vad_fallback.json".to_string(),
    ]);
    let actual = fixture_paths()?
        .iter()
        .map(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .expect("fixture name should be UTF-8")
                .to_string()
        })
        .collect::<BTreeSet<_>>();

    assert_eq!(actual, expected);
    Ok(())
}

#[test]
fn protocol_conformance_fixtures_validate_against_schema() -> Result<(), Box<dyn Error>> {
    let schema = load_json(&schema_path())?;
    jsonschema::meta::validate(&schema)
        .unwrap_or_else(|error| panic!("canonical schema is invalid: {error}"));
    let validator = jsonschema::validator_for(&schema)?;

    for path in fixture_paths()? {
        let fixture = load_json(&path)?;
        validator
            .validate(&fixture)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    }

    Ok(())
}

#[test]
fn protocol_conformance_fixtures_round_trip_through_rust_codecs() -> Result<(), Box<dyn Error>> {
    for path in fixture_paths()? {
        let fixture = load_json(&path)?;
        let encoded = round_trip_through_rust_codec(fixture.clone())?;

        assert_eq!(encoded, fixture, "{}", path.display());
    }

    Ok(())
}

#[test]
fn protocol_conformance_schema_synthesized_messages_round_trip() -> Result<(), Box<dyn Error>> {
    let schema = load_json(&schema_path())?;
    for (name, definition) in schema_message_definitions(&schema) {
        let synthetic = synthetic_message_from_schema(definition);
        let encoded = round_trip_through_rust_codec(synthetic.clone())?;

        assert_eq!(encoded, synthetic, "{name}");
    }

    Ok(())
}
