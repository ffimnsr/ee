//! `update` wire payload: line cache ops, interned syntax scopes, decoding.
//!
//! Split out of `backend.rs` (which is over the repository 1000 LOC bar) so the
//! payload shape, its validation, and the op-to-cache application live in one
//! place. `backend.rs` re-exports these names, so `crate::backend::CoreUpdate`
//! and friends keep working.

use std::io;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use super::{CachedLine, LineSlot, normalize_line_text, strip_line_ending};

/// Trailing bytes [`normalize_line_text`] can strip from served line text (LF or
/// CRLF). A span may legitimately end on those bytes, so a span *end* past the
/// line is clipped rather than rejected; a span *start* past the line plus this
/// slack cannot describe anything the frontend could style and rejects.
const TRAILING_LINE_ENDING_BYTES: usize = 2;

#[derive(Debug, Deserialize)]
pub(crate) struct CoreNotificationParams {
    pub(crate) view_id: String,
    pub(crate) update: CoreUpdate,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(crate) struct CoreUpdate {
    pub(crate) ops: Vec<CoreUpdateOp>,
    pub(crate) pristine: bool,
    #[serde(default)]
    pub(crate) annotations: Vec<CoreAnnotation>,
    /// VLF-only line-count metadata (Stage A Phase 3); absent on rope updates.
    #[serde(default)]
    pub(crate) vlf_total_lines: Option<VlfTotalLines>,
    /// Interned syntax scope names for this update. Lines carry scope ids in
    /// their flat `spans` arrays; the table travels once per payload.
    #[serde(default)]
    pub(crate) scopes: Vec<String>,
    /// Binary text frame carried alongside this payload, when any row used the
    /// blob carrier. Read from the frame that follows the notification and
    /// attached before the update is applied; never part of the JSON.
    #[serde(skip)]
    pub(crate) blob: Option<Arc<[u8]>>,
}

impl CoreUpdate {
    /// Whether any op takes its rows from the binary text frame.
    pub(crate) fn declares_blob(&self) -> bool {
        self.ops.iter().any(|op| op.blob)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
pub(crate) struct VlfTotalLines {
    pub(crate) count: u64,
    pub(crate) exact: bool,
    #[serde(default)]
    pub(crate) index_progress: f64,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub(crate) struct CoreAnnotation {
    #[serde(rename = "type")]
    pub(crate) annotation_type: String,
    #[serde(default)]
    pub(crate) ranges: Vec<[usize; 4]>,
    #[serde(default)]
    pub(crate) payloads: Option<Vec<Value>>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct CoreUpdateOp {
    pub(crate) op: CoreUpdateKind,
    pub(crate) n: usize,
    #[serde(default)]
    pub(crate) lines: Vec<CoreLine>,
    /// Set when this op's rows take their text from the update's binary text frame:
    /// a row without `text` consumes the next frame row, in order.
    #[serde(default)]
    pub(crate) blob: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CoreUpdateKind {
    #[serde(rename = "ins")]
    Insert,
    Skip,
    Invalidate,
    Copy,
    Update,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub(crate) struct CoreLine {
    #[serde(default)]
    pub(crate) text: Option<String>,
    #[serde(default)]
    pub(crate) cursor: Vec<usize>,
    /// Flat `[start, end, scope_id, ..]` triples into [`CoreUpdate::scopes`].
    /// Absent means "this op says nothing about spans"; an empty array clears
    /// them.
    #[serde(default)]
    pub(crate) spans: Option<Vec<u32>>,
    /// Logical line number of this visual row (0-based); `None` on wrapped
    /// continuation rows, present only on the first row of each logical line.
    #[serde(default, rename = "ln")]
    pub(crate) logical_line: Option<usize>,
}

/// A decoded span. Spans are produced by the backend and consumed by
/// `highlight.rs`; nothing deserializes this shape any more — the wire carries
/// flat integer triples plus the update's scope table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CoreSyntaxSpan {
    pub(crate) start_byte: usize,
    pub(crate) end_byte: usize,
    pub(crate) scope: Arc<str>,
}

/// Resolves one payload's interned scope names into shared strings.
///
/// Every span that references a name shares a single `Arc<str>`, so decoding
/// allocates per distinct scope rather than per span.
pub(crate) fn scope_table(names: &[String]) -> Vec<Arc<str>> {
    names.iter().map(|name| Arc::from(name.as_str())).collect()
}

/// Decodes flat span triples against the update's interned scope table.
///
/// `line_len` is the length of the line text the frontend will style (trailing
/// line ending already stripped).
///
/// Fails closed: a misaligned array, an inverted range, a triple that starts
/// before the previous one ended (the highlighter walks forward-only, so such a
/// triple would duplicate text), a span start past the served line, or an
/// out-of-range scope id rejects the update instead of dropping or mis-styling
/// spans.
pub(crate) fn decode_spans(
    flat: &[u32],
    scopes: &[Arc<str>],
    line_len: usize,
) -> io::Result<Vec<CoreSyntaxSpan>> {
    if !flat.len().is_multiple_of(3) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("spans array is not [start, end, scope_id] triples: {} entries", flat.len()),
        ));
    }
    let mut spans = Vec::with_capacity(flat.len() / 3);
    let mut cursor = 0usize;
    for triple in flat.chunks_exact(3) {
        let (start, end, scope_id) = (triple[0] as usize, triple[1] as usize, triple[2]);
        if end < start {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("span range inverted: {start}..{end}"),
            ));
        }
        if start > line_len.saturating_add(TRAILING_LINE_ENDING_BYTES) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("span start {start} beyond line length {line_len}"),
            ));
        }
        // The id is validated before zero-width spans are dropped, so a malformed
        // table cannot hide behind an empty range.
        let Some(scope) = scopes.get(scope_id as usize) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("span scope id {scope_id} out of range ({})", scopes.len()),
            ));
        };
        let end = end.min(line_len);
        if end <= start {
            continue;
        }
        if start < cursor {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("span range overlaps the previous span: {start} < {cursor}"),
            ));
        }
        cursor = end;
        spans.push(CoreSyntaxSpan { start_byte: start, end_byte: end, scope: Arc::clone(scope) });
    }
    Ok(spans)
}

