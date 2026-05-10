use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use xi_rope::Rope;
use xi_rope::rope::Utf16CodeUnitsMetric;

const MAX_LEAF: usize = 1024;
const MAX_CHILDREN: usize = 8;
const TARGET_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Default)]
struct BenchMeta {
    len: usize,
    lines: usize,
    utf16_size: usize,
}

impl BenchMeta {
    fn for_text(text: &str) -> Self {
        Self {
            len: text.len(),
            lines: bytecount::count(text.as_bytes(), b'\n'),
            utf16_size: text.encode_utf16().count(),
        }
    }

    fn accumulate(&mut self, other: Self) {
        self.len += other.len;
        self.lines += other.lines;
        self.utf16_size += other.utf16_size;
    }
}

enum ParentSideNode {
    Leaf { text: String, meta: BenchMeta },
    Internal { children: Vec<ParentSideNode>, child_meta: Vec<BenchMeta>, meta: BenchMeta },
}

impl ParentSideNode {
    fn meta(&self) -> BenchMeta {
        match self {
            Self::Leaf { meta, .. } | Self::Internal { meta, .. } => *meta,
        }
    }
}

struct ParentSideRope {
    root: ParentSideNode,
}

impl ParentSideRope {
    fn from_text(text: &str) -> Self {
        let mut nodes = split_leaves(text)
            .map(|leaf| ParentSideNode::Leaf { meta: BenchMeta::for_text(&leaf), text: leaf })
            .collect::<Vec<_>>();

        while nodes.len() > 1 {
            let mut parents = Vec::with_capacity(nodes.len().div_ceil(MAX_CHILDREN));
            for chunk in nodes.chunks(MAX_CHILDREN) {
                let children = chunk.iter().map(clone_node_for_bench).collect::<Vec<_>>();
                let child_meta = children.iter().map(ParentSideNode::meta).collect::<Vec<_>>();
                let mut meta = BenchMeta::default();
                for child in &child_meta {
                    meta.accumulate(*child);
                }
                parents.push(ParentSideNode::Internal { children, child_meta, meta });
            }
            nodes = parents;
        }

        Self {
            root: nodes.pop().unwrap_or_else(|| ParentSideNode::Leaf {
                text: String::new(),
                meta: BenchMeta::default(),
            }),
        }
    }

    fn offset_of_line(&self, line: usize) -> usize {
        offset_of_line_in_node(&self.root, line)
    }

    fn line_of_offset(&self, offset: usize) -> usize {
        line_of_offset_in_node(&self.root, offset)
    }

    fn offset_of_utf16(&self, utf16_offset: usize) -> usize {
        offset_of_utf16_in_node(&self.root, utf16_offset)
    }

    fn utf16_of_offset(&self, offset: usize) -> usize {
        utf16_of_offset_in_node(&self.root, offset)
    }
}

fn clone_node_for_bench(node: &ParentSideNode) -> ParentSideNode {
    match node {
        ParentSideNode::Leaf { text, meta } => {
            ParentSideNode::Leaf { text: text.clone(), meta: *meta }
        }
        ParentSideNode::Internal { children, child_meta, meta } => ParentSideNode::Internal {
            children: children.iter().map(clone_node_for_bench).collect(),
            child_meta: child_meta.clone(),
            meta: *meta,
        },
    }
}

fn offset_of_line_in_node(node: &ParentSideNode, mut line: usize) -> usize {
    match node {
        ParentSideNode::Leaf { text, meta } => {
            if line > meta.lines {
                return meta.len;
            }
            let mut offset = 0;
            for _ in 0..line {
                match memchr::memchr(b'\n', &text.as_bytes()[offset..]) {
                    Some(pos) => offset += pos + 1,
                    None => return meta.len,
                }
            }
            offset
        }
        ParentSideNode::Internal { children, child_meta, meta } => {
            if line > meta.lines {
                return meta.len;
            }
            let mut byte_offset = 0;
            for (child, child_meta) in children.iter().zip(child_meta) {
                if line <= child_meta.lines {
                    return byte_offset + offset_of_line_in_node(child, line);
                }
                line -= child_meta.lines;
                byte_offset += child_meta.len;
            }
            meta.len
        }
    }
}

fn line_of_offset_in_node(node: &ParentSideNode, offset: usize) -> usize {
    match node {
        ParentSideNode::Leaf { text, meta } => {
            bytecount::count(&text.as_bytes()[..offset.min(meta.len)], b'\n')
        }
        ParentSideNode::Internal { children, child_meta, meta } => {
            let mut remaining = offset.min(meta.len);
            let mut lines = 0;
            for (child, child_meta) in children.iter().zip(child_meta) {
                if remaining <= child_meta.len {
                    return lines + line_of_offset_in_node(child, remaining);
                }
                remaining -= child_meta.len;
                lines += child_meta.lines;
            }
            lines
        }
    }
}

fn offset_of_utf16_in_node(node: &ParentSideNode, mut utf16_offset: usize) -> usize {
    match node {
        ParentSideNode::Leaf { text, meta } => {
            if utf16_offset >= meta.utf16_size {
                return meta.len;
            }
            let mut utf16_count = 0;
            for (byte_offset, ch) in text.char_indices() {
                if utf16_count >= utf16_offset {
                    return byte_offset;
                }
                utf16_count += ch.len_utf16();
            }
            meta.len
        }
        ParentSideNode::Internal { children, child_meta, meta } => {
            if utf16_offset >= meta.utf16_size {
                return meta.len;
            }
            let mut byte_offset = 0;
            for (child, child_meta) in children.iter().zip(child_meta) {
                if utf16_offset <= child_meta.utf16_size {
                    return byte_offset + offset_of_utf16_in_node(child, utf16_offset);
                }
                utf16_offset -= child_meta.utf16_size;
                byte_offset += child_meta.len;
            }
            meta.len
        }
    }
}

