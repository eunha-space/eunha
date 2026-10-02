//! CSV as Ruby's `csv` library writes and reads it, which is what
//! Mastodon's exports are made with and its imports parsed by.
//!
//! Writing follows `CSV.generate`'s defaults: `,` between fields, `\n`
//! after each row, a field quoted only when it holds a quote, a comma or a
//! line break, an empty string written as `""` (`quote_empty: true`) and
//! `nil` as nothing at all.
//!
//! Reading follows `CSV.open` with `skip_blanks: true`: the row separator
//! is whichever line ending comes first, blank lines are skipped, an empty
//! unquoted field is `nil` while an empty quoted one is `""`, and malformed
//! quoting is an error worded as `CSV::MalformedCSVError` words it.

/// One field to write: `None` is Ruby's `nil`.
pub type Field = Option<String>;

/// `CSV.generate_line(fields)`.
pub fn write_row(out: &mut String, fields: &[Field]) {
    for (index, field) in fields.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let Some(field) = field else { continue };
        if field.is_empty() || field.contains(['"', ',', '\r', '\n']) {
            out.push('"');
            out.push_str(&field.replace('"', "\"\""));
            out.push('"');
        } else {
            out.push_str(field);
        }
    }
    out.push('\n');
}

/// A `CSV::MalformedCSVError`'s message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Malformed(pub String);

impl std::fmt::Display for Malformed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Every row of `data`, blank lines skipped. A field is `None` when it was
/// empty and unquoted.
pub fn parse(data: &[u8]) -> Result<Vec<Vec<Field>>, Malformed> {
    let text = std::str::from_utf8(data).map_err(|error| {
        let line = data[..error.valid_up_to()]
            .iter()
            .filter(|&&b| b == b'\n')
            .count()
            + 1;
        Malformed(format!("Invalid byte sequence in UTF-8 in line {line}."))
    })?;
    // `row_sep: :auto`: the first line ending in the data.
    let row_sep: &str = match text.find(['\r', '\n']) {
        Some(at) if text[at..].starts_with("\r\n") => "\r\n",
        Some(at) if text[at..].starts_with('\r') => "\r",
        _ => "\n",
    };

    let mut rows = Vec::new();
    let mut row: Vec<Field> = Vec::new();
    let mut line = 1usize;
    let mut rest = text;
    // Whether the current row has begun; a row separator straight after the
    // previous one is a blank line.
    let mut in_row = false;
    loop {
        if rest.is_empty() {
            if in_row {
                rows.push(std::mem::take(&mut row));
            }
            break;
        }
        if let Some(after) = rest.strip_prefix(row_sep) {
            if in_row {
                rows.push(std::mem::take(&mut row));
                in_row = false;
            }
            line += 1;
            rest = after;
            continue;
        }
        in_row = true;
        // One field, then a comma, a row separator, or the end.
        if let Some(quoted) = rest.strip_prefix('"') {
            let start_line = line;
            let mut value = String::new();
            let mut chars = quoted.char_indices();
            let mut closed_at = None;
            while let Some((at, c)) = chars.next() {
                if c == '"' {
                    if quoted[at + 1..].starts_with('"') {
                        value.push('"');
                        chars.next();
                    } else {
                        closed_at = Some(at + 1);
                        break;
                    }
                } else {
                    if c == '\n' {
                        line += 1;
                    }
                    value.push(c);
                }
            }
            let Some(closed_at) = closed_at else {
                return Err(Malformed(format!(
                    "Unclosed quoted field in line {start_line}."
                )));
            };
            rest = &quoted[closed_at..];
            row.push(Some(value));
            if let Some(after) = rest.strip_prefix(',') {
                rest = after;
                if rest.is_empty() || rest.starts_with(row_sep) {
                    row.push(None);
                }
            } else if !(rest.is_empty() || rest.starts_with(row_sep)) {
                return Err(Malformed(format!(
                    "Any value after quoted field isn't allowed in line {line}."
                )));
            }
        } else {
            let end = rest
                .find(|c| c == ',' || row_sep.starts_with(c) || c == '\r' || c == '\n')
                .unwrap_or(rest.len());
            let value = &rest[..end];
            if value.contains('"') {
                return Err(Malformed(format!("Illegal quoting in line {line}.")));
            }
            rest = &rest[end..];
            if !(rest.is_empty() || rest.starts_with(row_sep) || rest.starts_with(',')) {
                let c = rest.chars().next().unwrap_or('\n');
                return Err(Malformed(format!(
                    "Unquoted fields do not allow new line <{c:?}> in line {line}."
                )));
            }
            row.push((!value.is_empty()).then(|| value.to_owned()));
            if let Some(after) = rest.strip_prefix(',') {
                rest = after;
                if rest.is_empty() || rest.starts_with(row_sep) {
                    row.push(None);
                }
            }
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(fields: &[Option<&str>]) -> String {
        let mut out = String::new();
        let fields: Vec<Field> = fields.iter().map(|f| f.map(str::to_owned)).collect();
        write_row(&mut out, &fields);
        out
    }

    #[test]
    fn writes_as_ruby_does() {
        assert_eq!(line(&[Some("a"), Some("b")]), "a,b\n");
        assert_eq!(line(&[Some(""), None]), "\"\",\n");
        assert_eq!(line(&[Some("en, fr")]), "\"en, fr\"\n");
        assert_eq!(line(&[Some("say \"hi\"")]), "\"say \"\"hi\"\"\"\n");
        assert_eq!(line(&[Some(" padded ")]), " padded \n");
    }

    fn some(s: &str) -> Field {
        Some(s.to_owned())
    }

    #[test]
    fn reads_as_ruby_does() {
        assert_eq!(
            parse(b"a,b\n\nc,\n").unwrap(),
            vec![vec![some("a"), some("b")], vec![some("c"), None]]
        );
        assert_eq!(
            parse(b"\"x,y\",\"\"\r\nz\r\n").unwrap(),
            vec![vec![some("x,y"), some("")], vec![some("z")]]
        );
        assert_eq!(
            parse(b"\"multi\nline\",2").unwrap(),
            vec![vec![some("multi\nline"), some("2")]]
        );
        assert_eq!(parse(b"").unwrap(), Vec::<Vec<Field>>::new());
        assert_eq!(parse(b"\n\n").unwrap(), Vec::<Vec<Field>>::new());
    }

    #[test]
    fn malformed_quoting_is_refused() {
        assert_eq!(
            parse(b"a\"b\n").unwrap_err().0,
            "Illegal quoting in line 1."
        );
        assert_eq!(
            parse(b"ok\n\"open\n").unwrap_err().0,
            "Unclosed quoted field in line 2."
        );
        assert_eq!(
            parse(b"\"a\"b\n").unwrap_err().0,
            "Any value after quoted field isn't allowed in line 1."
        );
        assert!(parse(b"\xff\n").is_err());
    }
}
