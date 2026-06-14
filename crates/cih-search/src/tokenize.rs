pub fn tokenize(input: &str) -> Vec<String> {
    let mut normalized = String::with_capacity(input.len() + input.len() / 8);
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

    normalized
        .split_whitespace()
        .filter(|token| token.len() > 1)
        .map(str::to_string)
        .collect()
}

fn is_camel_boundary(prev: char, current: char, next: Option<char>) -> bool {
    if current.is_ascii_uppercase() && (prev.is_ascii_lowercase() || prev.is_ascii_digit()) {
        return true;
    }
    current.is_ascii_uppercase()
        && prev.is_ascii_uppercase()
        && next
            .map(|next_ch| next_ch.is_ascii_lowercase())
            .unwrap_or(false)
}
