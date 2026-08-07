/// Reusable tokenizer scratch. Keeping the normalization buffer across fields
/// avoids allocating one concatenated synthetic document for every graph node.
#[derive(Default)]
pub struct Tokenizer {
    normalized: String,
}

impl Tokenizer {
    pub fn tokenize_into(&mut self, input: &str, output: &mut Vec<String>) {
        self.normalized.clear();
        let wanted = input.len().saturating_add(input.len() / 8);
        if self.normalized.capacity() < wanted {
            self.normalized.reserve(wanted);
        }

        normalize_into(input, &mut self.normalized);
        output.extend(
            self.normalized
                .split_whitespace()
                .filter(|token| token.len() > 1)
                .map(str::to_string),
        );
    }
}

pub fn tokenize(input: &str) -> Vec<String> {
    let mut tokenizer = Tokenizer::default();
    let mut tokens = Vec::new();
    tokenizer.tokenize_into(input, &mut tokens);
    tokens
}

pub fn tokenize_into(input: &str, output: &mut Vec<String>) {
    Tokenizer::default().tokenize_into(input, output);
}

fn normalize_into(input: &str, normalized: &mut String) {
    let mut prev: Option<char> = None;
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch.is_ascii_alphanumeric() {
            let next = chars.peek().copied();
            if let Some(prev_ch) = prev {
                if is_camel_boundary(prev_ch, ch, next) {
                    normalized.push(' ');
                }
            }
            normalized.push(ch.to_ascii_lowercase());
            prev = Some(ch);
        } else {
            normalized.push(' ');
            prev = None;
        }
    }
}

fn is_camel_boundary(prev: char, current: char, next: Option<char>) -> bool {
    // Split lower/digit→upper (`ownerService`) and acronym→word (`HTTPServer`).
    if current.is_ascii_uppercase() && (prev.is_ascii_lowercase() || prev.is_ascii_digit()) {
        return true;
    }
    current.is_ascii_uppercase()
        && prev.is_ascii_uppercase()
        && next
            .map(|next_ch| next_ch.is_ascii_lowercase())
            .unwrap_or(false)
}
