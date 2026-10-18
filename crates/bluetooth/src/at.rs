//! The AT lines a phone sends a hands-free unit, parsed.
//!
//! Only the audio gateway's side is parsed: what this crate *sends* is a
//! handful of fixed commands built where they are needed. Every line the
//! gateway sends is either a final result (`OK`, `ERROR`, `+CME ERROR: n`)
//! that completes the command in flight, or a result code — solicited by that
//! command or unsolicited — named by its `+XXXX:` prefix.
//!
//! Lines are split on CR and LF both: the gateway frames a response as
//! `\r\n<line>\r\n`, and phones differ in whether they send the leading pair.

/// One line from the gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgLine {
    Ok,
    /// `ERROR` or `+CME ERROR: <n>`; the number when there is one.
    Error(Option<u32>),
    /// The incoming call alert, repeated while it rings.
    Ring,
    /// `+BRSF: <features>`.
    Brsf(u32),
    /// `+CIND: ("service",(0,1)),("call",(0,1)),...` — the indicator names,
    /// in the order their values will be reported.
    CindNames(Vec<String>),
    /// `+CIND: 1,0,0,...` — every indicator's value, in that order.
    CindValues(Vec<u32>),
    /// `+CIEV: <index>,<value>`; the index counts from 1.
    Ciev {
        index: usize,
        value: u32,
    },
    /// `+CLIP: "<number>",<type>[,...,"<name>"]` — who is calling.
    Clip {
        number: Option<String>,
        name: Option<String>,
    },
    /// One line of a `+CLCC` listing.
    Clcc(ClccEntry),
    /// `+CHLD: (0,1,2,...)`.
    Chld,
    /// `+VGS: <n>` / `+VGM: <n>` — the phone moved a volume.
    Volume {
        speaker: bool,
        level: u8,
    },
    /// `+BCS: <codec>` — the gateway selects a codec.
    Bcs(u32),
    /// Anything else: kept whole for the log rather than guessed at.
    Other(String),
}

/// One call as `AT+CLCC` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClccEntry {
    pub index: u32,
    pub incoming: bool,
    /// 0 active, 1 held, 2 dialing, 3 alerting, 4 incoming, 5 waiting.
    pub status: u32,
    pub number: Option<String>,
}

/// Split what arrived on the link into complete lines, keeping a partial one
/// in `pending` for the next read.
pub fn split_lines(pending: &mut String, data: &str) -> Vec<String> {
    pending.push_str(data);
    let mut lines = Vec::new();
    while let Some(end) = pending.find(['\r', '\n']) {
        let line: String = pending.drain(..=end).collect();
        let line = line.trim();
        if !line.is_empty() {
            lines.push(line.to_string());
        }
    }
    lines
}

pub fn parse(line: &str) -> AgLine {
    let line = line.trim();
    match line {
        "OK" => return AgLine::Ok,
        "ERROR" => return AgLine::Error(None),
        "RING" => return AgLine::Ring,
        _ => {}
    }
    let Some((head, rest)) = line.split_once(':') else {
        return AgLine::Other(line.to_string());
    };
    let rest = rest.trim();
    match head.trim() {
        "+CME ERROR" => AgLine::Error(rest.parse().ok()),
        "+BRSF" => rest
            .parse()
            .map(AgLine::Brsf)
            .unwrap_or_else(|_| AgLine::Other(line.into())),
        "+CIND" if rest.starts_with('(') => AgLine::CindNames(cind_names(rest)),
        "+CIND" => AgLine::CindValues(numbers(rest)),
        "+CIEV" => match numbers(rest).as_slice() {
            [index, value] => AgLine::Ciev {
                index: *index as usize,
                value: *value,
            },
            _ => AgLine::Other(line.into()),
        },
        "+CLIP" => {
            let fields = fields(rest);
            AgLine::Clip {
                number: fields.first().and_then(|f| text(f)),
                // number, type, subaddress, its type, then the name.
                name: fields.get(4).and_then(|f| text(f)),
            }
        }
        "+CLCC" => {
            let fields = fields(rest);
            let num = |i: usize| fields.get(i).and_then(|f| f.trim().parse::<u32>().ok());
            match (num(0), num(1), num(2)) {
                (Some(index), Some(dir), Some(status)) => AgLine::Clcc(ClccEntry {
                    index,
                    incoming: dir == 1,
                    status,
                    number: fields.get(5).and_then(|f| text(f)),
                }),
                _ => AgLine::Other(line.into()),
            }
        }
        "+CHLD" => AgLine::Chld,
        "+VGS" | "+VGM" => match rest.parse::<u8>() {
            Ok(level) => AgLine::Volume {
                speaker: head.trim() == "+VGS",
                level,
            },
            Err(_) => AgLine::Other(line.into()),
        },
        "+BCS" => rest
            .parse()
            .map(AgLine::Bcs)
            .unwrap_or_else(|_| AgLine::Other(line.into())),
        _ => AgLine::Other(line.to_string()),
    }
}