/// Row bytes of a payload's text frame, in wire order.
///
/// Rows are handed out positionally: an op that sets its `blob` flag consumes one
/// row per line that does not carry `text`, in op order. Nothing is indexed by
/// offset, so a malformed frame cannot make one row read another's bytes.
#[derive(Debug, Default)]
pub(crate) struct BlobRows<'a> {
    rows: Vec<&'a [u8]>,
    next: usize,
}

impl<'a> BlobRows<'a> {
    /// Decodes the frame, failing closed on any header/payload disagreement.
    pub(crate) fn decode(frame: &'a [u8]) -> io::Result<Self> {
        let rows = xi_core_lib::text_blob::decode_frame(frame)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        Ok(BlobRows { rows, next: 0 })
    }

    /// Bytes for one row, consuming the next frame row when `takes_bytes`.
    fn take(&mut self, takes_bytes: bool) -> io::Result<Option<&'a [u8]>> {
        if !takes_bytes {
            return Ok(None);
        }
        let row = self.rows.get(self.next).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("text frame has {} rows but the payload references more", self.rows.len()),
            )
        })?;
        self.next += 1;
        Ok(Some(row))
    }
}

/// Validates a payload's text-frame usage before anything is committed, returning
/// the decoded rows for the appliers to consume in the same order.
///
/// Fail closed on every rule the carrier has: an op may only set `blob` when the
/// update carried a frame, every row of such an op must resolve to text (either its
/// own `text` or one frame row — a row with neither cannot mean "keep previous"
/// inside a blob op), each frame row must be valid UTF-8, and the frame must be
/// consumed exactly.
pub(crate) fn validate_blob_ops<'a>(
    ops: &[CoreUpdateOp],
    frame: Option<&'a [u8]>,
) -> io::Result<BlobRows<'a>> {
    let declares = ops.iter().any(|op| op.blob);
    if !declares {
        return Ok(BlobRows::default());
    }
    let Some(frame) = frame else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "update declares a text frame but none was carried",
        ));
    };
    let mut rows = BlobRows::decode(frame)?;

    for op in ops {
        if !op.blob {
            continue;
        }
        for line in &op.lines {
            let bytes = rows.take(line.text.is_none())?;
            if let Some(bytes) = bytes {
                std::str::from_utf8(bytes).map_err(|err| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("text frame row is not UTF-8: {err}"),
                    )
                })?;
            } else if line.text.is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "blob op has a row with neither `text` nor a frame row",
                ));
            }
        }
    }

    if rows.next != rows.rows.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "text frame has {} rows but the payload consumed {}",
                rows.rows.len(),
                rows.next
            ),
        ));
    }
    rows.next = 0;
    Ok(rows)
}

