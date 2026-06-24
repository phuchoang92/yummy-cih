/// Language-agnostic MinHash body fingerprint shared across Java, Python, and TypeScript.
///
/// Each language maps its tree-sitter leaf node kinds to a small alphabet
/// (I=identifier, S=string, N=number, T=type, K=keyword, O=other) so that
/// structurally similar functions across a codebase produce similar fingerprints
/// regardless of identifier or literal choices.
use cih_core::BodyFingerprint;
use tree_sitter::Node as TsNode;

pub(crate) const MINHASH_K: usize = 64;
pub(crate) const MINHASH_MIN_TOKENS: u32 = 30;

pub(crate) const MINHASH_SEEDS: [u64; 64] = [
    0x9e3779b97f4a7c15, 0x6c62272e07bb0142, 0x94d049bb133111eb, 0xbf58476d1ce4e5b9,
    0x517cc1b727220a95, 0x4be98134a5976fd3, 0xa8c2fda8d01bcc3d, 0x0bc150392e34b12b,
    0x3917bfda55c5b0a3, 0x7465fca80eceed01, 0xf17b2e68e95b4b63, 0x2e52d1d0c50a8471,
    0xd97390e8e9cbb87b, 0x2abe1a8a8c8a1e2b, 0x6d2ee4a3c3cfb9c5, 0x84f8e5a9c8c7f5a1,
    0x3c7f5d1b8e6a2f4c, 0xa1b2c3d4e5f60718, 0x192a3b4c5d6e7f80, 0x8f7e6d5c4b3a2918,
    0xdeadbeefcafebabe, 0x0102030405060708, 0xf0e0d0c0b0a09080, 0x123456789abcdef0,
    0xfedcba9876543210, 0xa5a5a5a5a5a5a5a5, 0x5a5a5a5a5a5a5a5a, 0xc3c3c3c3c3c3c3c3,
    0x3c3c3c3c3c3c3c3c, 0xe7e7e7e7e7e7e7e7, 0x1818181818181818, 0xaaaa0000bbbb1111,
    0xcccc2222dddd3333, 0xeeee4444ffff5555, 0x0000666677778888, 0x9999aaaabbbbcccc,
    0xddddeeeeffff0000, 0x1111222233334444, 0x5555666677778888, 0x9999aaaabbbbdddd,
    0xeeeeffff11112222, 0x3333444455556666, 0x7777888899990000, 0xaaaabbbbccccdddd,
    0xeeeeffffaaaabbbb, 0xccccddddeeee0000, 0xffff000011112222, 0x4444333322221111,
    0xbbbb0000aaaa9999, 0x8888777766665555, 0x4444333399998888, 0x7777666655554444,
    0x3333222211110000, 0xffffeeeeddddcccc, 0xbbbbaaaa99998888, 0x7777666655554444,
    0x3333222211110000, 0xffffeeeeddddcccc, 0xbbbbaaaa99998888, 0x7777666655554444,
    0x3333222211110000, 0x0000111122223333, 0x4444555566667777, 0x8888999900001111,
];

pub fn normalize_leaf_token_java(kind: &str) -> &'static str {
    match kind {
        "identifier" => "I",
        "string_literal" | "text_block" => "S",
        "decimal_integer_literal" | "hex_integer_literal" | "octal_integer_literal"
        | "binary_integer_literal" | "decimal_floating_point_literal"
        | "hex_floating_point_literal" => "N",
        "type_identifier" | "void_type" | "integral_type" | "floating_point_type"
        | "boolean_type" | "array_type" | "generic_type" => "T",
        "true" | "false" | "null_literal" | "if" | "else" | "for" | "while" | "do"
        | "return" | "break" | "continue" | "throw" | "try" | "catch" | "finally"
        | "switch" | "case" | "default" | "new" | "this" | "super" | "instanceof"
        | "class" | "interface" | "enum" | "extends" | "implements" | "static"
        | "final" | "public" | "private" | "protected" | "abstract" | "synchronized"
        | "volatile" | "transient" | "native" => "K",
        _ => "O",
    }
}

pub fn normalize_leaf_token_python(kind: &str) -> &'static str {
    match kind {
        "identifier" => "I",
        "string" | "concatenated_string" => "S",
        "integer" | "float" => "N",
        "type" => "T",
        "def" | "class" | "return" | "if" | "elif" | "else" | "for" | "while" | "with"
        | "import" | "from" | "as" | "pass" | "break" | "continue" | "raise" | "try"
        | "except" | "finally" | "yield" | "lambda" | "and" | "or" | "not" | "in"
        | "is" | "None" | "True" | "False" | "async" | "await" | "global" | "nonlocal"
        | "del" | "assert" | "match" | "case" => "K",
        _ => "O",
    }
}

pub fn normalize_leaf_token_typescript(kind: &str) -> &'static str {
    match kind {
        "identifier" | "property_identifier" | "shorthand_property_identifier" => "I",
        "string" | "template_string" | "regex" => "S",
        "number" => "N",
        "type_identifier" | "predefined_type" => "T",
        "function" | "class" | "return" | "if" | "else" | "for" | "while" | "do" | "switch"
        | "case" | "default" | "break" | "continue" | "throw" | "try" | "catch"
        | "finally" | "new" | "this" | "super" | "import" | "export" | "from" | "as"
        | "typeof" | "instanceof" | "in" | "of" | "delete" | "void" | "null" | "undefined"
        | "true" | "false" | "async" | "await" | "yield" | "let" | "const" | "var"
        | "type" | "interface" | "enum" | "extends" | "implements" | "static" | "abstract"
        | "public" | "private" | "protected" | "readonly" | "override" | "declare" => "K",
        _ => "O",
    }
}

pub(crate) fn collect_leaf_tokens<'a>(
    node: TsNode<'a>,
    normalize: fn(&str) -> &'static str,
    tokens: &mut Vec<&'static str>,
) {
    if node.child_count() == 0 {
        tokens.push(normalize(node.kind()));
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_leaf_tokens(child, normalize, tokens);
    }
}

pub(crate) fn minhash_compute(tokens: &[&'static str]) -> [u32; 64] {
    let mut hashes = [u32::MAX; 64];
    if tokens.len() < 3 {
        return hashes;
    }
    for i in 0..=(tokens.len() - 3) {
        let gram = format!("{}{}{}", tokens[i], tokens[i + 1], tokens[i + 2]);
        let gram_bytes = gram.as_bytes();
        for k in 0..MINHASH_K {
            let mut h: u64 = MINHASH_SEEDS[k];
            for &b in gram_bytes {
                h ^= b as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
            let v = (h >> 32) as u32;
            if v < hashes[k] {
                hashes[k] = v;
            }
        }
    }
    hashes
}

/// Compute a MinHash fingerprint for a tree-sitter body node using the given
/// language-specific token normalizer. Returns `None` if the body is too short
/// (< `MINHASH_MIN_TOKENS` leaf tokens) to produce a reliable fingerprint.
pub fn compute_body_fingerprint(
    body: TsNode<'_>,
    provider: &'static str,
    normalize: fn(&str) -> &'static str,
) -> Option<BodyFingerprint> {
    let mut tokens: Vec<&'static str> = Vec::new();
    collect_leaf_tokens(body, normalize, &mut tokens);
    let leaf_token_count = tokens.len() as u32;
    if leaf_token_count < MINHASH_MIN_TOKENS {
        return None;
    }
    let minhash = minhash_compute(&tokens);
    Some(BodyFingerprint {
        provider: provider.to_string(),
        leaf_token_count,
        minhash: minhash.to_vec(),
    })
}