/// Comma-separated fields, with commas inside quotes left alone.
fn fields(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            ',' if !quoted => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// A quoted field's text, `None` when empty — a withheld number arrives as
/// `""`, not as a missing field.
fn text(field: &str) -> Option<String> {
    let t = field.trim().trim_matches('"').trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn numbers(s: &str) -> Vec<u32> {
    s.split(',').filter_map(|n| n.trim().parse().ok()).collect()
}

/// The names out of `("service",(0,1)),("call",(0,1)),...`.
fn cind_names(s: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = s;
    while let Some(start) = rest.find('"') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('"') else { break };
        names.push(after[..end].to_string());
        rest = &after[end + 1..];
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_split_on_either_terminator_and_keep_a_partial_one() {
        let mut pending = String::new();
        assert_eq!(
            split_lines(&mut pending, "\r\n+BRSF: 871\r\n\r\nOK\r\n\r\n+CI"),
            vec!["+BRSF: 871", "OK"]
        );
        assert_eq!(split_lines(&mut pending, "EV: 2,1\r\n"), vec!["+CIEV: 2,1"]);
    }

    #[test]
    fn indicator_names_are_read_in_order() {
        let line = r#"+CIND: ("call",(0,1)),("callsetup",(0-3)),("service",(0-1)),("signal",(0-5)),("roam",(0,1)),("battchg",(0-5)),("callheld",(0-2))"#;
        assert_eq!(
            parse(line),
            AgLine::CindNames(
                [
                    "call",
                    "callsetup",
                    "service",
                    "signal",
                    "roam",
                    "battchg",
                    "callheld"
                ]
                .map(String::from)
                .to_vec()
            )
        );
        assert_eq!(
            parse("+CIND: 0,0,1,4,0,3,0"),
            AgLine::CindValues(vec![0, 0, 1, 4, 0, 3, 0])
        );
    }

    #[test]
    fn caller_id_carries_number_and_name_and_a_withheld_number_is_none() {
        assert_eq!(
            parse(r#"+CLIP: "+380501234567",145,,,"Mom""#),
            AgLine::Clip {
                number: Some("+380501234567".into()),
                name: Some("Mom".into())
            }
        );
        assert_eq!(
            parse(r#"+CLIP: "",128"#),
            AgLine::Clip {
                number: None,
                name: None
            }
        );
    }

    #[test]
    fn a_call_listing_line_names_direction_status_and_number() {
        assert_eq!(
            parse(r#"+CLCC: 1,1,4,0,0,"+380501234567",145"#),
            AgLine::Clcc(ClccEntry {
                index: 1,
                incoming: true,
                status: 4,
                number: Some("+380501234567".into())
            })
        );
    }

    #[test]
    fn extended_errors_keep_their_number() {
        assert_eq!(parse("+CME ERROR: 30"), AgLine::Error(Some(30)));
        assert_eq!(parse("ERROR"), AgLine::Error(None));
    }
}
