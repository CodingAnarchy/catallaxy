//! Line diffs for `catx test` failures: a unified diff with invisible characters made visible.

/// Lines of context around each change.
const CONTEXT: usize = 3;

/// Above this many line pairs (after the common start and end are taken off), the middle is
/// shown as all removed and all added instead of being aligned, to bound time and memory.
const MAX_CELLS: usize = 4_000_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Edit {
    Same,
    Del,
    Add,
}

/// A unified diff from `want` to `got`, headed `--- {want_name}` / `+++ {got_name}`; empty when
/// they are equal. Tabs, carriage returns, other control characters and trailing spaces are shown
/// as visible symbols, and a missing final newline is noted, so whitespace differences show.
pub fn unified(want_name: &str, want: &str, got_name: &str, got: &str) -> String {
    if want == got {
        return String::new();
    }
    let a = split(want);
    let b = split(got);
    let edits = align(&a, &b);
    let mut out = format!("--- {want_name}\n+++ {got_name}\n");
    // Group the edits into hunks with CONTEXT lines around each run of changes.
    let changed: Vec<usize> = edits.iter().enumerate().filter(|(_, e)| e.0 != Edit::Same).map(|(i, _)| i).collect();
    let mut i = 0;
    while i < changed.len() {
        let start = changed[i].saturating_sub(CONTEXT);
        let mut end = changed[i];
        while i < changed.len() && changed[i] <= end + 2 * CONTEXT + 1 {
            end = changed[i];
            i += 1;
        }
        let end = (end + CONTEXT + 1).min(edits.len());
        let (a0, b0) = (edits[start].1, edits[start].2);
        let a_len = edits[start..end].iter().filter(|e| e.0 != Edit::Add).count();
        let b_len = edits[start..end].iter().filter(|e| e.0 != Edit::Del).count();
        out.push_str(&format!("@@ -{} +{} @@\n", range(a0, a_len), range(b0, b_len)));
        for &(e, ai, bi) in &edits[start..end] {
            let (mark, line) = match e {
                Edit::Same => (' ', &a[ai]),
                Edit::Del => ('-', &a[ai]),
                Edit::Add => ('+', &b[bi]),
            };
            out.push(mark);
            out.push_str(&visible(&line.0));
            out.push('\n');
            if !line.1 {
                out.push_str("\\ No newline at end of output\n");
            }
        }
    }
    out
}

/// `start,len` with 1-based `start`, as unified diffs number lines (`0,0` for an empty side).
fn range(start: usize, len: usize) -> String {
    if len == 0 {
        format!("{start},0")
    } else {
        format!("{},{len}", start + 1)
    }
}

/// The lines of `s`, each with whether it ended in a newline.
fn split(s: &str) -> Vec<(String, bool)> {
    s.split_inclusive('\n')
        .map(|l| match l.strip_suffix('\n') {
            Some(l) => (l.to_string(), true),
            None => (l.to_string(), false),
        })
        .collect()
}

/// The edit script turning `a` into `b`: each step with the indexes into `a` and `b` it stands at.
fn align(a: &[(String, bool)], b: &[(String, bool)]) -> Vec<(Edit, usize, usize)> {
    let pre = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..].iter().rev().zip(b[pre..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (am, bm) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    let mut out: Vec<(Edit, usize, usize)> = (0..pre).map(|i| (Edit::Same, i, i)).collect();
    if (am.len() + 1).saturating_mul(bm.len() + 1) > MAX_CELLS {
        out.extend((0..am.len()).map(|i| (Edit::Del, pre + i, pre)));
        out.extend((0..bm.len()).map(|j| (Edit::Add, pre + am.len(), pre + j)));
    } else {
        // Longest common subsequence, by suffix lengths.
        let w = bm.len() + 1;
        let mut lcs = vec![0u32; (am.len() + 1) * w];
        for i in (0..am.len()).rev() {
            for j in (0..bm.len()).rev() {
                lcs[i * w + j] = if am[i] == bm[j] {
                    lcs[(i + 1) * w + j + 1] + 1
                } else {
                    lcs[(i + 1) * w + j].max(lcs[i * w + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < am.len() || j < bm.len() {
            if i < am.len() && j < bm.len() && am[i] == bm[j] {
                out.push((Edit::Same, pre + i, pre + j));
                i += 1;
                j += 1;
            } else if i < am.len() && (j == bm.len() || lcs[(i + 1) * w + j] >= lcs[i * w + j + 1]) {
                // Removals before additions, as diff(1) shows a changed line.
                out.push((Edit::Del, pre + i, pre + j));
                i += 1;
            } else {
                out.push((Edit::Add, pre + i, pre + j));
                j += 1;
            }
        }
    }
    let (ae, be) = (a.len() - suf, b.len() - suf);
    out.extend((0..suf).map(|k| (Edit::Same, ae + k, be + k)));
    out
}

/// `line` with tabs as `→`, carriage returns as `␍`, other control characters as `\u{..}`, and
/// trailing spaces as `·`.
fn visible(line: &str) -> String {
    let body = line.trim_end_matches(' ');
    let mut out = String::with_capacity(line.len());
    for c in body.chars() {
        match c {
            '\t' => out.push('→'),
            '\r' => out.push('␍'),
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out.extend(std::iter::repeat_n('·', line.len() - body.len()));
    out
}

#[cfg(test)]
mod tests {
    use super::unified;

    #[test]
    fn hunks_context_and_markers() {
        assert_eq!(unified("want", "a\nb\n", "got", "a\nb\n"), "");
        let d = unified("want", "1\n2\n3\n4\n5\n6\n7\n8\n9\n", "got", "1\n2\n3\n4\nX\n6\n7\n8\n9\n");
        assert_eq!(d, "--- want\n+++ got\n@@ -2,7 +2,7 @@\n 2\n 3\n 4\n-5\n+X\n 6\n 7\n 8\n");
        let d = unified("want", "hi\n", "got", "hi \tx\r\n");
        assert_eq!(d, "--- want\n+++ got\n@@ -1,1 +1,1 @@\n-hi\n+hi →x␍\n");
        let d = unified("want", "a\nb\n", "got", "a\nb");
        assert_eq!(d, "--- want\n+++ got\n@@ -1,2 +1,2 @@\n a\n-b\n+b\n\\ No newline at end of output\n");
        let d = unified("want", "", "got", "x\n");
        assert_eq!(d, "--- want\n+++ got\n@@ -0,0 +1,1 @@\n+x\n");
        let d = unified("want", "a\n", "got", "");
        assert_eq!(d, "--- want\n+++ got\n@@ -1,1 +0,0 @@\n-a\n");
    }

    #[test]
    fn insertions_and_deletions_align() {
        let d = unified("w", "a\nb\nc\nd\n", "g", "a\nc\nd\ne\n");
        assert_eq!(d, "--- w\n+++ g\n@@ -1,4 +1,4 @@\n a\n-b\n c\n d\n+e\n");
    }
}
