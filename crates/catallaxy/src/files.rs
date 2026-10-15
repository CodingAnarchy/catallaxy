//! Expands the path arguments of `check`, `fmt`, `fix` and `test`: files are taken as given,
//! directories are walked recursively for `*.cx` files.

use std::path::{Path, PathBuf};

/// What a directory walk leaves out, besides hidden directories and `target/`.
#[derive(Clone, Copy)]
pub struct Skip<'a> {
    /// Directories with these names (`reject/`, whose programs are meant to be wrong; `lib/`,
    /// which holds modules tests import).
    pub dirs: &'a [&'a str],
    /// Files whose first line is `# expect: ...`: tests that must fail `catx check`.
    pub expected_failures: bool,
}

/// What `check`, `fmt` and `fix` skip while walking: programs that are meant to be wrong.
pub const MEANT_TO_FAIL: Skip = Skip { dirs: &["reject"], expected_failures: true };

/// Expands `args` into the files to process, in argument order (each directory's files sorted).
///
/// A file named directly is always included. A path that does not exist is returned as is, so
/// the caller reports it.
pub fn expand(args: &[&str], skip: Skip) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for a in args {
        let p = Path::new(a);
        if p.is_dir() {
            let mut found = Vec::new();
            walk(p, skip, &mut found);
            found.sort();
            out.extend(found);
        } else {
            out.push(p.to_path_buf());
        }
    }
    out
}

fn walk(dir: &Path, skip: Skip, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name();
        let name = name.to_string_lossy();
        if p.is_dir() {
            if name.starts_with('.') || name == "target" || skip.dirs.contains(&name.as_ref()) {
                continue;
            }
            walk(&p, skip, out);
        } else if p.extension().is_some_and(|x| x == "cx") {
            if skip.expected_failures && expects_failure(&p) {
                continue;
            }
            out.push(p);
        }
    }
}

/// Whether the file's first line is `# expect: ...` (read no further than that line).
pub fn expects_failure(path: &Path) -> bool {
    use std::io::Read;
    let Ok(f) = std::fs::File::open(path) else { return false };
    let mut head = Vec::new();
    if f.take(256).read_to_end(&mut head).is_err() {
        return false;
    }
    head.starts_with(b"# expect:")
}
