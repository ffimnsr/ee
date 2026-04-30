use std::collections::hash_map::DefaultHasher;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, OnceLock};
use std::thread;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::{LookupSpan, Registry};

static TRACE_STATE: LazyLock<Arc<TraceState>> = LazyLock::new(|| Arc::new(TraceState::default()));
static TRACE_INSTALL: OnceLock<()> = OnceLock::new();

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TraceRecord {
    pub name: String,
    pub cat: String,
    pub ph: String,
    pub ts: u64,
    pub pid: u32,
    pub tid: u64,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub args: Map<String, Value>,
}

#[derive(Default)]
struct TraceState {
    enabled: AtomicBool,
    records: Mutex<Vec<TraceRecord>>,
}

#[derive(Clone)]
struct RuntimeTraceLayer {
    state: Arc<TraceState>,
}

#[derive(Clone, Debug, Default)]
struct SpanTraceData {
    name: String,
    categories: String,
    fields: Map<String, Value>,
}

#[derive(Default)]
struct JsonVisitor {
    fields: Map<String, Value>,
}

impl RuntimeTraceLayer {
    fn new(state: Arc<TraceState>) -> Self {
        Self { state }
    }
}

impl TraceState {
    fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
        if !enabled {
            self.lock_records().clear();
        }
    }

    fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    fn push_record(&self, record: TraceRecord) {
        self.lock_records().push(record);
    }

    fn collect(&self) -> Vec<TraceRecord> {
        let mut records = self.lock_records().clone();
        records.sort_by_key(|record| (record.ts, phase_order(&record.ph)));
        records
    }

    fn lock_records(&self) -> MutexGuard<'_, Vec<TraceRecord>> {
        self.records.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Visit for JsonVisitor {
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields.insert(field.name().to_owned(), Value::Bool(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields.insert(field.name().to_owned(), Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields.insert(field.name().to_owned(), Value::from(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.fields.insert(field.name().to_owned(), Value::from(value));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields.insert(field.name().to_owned(), Value::String(value.to_owned()));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.fields.insert(field.name().to_owned(), Value::String(value.to_string()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields.insert(field.name().to_owned(), Value::String(format!("{:?}", value)));
    }
}

impl<S> Layer<S> for RuntimeTraceLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn enabled(&self, _metadata: &tracing::Metadata<'_>, _ctx: Context<'_, S>) -> bool {
        self.state.is_enabled()
    }

    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut visitor = JsonVisitor::default();
        attrs.record(&mut visitor);
        let categories = take_categories(&mut visitor.fields);
        let data = SpanTraceData {
            name: attrs.metadata().name().to_owned(),
            categories,
            fields: visitor.fields,
        };

        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(data);
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            let mut visitor = JsonVisitor::default();
            values.record(&mut visitor);
            let mut extensions = span.extensions_mut();
            if let Some(data) = extensions.get_mut::<SpanTraceData>() {
                data.fields.extend(visitor.fields);
                let categories = take_categories(&mut data.fields);
                if !categories.is_empty() {
                    data.categories = categories;
                }
            }
        }
    }

    fn on_enter(&self, id: &Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            let extensions = span.extensions();
            if let Some(data) = extensions.get::<SpanTraceData>() {
                self.state.push_record(TraceRecord {
                    name: data.name.clone(),
                    cat: data.categories.clone(),
                    ph: "B".to_owned(),
                    ts: timestamp_micros(),
                    pid: std::process::id(),
                    tid: thread_id_u64(),
                    args: data.fields.clone(),
                });
            }
        }
    }

    fn on_exit(&self, id: &Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            let extensions = span.extensions();
            if let Some(data) = extensions.get::<SpanTraceData>() {
                self.state.push_record(TraceRecord {
                    name: data.name.clone(),
                    cat: data.categories.clone(),
                    ph: "E".to_owned(),
                    ts: timestamp_micros(),
                    pid: std::process::id(),
                    tid: thread_id_u64(),
                    args: Map::new(),
                });
            }
        }
    }

    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = JsonVisitor::default();
        event.record(&mut visitor);
        let categories = take_categories(&mut visitor.fields);
        self.state.push_record(TraceRecord {
            name: event.metadata().name().to_owned(),
            cat: categories,
            ph: "i".to_owned(),
            ts: timestamp_micros(),
            pid: std::process::id(),
            tid: thread_id_u64(),
            args: visitor.fields,
        });
    }
}

