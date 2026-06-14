#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    pub chunk_idx: usize,
    pub text: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: u32,
    pub end_line: u32,
}

pub fn chunk_text(text: &str, chunk_bytes: usize, overlap_bytes: usize) -> Vec<Chunk> {
    if text.is_empty() {
        return Vec::new();
    }
    let chunk_bytes = chunk_bytes.max(1);
    let overlap_bytes = overlap_bytes.min(chunk_bytes.saturating_sub(1));
    let line_starts = line_starts(text);

    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < text.len() {
        start = clamp_char_boundary(text, start);
        let end = clamp_char_boundary(text, (start + chunk_bytes).min(text.len()));
        let body = text[start..end].to_string();
        chunks.push(Chunk {
            chunk_idx: chunks.len(),
            text: body,
            start_byte: start,
            end_byte: end,
            start_line: line_for_offset(&line_starts, start),
            end_line: line_for_offset(&line_starts, end),
        });
        if end == text.len() {
            break;
        }
        let next_start = end.saturating_sub(overlap_bytes);
        start = if next_start <= start { end } else { next_start };
    }

    chunks
}

fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (idx, byte) in text.bytes().enumerate() {
        if byte == b'\n' && idx + 1 < text.len() {
            starts.push(idx + 1);
        }
    }
    starts
}

fn line_for_offset(starts: &[usize], offset: usize) -> u32 {
    match starts.binary_search(&offset) {
        Ok(idx) => idx as u32 + 1,
        Err(idx) => idx as u32,
    }
}

fn clamp_char_boundary(text: &str, mut idx: usize) -> usize {
    idx = idx.min(text.len());
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}