fn utf16_of_offset_in_node(node: &ParentSideNode, offset: usize) -> usize {
    match node {
        ParentSideNode::Leaf { text, meta } => text[..offset.min(meta.len)].encode_utf16().count(),
        ParentSideNode::Internal { children, child_meta, meta } => {
            let mut remaining = offset.min(meta.len);
            let mut utf16_size = 0;
            for (child, child_meta) in children.iter().zip(child_meta) {
                if remaining <= child_meta.len {
                    return utf16_size + utf16_of_offset_in_node(child, remaining);
                }
                remaining -= child_meta.len;
                utf16_size += child_meta.utf16_size;
            }
            utf16_size
        }
    }
}

fn split_leaves(text: &str) -> impl Iterator<Item = String> + '_ {
    let mut start = 0;
    std::iter::from_fn(move || {
        if start >= text.len() {
            return None;
        }
        let mut end = (start + MAX_LEAF).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = text[start..].char_indices().nth(1).map_or(text.len(), |(idx, _)| start + idx);
        }
        let leaf = text[start..end].to_owned();
        start = end;
        Some(leaf)
    })
}

fn repeat_to_target(seed: &str, target_bytes: usize) -> String {
    let mut text = String::with_capacity(target_bytes + seed.len());
    while text.len() + seed.len() <= target_bytes {
        text.push_str(seed);
    }
    if text.len() < target_bytes {
        let mut end = target_bytes - text.len();
        while end > 0 && !seed.is_char_boundary(end) {
            end -= 1;
        }
        text.push_str(&seed[..end]);
    }
    text
}

fn fixture_mixed_text() -> String {
    repeat_to_target(
        "fn alpha() { println!(\"hello\"); }\r\nplain line 🙂 with utf16\n",
        TARGET_BYTES,
    )
}

fn query_points(total: usize) -> Vec<usize> {
    [0, total / 16, total / 7, total / 3, total / 2, total * 2 / 3, total * 6 / 7, total]
        .into_iter()
        .collect()
}

fn byte_query_points(text: &str) -> Vec<usize> {
    let mut points = query_points(text.len());
    for point in &mut points {
        while *point > 0 && !text.is_char_boundary(*point) {
            *point -= 1;
        }
    }
    points.dedup();
    points
}

fn bench_parent_metadata(c: &mut Criterion) {
    let text = fixture_mixed_text();
    let rope = Rope::from(text.as_str());
    let parent_side = ParentSideRope::from_text(&text);
    let line_points = query_points(rope.measure::<xi_rope::LinesMetric>());
    let byte_points = byte_query_points(&text);
    let utf16_points = query_points(rope.measure::<Utf16CodeUnitsMetric>());

    let mut group = c.benchmark_group("rope_metadata_layout");
    group.throughput(Throughput::Bytes(text.len() as u64));

    group.bench_with_input(
        BenchmarkId::new("current_node_metadata", "offset_of_line"),
        &line_points,
        |b, points| {
            b.iter(|| {
                for point in points {
                    black_box(rope.offset_of_line(black_box(*point)));
                }
            })
        },
    );
    group.bench_with_input(
        BenchmarkId::new("parent_side_child_metadata", "offset_of_line"),
        &line_points,
        |b, points| {
            b.iter(|| {
                for point in points {
                    black_box(parent_side.offset_of_line(black_box(*point)));
                }
            })
        },
    );
    group.bench_with_input(
        BenchmarkId::new("current_node_metadata", "line_of_offset"),
        &byte_points,
        |b, points| {
            b.iter(|| {
                for point in points {
                    black_box(rope.line_of_offset(black_box(*point)));
                }
            })
        },
    );
    group.bench_with_input(
        BenchmarkId::new("parent_side_child_metadata", "line_of_offset"),
        &byte_points,
        |b, points| {
            b.iter(|| {
                for point in points {
                    black_box(parent_side.line_of_offset(black_box(*point)));
                }
            })
        },
    );
    group.bench_with_input(
        BenchmarkId::new("current_node_metadata", "offset_of_utf16"),
        &utf16_points,
        |b, points| {
            b.iter(|| {
                for point in points {
                    black_box(rope.count_base_units::<Utf16CodeUnitsMetric>(black_box(*point)));
                }
            })
        },
    );
    group.bench_with_input(
        BenchmarkId::new("parent_side_child_metadata", "offset_of_utf16"),
        &utf16_points,
        |b, points| {
            b.iter(|| {
                for point in points {
                    black_box(parent_side.offset_of_utf16(black_box(*point)));
                }
            })
        },
    );
    group.bench_with_input(
        BenchmarkId::new("current_node_metadata", "utf16_of_offset"),
        &byte_points,
        |b, points| {
            b.iter(|| {
                for point in points {
                    black_box(rope.count::<Utf16CodeUnitsMetric>(black_box(*point)));
                }
            })
        },
    );
    group.bench_with_input(
        BenchmarkId::new("parent_side_child_metadata", "utf16_of_offset"),
        &byte_points,
        |b, points| {
            b.iter(|| {
                for point in points {
                    black_box(parent_side.utf16_of_offset(black_box(*point)));
                }
            })
        },
    );

    group.finish();
}

criterion_group!(benches, bench_parent_metadata);
criterion_main!(benches);
