//! Lightweight, Oracle-aware SQL table scanner.
//!
//! Does NOT parse SQL fully — uses a conservative token-state-machine approach
//! tuned for the Oracle-style banking SQL patterns CIH encounters. Aims for zero
//! false positives at the cost of occasional missed tables in highly dynamic SQL.

/// Which direction data flows relative to the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableOp {
    Read,
    Write,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableAccess {
    pub table: String,
    pub op: TableOp,
}

/// SQL keywords we must not mistake for table names.
static SQL_KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "CROSS",
    "OUTER",
    "ON",
    "AND",
    "OR",
    "NOT",
    "IN",
    "IS",
    "NULL",
    "AS",
    "DISTINCT",
    "ALL",
    "INSERT",
    "INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE",
    "MERGE",
    "USING",
    "WHEN",
    "MATCHED",
    "THEN",
    "ORDER",
    "GROUP",
    "BY",
    "HAVING",
    "UNION",
    "INTERSECT",
    "EXCEPT",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "WITH",
    "RETURNING",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "PIVOT",
    "UNPIVOT",
    "CONNECT",
    "BETWEEN",
    "LIKE",
    "EXISTS",
    "ANY",
    "SOME",
    "LIMIT",
    "OFFSET",
    "FETCH",
    "NEXT",
    "ROWS",
    "ONLY",
    "FOR",
    "OF",
    "AT",
    "WITHIN",
    "PARTITION",
    "DUAL", // Oracle pseudo-table — always skipped
];

fn is_keyword(token: &str) -> bool {
    SQL_KEYWORDS.contains(&token)
}

/// Strip `/* ... */` block comments (including Oracle hints `/*+ ... */`).
fn strip_block_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            // skip until */
            i += 2;
            while i + 1 < bytes.len() {
                if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    i += 2;
                    break;
                }
                i += 1;
            }
            out.push(' ');
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

/// Strip `-- ...` line comments.
fn strip_line_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    for line in sql.lines() {
        if let Some(idx) = line.find("--") {
            out.push_str(&line[..idx]);
        } else {
            out.push_str(line);
        }
        out.push(' ');
    }
    out
}