pub fn install() {
    TRACE_INSTALL.get_or_init(|| {
        let subscriber = Registry::default().with(RuntimeTraceLayer::new(TRACE_STATE.clone()));
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

pub fn set_enabled(enabled: bool) {
    install();
    TRACE_STATE.set_enabled(enabled);
}

pub fn is_enabled() -> bool {
    TRACE_STATE.is_enabled()
}

pub fn collect() -> Vec<TraceRecord> {
    TRACE_STATE.collect()
}

pub fn collect_json() -> Result<Value, serde_json::Error> {
    serde_json::to_value(collect())
}

pub fn decode_json(value: Value) -> Result<Vec<TraceRecord>, serde_json::Error> {
    serde_json::from_value(value)
}

pub fn write_json<W: Write>(records: &[TraceRecord], writer: W) -> Result<(), serde_json::Error> {
    serde_json::to_writer(writer, records)
}

pub fn save_to_file<P: AsRef<Path>>(
    path: P,
    frontend_samples: Value,
    plugin_samples: impl IntoIterator<Item = Value>,
) -> Result<(), SaveTraceError> {
    let mut records = collect();
    extend_records(&mut records, frontend_samples).map_err(SaveTraceError::Json)?;
    for plugin_sample in plugin_samples {
        extend_records(&mut records, plugin_sample).map_err(SaveTraceError::Json)?;
    }

    let trace_file = File::create(path.as_ref()).map_err(SaveTraceError::Io)?;
    write_json(&records, trace_file).map_err(SaveTraceError::Json)
}

#[derive(Debug)]
pub enum SaveTraceError {
    Io(std::io::Error),
    Json(serde_json::Error),
}

fn extend_records(records: &mut Vec<TraceRecord>, value: Value) -> Result<(), serde_json::Error> {
    let mut decoded = decode_json(value)?;
    records.append(&mut decoded);
    records.sort_by_key(|record| (record.ts, phase_order(&record.ph)));
    Ok(())
}

fn take_categories(fields: &mut Map<String, Value>) -> String {
    fields
        .remove("categories")
        .or_else(|| fields.remove("category"))
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_default()
}

fn timestamp_micros() -> u64 {
    let now =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    now.as_micros() as u64
}

fn thread_id_u64() -> u64 {
    let mut hasher = DefaultHasher::new();
    thread::current().id().hash(&mut hasher);
    hasher.finish()
}

fn phase_order(phase: &str) -> u8 {
    match phase {
        "B" => 0,
        "i" => 1,
        "E" => 2,
        _ => 3,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn disabled_tracing_drops_records() {
        let _guard = TEST_LOCK.lock().unwrap();
        set_enabled(false);
        tracing::trace!(name: "disabled-event", categories = "test");
        assert!(collect().is_empty());
    }

    #[test]
    fn enabled_tracing_records_events_and_spans() {
        let _guard = TEST_LOCK.lock().unwrap();
        set_enabled(false);
        set_enabled(true);

        tracing::trace!(name: "trace-event", categories = "test", answer = 42_u64);
        {
            let _span =
                tracing::trace_span!("trace-span", categories = "test", token = 7).entered();
        }

        let records = collect();
        assert!(records.iter().any(|record| record.name == "trace-event" && record.ph == "i"));
        assert!(records.iter().any(|record| record.name == "trace-span" && record.ph == "B"));
        assert!(records.iter().any(|record| record.name == "trace-span" && record.ph == "E"));
        set_enabled(false);
    }
}