/// Text for one wire row.
///
/// `Ok(None)` means this op says nothing about the row's text (the frontend keeps
/// what it already has); `Ok(Some(_))` is the row's raw text, still carrying any
/// trailing line ending, exactly like the per-line carrier.
pub(crate) fn row_text(
    text: Option<String>,
    bytes: Option<&[u8]>,
    op: &str,
) -> io::Result<Option<String>> {
    match (text, bytes) {
        (Some(_), Some(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{op}: line carries both `text` and a frame row"),
        )),
        (Some(text), None) => Ok(Some(text)),
        (None, Some(bytes)) => {
            let text = std::str::from_utf8(bytes).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{op}: text frame row is not UTF-8: {err}"),
                )
            })?;
            Ok(Some(text.to_owned()))
        }
        (None, None) => Ok(None),
    }
}

/// Consumes the frame rows for one op's lines, in order.
///
/// Used by the appliers after [`validate_blob_ops`] has accepted the payload, so a
/// row's bytes are already known to exist and be UTF-8.
pub(crate) fn take_op_rows<'a>(
    rows: &mut BlobRows<'a>,
    op: &CoreUpdateOp,
) -> Vec<Option<&'a [u8]>> {
    op.lines
        .iter()
        .map(|line| if op.blob { rows.take(line.text.is_none()).ok().flatten() } else { None })
        .collect()
}

impl LineSlot {
    /// Builds a slot from one wire line, resolving span scope ids against the
    /// update's interned table and the row's text against the payload's frame.
    pub(crate) fn from_core_line(
        line: CoreLine,
        scopes: &[Arc<str>],
        bytes: Option<&[u8]>,
    ) -> io::Result<Self> {
        let CoreLine { text, cursor, spans, logical_line } = line;
        let text = row_text(text, bytes, "insert")?;
        let line_len = text.as_deref().map_or(0, |text| strip_line_ending(text).len());
        let syntax_spans = match spans {
            Some(flat) => decode_spans(&flat, scopes, line_len)?,
            None => Vec::new(),
        };
        Ok(LineSlot::Known(CachedLine {
            text: normalize_line_text(text),
            cursors: cursor,
            syntax_spans,
            logical_line,
        }))
    }

    pub(crate) fn merge(
        self,
        update: CoreLine,
        scopes: &[Arc<str>],
        bytes: Option<&[u8]>,
    ) -> io::Result<Self> {
        match self {
            LineSlot::Known(mut line) => {
                let CoreLine { text, cursor, spans, logical_line } = update;
                let text = row_text(text, bytes, "update")?;
                // Spans are offsets into the line text the frontend will style, so
                // the bound follows the text this op serves: the new text when the
                // op carries one, otherwise the text already cached.
                let line_len = match &text {
                    Some(text) => strip_line_ending(text).len(),
                    None => line.text.len(),
                };
                if let Some(text) = text {
                    line.text = normalize_line_text(Some(text));
                }
                line.cursors = cursor;
                // Absent spans mean this op says nothing about them; present
                // spans (including an empty array) replace the line's spans.
                if let Some(spans) = spans {
                    line.syntax_spans = decode_spans(&spans, scopes, line_len)?;
                }
                if logical_line.is_some() {
                    line.logical_line = logical_line;
                }
                Ok(LineSlot::Known(line))
            }
            LineSlot::Invalid => {
                if update.text.is_none() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "update op cannot patch invalid line without text",
                    ));
                }
                LineSlot::from_core_line(update, scopes, bytes)
            }
        }
    }
}