/// Tokenize by whitespace and structural chars `( ) , ; =`.
fn tokenize(sql: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut start: Option<usize> = None;
    for (i, ch) in sql.char_indices() {
        let is_sep = matches!(ch, ' ' | '\t' | '\n' | '\r' | '(' | ')' | ',' | ';' | '=');
        if is_sep {
            if let Some(s) = start.take() {
                let tok = sql[s..i].trim();
                if !tok.is_empty() {
                    tokens.push(tok);
                }
            }
            // Emit `(` and `,` as their own tokens so callers can detect structure.
            match ch {
                '(' => tokens.push("("),
                ')' => tokens.push(")"),
                ',' => tokens.push(","),
                _ => {}
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        let tok = sql[s..].trim();
        if !tok.is_empty() {
            tokens.push(tok);
        }
    }
    tokens
}

/// Strip a leading `SCHEMA.` prefix from a potential table name.
fn strip_schema(name: &str) -> &str {
    if let Some(idx) = name.rfind('.') {
        &name[idx + 1..]
    } else {
        name
    }
}

/// Return `true` when `token` looks like a table name we should record.
fn is_table_candidate(token: &str) -> bool {
    if token.is_empty() || token == "(" || token == ")" || token == "," {
        return false;
    }
    // Functions or subquery aliases look like NAME( — they contain `(`
    if token.contains('(') || token.contains(')') {
        return false;
    }
    // String literals
    if token.starts_with('\'') || token.starts_with('"') {
        return false;
    }
    // Parameter placeholders
    if token.starts_with('?') || token.starts_with(':') {
        return false;
    }
    // Numeric literals
    if token
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
    {
        return false;
    }
    let bare = strip_schema(token);
    if is_keyword(bare) {
        return false;
    }
    true
}

/// Extract all table accesses from a SQL string.
///
/// Returns deduplicated `(table, op)` pairs — if a table appears as both Read and
/// Write (e.g. `MERGE`) both entries are present.
pub fn scan_tables(sql: &str) -> Vec<TableAccess> {
    let cleaned = strip_block_comments(sql);
    let cleaned = strip_line_comments(&cleaned);
    let upper = cleaned.to_ascii_uppercase();
    let tokens = tokenize(&upper);

    let mut results: Vec<TableAccess> = Vec::new();
    let mut depth: i32 = 0; // subquery parenthesis depth

    let n = tokens.len();
    let mut i = 0;

    while i < n {
        let tok = tokens[i];

        // Track subquery depth via `(` / `)` tokens.
        if tok == "(" {
            depth += 1;
            i += 1;
            continue;
        }
        if tok == ")" {
            if depth > 0 {
                depth -= 1;
            }
            i += 1;
            continue;
        }
        // Skip bare commas at the top level (handled inside the FROM loop).
        if tok == "," {
            i += 1;
            continue;
        }

        match tok {
            // SELECT ... FROM table [alias] [, table [alias]] ...
            // JOIN table [alias] ON ...
            "FROM" | "JOIN" => {
                let op = TableOp::Read;
                i += 1;
                // Consume comma-separated table list
                while i < n {
                    let candidate = tokens[i];
                    // `(` means a subquery is starting — descend into it but don't
                    // record it as a table. The tables inside will be picked up when
                    // we process those FROM/JOIN keywords.
                    if candidate == "(" {
                        depth += 1;
                        i += 1;
                        break;
                    }
                    if is_keyword(candidate) {
                        break;
                    }
                    if is_table_candidate(candidate) {
                        let name = strip_schema(candidate).to_string();
                        push_unique(&mut results, name, op);
                    }
                    i += 1;
                    // After a table name, peek: if next is a comma, continue to read
                    // the next table in the list; if it's an alias (non-keyword, non-comma
                    // identifier), skip it; then stop if neither comma nor keyword follows.
                    if i < n {
                        if tokens[i] == "," {
                            // comma-join: skip the comma and read the next table
                            i += 1;
                            continue;
                        }
                        // If next token looks like an alias (non-keyword identifier), skip it
                        if i < n && !is_keyword(tokens[i]) && tokens[i] != "(" && tokens[i] != "," {
                            i += 1; // skip alias
                        }
                        // After optional alias, if there's a comma — continue reading tables
                        if i < n && tokens[i] == "," {
                            i += 1;
                            continue;
                        }
                    }
                    break;
                }
                continue;
            }

            // INSERT INTO table ...
            "INTO" => {
                // Could be INSERT INTO or MERGE INTO — op depends on preceding token
                // For INSERT INTO: always write
                // For MERGE INTO: write
                // In both cases, the next token is the target table.
                i += 1;
                if i < n {
                    let candidate = tokens[i];
                    if is_table_candidate(candidate) {
                        let name = strip_schema(candidate).to_string();
                        push_unique(&mut results, name, TableOp::Write);
                        i += 1;
                    }
                }
                continue;
            }

            // UPDATE table SET ...
            "UPDATE" => {
                i += 1;
                if i < n {
                    let candidate = tokens[i];
                    // Skip `OR IGNORE`, `OR REPLACE` etc. (SQLite — but harmless to handle)
                    if candidate == "OR" {
                        i += 2; // skip OR + modifier
                    }
                    if i < n {
                        let candidate = tokens[i];
                        if is_table_candidate(candidate) {
                            let name = strip_schema(candidate).to_string();
                            push_unique(&mut results, name, TableOp::Write);
                            i += 1;
                        }
                    }
                }
                continue;
            }

            // DELETE FROM table ...
            "DELETE" => {
                // Skip FROM keyword
                i += 1;
                if i < n && tokens[i] == "FROM" {
                    i += 1;
                }
                if i < n {
                    let candidate = tokens[i];
                    if is_table_candidate(candidate) {
                        let name = strip_schema(candidate).to_string();
                        push_unique(&mut results, name, TableOp::Write);
                        i += 1;
                    }
                }
                continue;
            }

            // MERGE INTO ... USING source_table
            "MERGE" => {
                i += 1;
                // skip INTO
                if i < n && tokens[i] == "INTO" {
                    i += 1;
                }
                if i < n {
                    let candidate = tokens[i];
                    if is_table_candidate(candidate) {
                        let name = strip_schema(candidate).to_string();
                        push_unique(&mut results, name, TableOp::Write);
                        i += 1;
                    }
                }
                // USING <source> — source is read
                while i < n && tokens[i] != "USING" {
                    i += 1;
                }
                if i < n && tokens[i] == "USING" {
                    i += 1;
                    if i < n {
                        let candidate = tokens[i];
                        if candidate != "(" && is_table_candidate(candidate) {
                            let name = strip_schema(candidate).to_string();
                            push_unique(&mut results, name, TableOp::Read);
                            i += 1;
                        }
                    }
                }
                continue;
            }

            _ => {}
        }

        i += 1;
    }

    results
}

fn push_unique(results: &mut Vec<TableAccess>, table: String, op: TableOp) {
    let already = results.iter().any(|r| r.table == table && r.op == op);
    if !already && !table.is_empty() {
        results.push(TableAccess { table, op });
    }
}

#[cfg(test)]
mod tests;

