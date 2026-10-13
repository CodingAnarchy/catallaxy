//! Test-case reduction by statement deletion.
//!
//! A "unit" is one line, or a line that opens a block (ends with `{` or `|`) together with every
//! line up to its matching close. Units are deleted greedily, whole blocks first; a candidate is
//! kept when it still passes `catx check` and still fails the same way (the caller's predicate).

use std::collections::HashSet;
use std::time::{Duration, Instant};

/// Index ranges `[start, end]` (inclusive) of deletable units.
fn units(lines: &[&str]) -> Vec<(usize, usize)> {
    let mut out = vec![];
    for (i, l) in lines.iter().enumerate() {
        let t = l.trim_end();
        let opens = t.ends_with('{') || (t.ends_with('|') && t.contains('{'));
        if !opens {
            out.push((i, i));
            continue;
        }
        let indent = l.len() - l.trim_start().len();
        // The matching close is the next line at the same indentation that starts with `}` and
        // does not continue the construct (`} else {`).
        let mut end = None;
        for (j, m) in lines.iter().enumerate().skip(i + 1) {
            let mi = m.len() - m.trim_start().len();
            let mt = m.trim();
            if mi == indent && mt.starts_with('}') {
                if mt.ends_with('{') {
                    continue;
                }
                end = Some(j);
                break;
            }
            if mi < indent && !mt.is_empty() {
                break;
            }
        }
        out.push((i, end.unwrap_or(i)));
    }
    out
}

/// Reduces `src` while `interesting` holds, within `budget`.
pub fn minimize(src: &str, budget: Duration, mut interesting: impl FnMut(&str) -> bool) -> String {
    let start = Instant::now();
    let mut cur: Vec<String> = src.lines().map(String::from).collect();
    // Units whose deletion was tried and rejected, by text, so a restart skips them.
    let mut needed: HashSet<Vec<String>> = HashSet::new();
    'outer: while start.elapsed() < budget {
        let refs: Vec<&str> = cur.iter().map(String::as_str).collect();
        let mut us = units(&refs);
        // Larger units (whole blocks) first, then later lines first.
        us.sort_by_key(|&(s, e)| (std::cmp::Reverse(e - s), std::cmp::Reverse(s)));
        for (s, e) in us {
            if start.elapsed() >= budget {
                break 'outer;
            }
            let t = cur[s].trim();
            if t.is_empty() || t.starts_with('}') || t.starts_with('#') {
                continue;
            }
            let unit: Vec<String> = cur[s..=e].to_vec();
            if needed.contains(&unit) {
                continue;
            }
            let keep: Vec<String> =
                cur.iter().enumerate().filter(|(i, _)| *i < s || *i > e).map(|(_, l)| l.clone()).collect();
            if interesting(&(keep.join("\n") + "\n")) {
                cur = keep;
                continue 'outer;
            }
            needed.insert(unit);
        }
        break;
    }
    cur.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_span_blocks() {
        let src = ["a = 1", "if x {", "  b = 2", "} else {", "  c = 3", "}", "puts(a)"];
        let us = units(&src);
        assert_eq!(us[1], (1, 5));
        assert_eq!(us[2], (2, 2));
        assert_eq!(us[6], (6, 6));
    }

    #[test]
    fn deletes_what_is_not_needed() {
        let src = "a = 1\nb = 2\nfor i in 0...3 {\n  c = i\n}\nboom()\nd = 4\n";
        let min = minimize(src, Duration::from_secs(5), |s| s.contains("boom()"));
        assert_eq!(min, "boom()\n");
    }
}
