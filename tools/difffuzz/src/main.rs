//! difffuzz: differential fuzzing of whole Catallaxy programs across builds and GC modes.
//!
//! See README.md for usage.

mod gen;
mod minimize;
mod oracle;
mod rng;

use oracle::{Bins, Mode, Verdict};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const USAGE: &str = "\
usage:
  difffuzz [fuzz] [options]      generate and check programs until the time or count runs out
  difffuzz gen SEED              print the program generated for SEED
  difffuzz run FILE...           run the oracle on existing programs
  difffuzz minimize FILE         reduce a failing program (writes FILE.min.cx)

options:
  --time SECS          time budget for `fuzz` (default 60)
  --count N            stop after N generated programs (default: unlimited)
  --seed N             first program seed (default: from the clock); program i uses seed N+i
  --jobs N             programs checked in parallel (default: half the cores)
  --catx PATH          release catx (default target/release/catx)
  --catx-debug PATH    debug catx (default target/debug/catx)
  --modes a,b,...      modes to run (default release,debug,gc-stress,gc-nongen,gc-threads0;
                       also available: gc-stress-nongen, debug-gc-stress). The first is the baseline.
  --corpus DIR         also run every .cx file in DIR first (repeatable)
  --out DIR            where failing programs are saved (default target/difffuzz/failures)
  --work DIR           scratch directory (default target/difffuzz/work)
  --timeout SECS       per-run timeout (default 60)
  --minimize-secs N    time budget per minimization (default 120; 0 disables)
  --save-rejects DIR   save generated programs that `catx check` rejects
";

struct Config {
    time: Duration,
    count: Option<u64>,
    seed: u64,
    jobs: usize,
    bins: Bins,
    modes: Vec<Mode>,
    corpus: Vec<PathBuf>,
    out: PathBuf,
    work: PathBuf,
    timeout: Duration,
    minimize: Duration,
    save_rejects: Option<PathBuf>,
}

fn die(msg: &str) -> ! {
    eprintln!("difffuzz: {msg}\n\n{USAGE}");
    std::process::exit(2)
}

fn parse(args: &[String]) -> (String, Vec<String>, Config) {
    let exe = std::env::consts::EXE_SUFFIX;
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
    let mut cfg = Config {
        time: Duration::from_secs(60),
        count: None,
        seed: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() % 1_000_000 * 1000,
        jobs: (cores / 2).max(1),
        bins: Bins {
            release: PathBuf::from(format!("target/release/catx{exe}")),
            debug: PathBuf::from(format!("target/debug/catx{exe}")),
        },
        modes: vec![],
        corpus: vec![],
        out: PathBuf::from("target/difffuzz/failures"),
        work: PathBuf::from("target/difffuzz/work"),
        timeout: Duration::from_secs(60),
        minimize: Duration::from_secs(120),
        save_rejects: None,
    };
    let mut modes = oracle::DEFAULT_MODES.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let mut cmd = None;
    let mut rest = vec![];
    let mut it = args.iter();
    let num = |v: Option<&String>, name: &str| -> u64 {
        v.and_then(|s| s.parse().ok()).unwrap_or_else(|| die(&format!("{name} needs a number")))
    };
    while let Some(a) = it.next() {
        match a.as_str() {
            "--time" => cfg.time = Duration::from_secs(num(it.next(), a)),
            "--count" => cfg.count = Some(num(it.next(), a)),
            "--seed" => cfg.seed = num(it.next(), a),
            "--jobs" => cfg.jobs = num(it.next(), a).max(1) as usize,
            "--timeout" => cfg.timeout = Duration::from_secs(num(it.next(), a)),
            "--minimize-secs" => cfg.minimize = Duration::from_secs(num(it.next(), a)),
            "--catx" => cfg.bins.release = it.next().unwrap_or_else(|| die("--catx needs a path")).into(),
            "--catx-debug" => cfg.bins.debug = it.next().unwrap_or_else(|| die("--catx-debug needs a path")).into(),
            "--corpus" => cfg.corpus.push(it.next().unwrap_or_else(|| die("--corpus needs a directory")).into()),
            "--out" => cfg.out = it.next().unwrap_or_else(|| die("--out needs a directory")).into(),
            "--work" => cfg.work = it.next().unwrap_or_else(|| die("--work needs a directory")).into(),
            "--save-rejects" => cfg.save_rejects = Some(it.next().unwrap_or_else(|| die("needs a directory")).into()),
            "--modes" => {
                modes = it.next().unwrap_or_else(|| die("--modes needs a list")).split(',').map(String::from).collect()
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0)
            }
            s if s.starts_with("--") => die(&format!("unknown option {s}")),
            s if cmd.is_none() && ["fuzz", "gen", "run", "minimize"].contains(&s) => cmd = Some(s.to_string()),
            s => rest.push(s.to_string()),
        }
    }
    let all = oracle::all_modes();
    cfg.modes = modes
        .iter()
        .map(|n| all.iter().find(|m| &m.name == n).cloned().unwrap_or_else(|| die(&format!("unknown mode {n}"))))
        .collect();
    (cmd.unwrap_or_else(|| "fuzz".into()), rest, cfg)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest, cfg) = parse(&args);
    match cmd.as_str() {
        "gen" => {
            let seed = rest.first().and_then(|s| s.parse().ok()).unwrap_or_else(|| die("gen needs a SEED"));
            print!("{}", gen::generate(seed));
        }
        "run" => {
            if rest.is_empty() {
                die("run needs files");
            }
            check_bins(&cfg);
            let mut failed = false;
            for f in &rest {
                let path = Path::new(f);
                if let Err(e) = oracle::check(&cfg.bins, path) {
                    println!("{f}: rejected by catx check\n{e}");
                    failed = true;
                    continue;
                }
                let (v, results) = oracle::judge(&cfg.bins, &cfg.modes, path, cfg.timeout, is_generated(path));
                match &v {
                    Verdict::Pass => println!("{f}: ok"),
                    Verdict::Slow => {
                        println!("{f}: too slow to judge (a mode timed out, but so nearly did the baseline)")
                    }
                    Verdict::Fail { .. } => {
                        failed = true;
                        println!("{f}:\n{}", oracle::report(&v, &results));
                    }
                }
            }
            std::process::exit(if failed { 1 } else { 0 });
        }
        "minimize" => {
            let f = rest.first().unwrap_or_else(|| die("minimize needs a FILE"));
            check_bins(&cfg);
            std::fs::create_dir_all(&cfg.work).unwrap();
            let src = std::fs::read_to_string(f).unwrap_or_else(|e| die(&format!("{f}: {e}")));
            let path = Path::new(f);
            // Intermittent failures get several chances to show up.
            let failure = (0..10).find_map(|_| {
                match oracle::judge(&cfg.bins, &cfg.modes, path, cfg.timeout, is_generated(path)).0 {
                    Verdict::Fail { kind, .. } => Some(kind),
                    Verdict::Pass | Verdict::Slow => None,
                }
            });
            let Some(kind) = failure else {
                println!("{f}: does not fail (10 attempts)");
                return;
            };
            let budget = if cfg.minimize.is_zero() { Duration::from_secs(120) } else { cfg.minimize };
            let min = minimize_failure(&cfg, &src, &kind, is_generated(path), 0, budget);
            let out = path.with_extension("min.cx");
            std::fs::write(&out, &min).unwrap();
            println!("{kind}: {} -> {} lines, wrote {}", src.lines().count(), min.lines().count(), out.display());
        }
        _ => fuzz(cfg),
    }
}

/// Generated programs carry a header comment; only they must exit 0.
fn is_generated(path: &Path) -> bool {
    std::fs::read_to_string(path).map(|s| s.starts_with("# difffuzz program")).unwrap_or(false)
}

fn check_bins(cfg: &Config) {
    for m in &cfg.modes {
        let b = cfg.bins.for_mode(m);
        if !b.exists() {
            die(&format!(
                "{} not found (mode {}); build it with `cargo build{}`",
                b.display(),
                m.name,
                if m.debug { "" } else { " --release" }
            ));
        }
    }
}

/// Reduces `src`, keeping candidates that type-check and fail with the same `kind`.
fn minimize_failure(
    cfg: &Config,
    src: &str,
    kind: &str,
    require_success: bool,
    worker: usize,
    budget: Duration,
) -> String {
    // Only the baseline and the failing mode are needed to reproduce.
    let failing_mode = kind.split(':').nth(1).unwrap_or("");
    let modes: Vec<Mode> = cfg
        .modes
        .iter()
        .enumerate()
        .filter(|(i, m)| *i == 0 || m.name == failing_mode)
        .map(|(_, m)| m.clone())
        .collect();
    let file = cfg.work.join(format!("min{worker}.cx"));
    minimize::minimize(src, budget, |cand| {
        if std::fs::write(&file, cand).is_err() || oracle::check(&cfg.bins, &file).is_err() {
            return false;
        }
        // Flaky failures (thread timing) get a few attempts.
        let tries = if kind.starts_with("panic") { 1 } else { 3 };
        (0..tries).any(|_| {
            matches!(oracle::judge(&cfg.bins, &modes, &file, cfg.timeout, require_success).0,
                Verdict::Fail { kind: k, .. } if k == kind)
        })
    })
}

#[derive(Default)]
struct Stats {
    generated: AtomicU64,
    rejected: AtomicU64,
    passed: AtomicU64,
    corpus_run: AtomicU64,
    /// Programs whose baseline was too slow to tell a timeout from a hang.
    slow: AtomicU64,
    failed: AtomicU64,
    /// Failure kind -> (count, first saved file).
    kinds: Mutex<HashMap<String, (u64, String)>>,
}

enum Job {
    Corpus(PathBuf),
    Generated(u64),
}

fn fuzz(cfg: Config) {
    check_bins(&cfg);
    std::fs::create_dir_all(&cfg.work).unwrap();
    std::fs::create_dir_all(&cfg.out).unwrap();
    if let Some(d) = &cfg.save_rejects {
        std::fs::create_dir_all(d).unwrap();
    }
    let mut corpus = vec![];
    for d in &cfg.corpus {
        let mut files: Vec<PathBuf> = std::fs::read_dir(d)
            .unwrap_or_else(|e| die(&format!("{}: {e}", d.display())))
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "cx"))
            .collect();
        files.sort();
        corpus.extend(files);
    }
    let modes: Vec<&str> = cfg.modes.iter().map(|m| m.name.as_str()).collect();
    println!(
        "difffuzz: seed {}, {} jobs, {}s budget, modes {}, {} corpus programs",
        cfg.seed,
        cfg.jobs,
        cfg.time.as_secs(),
        modes.join(","),
        corpus.len()
    );
    let cfg = Arc::new(cfg);
    let corpus = Arc::new(corpus);
    let stats = Arc::new(Stats::default());
    let next = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let start = Instant::now();
    let deadline = start + cfg.time;
    let mut handles = vec![];
    for w in 0..cfg.jobs {
        let (cfg, corpus, stats, next, stop) = (cfg.clone(), corpus.clone(), stats.clone(), next.clone(), stop.clone());
        handles.push(std::thread::spawn(move || loop {
            // The corpus always runs in full; generated programs only within the budget.
            let i = next.fetch_add(1, Ordering::SeqCst);
            let job = if i < corpus.len() {
                Job::Corpus(corpus[i].clone())
            } else {
                let k = (i - corpus.len()) as u64;
                if stop.load(Ordering::SeqCst) || Instant::now() >= deadline || cfg.count.is_some_and(|c| k >= c) {
                    return;
                }
                Job::Generated(cfg.seed + k)
            };
            work(&cfg, &stats, w, job);
        }));
    }
    // Progress every 30 seconds.
    let reporter = {
        let (stats, stop) = (stats.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut last = Instant::now();
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(200));
                if last.elapsed() >= Duration::from_secs(30) {
                    last = Instant::now();
                    println!(
                        "  [{:>5}s] {} generated, {} rejected, {} passed, {} failed",
                        start.elapsed().as_secs(),
                        stats.generated.load(Ordering::SeqCst),
                        stats.rejected.load(Ordering::SeqCst),
                        stats.passed.load(Ordering::SeqCst),
                        stats.failed.load(Ordering::SeqCst)
                    );
                }
            }
        })
    };
    for h in handles {
        h.join().unwrap();
    }
    stop.store(true, Ordering::SeqCst);
    reporter.join().unwrap();

    let gen = stats.generated.load(Ordering::SeqCst);
    let rej = stats.rejected.load(Ordering::SeqCst);
    let failed = stats.failed.load(Ordering::SeqCst);
    println!("difffuzz: done in {:.0}s", start.elapsed().as_secs_f64());
    println!("  corpus programs run:   {}", stats.corpus_run.load(Ordering::SeqCst));
    println!("  programs generated:    {gen}");
    println!("  rejected by catx check: {rej} ({:.1}%)", if gen == 0 { 0.0 } else { 100.0 * rej as f64 / gen as f64 });
    println!("  generated and passed:  {}", stats.passed.load(Ordering::SeqCst));
    println!("  too slow to judge:     {}", stats.slow.load(Ordering::SeqCst));
    println!("  failures:              {failed}");
    let kinds = stats.kinds.lock().unwrap();
    let mut ks: Vec<_> = kinds.iter().collect();
    ks.sort();
    for (k, (n, f)) in ks {
        println!("    {n:>4} x {k}   (e.g. {f})");
    }
    if failed > 0 {
        println!("failing programs saved in {}", cfg.out.display());
        std::process::exit(1);
    }
}

fn work(cfg: &Config, stats: &Stats, w: usize, job: Job) {
    let (src, name, require_success) = match &job {
        Job::Corpus(p) => {
            let src = std::fs::read_to_string(p).unwrap_or_default();
            let name = p.file_stem().unwrap().to_string_lossy().into_owned();
            let generated = src.starts_with("# difffuzz program");
            (src, name, generated)
        }
        Job::Generated(seed) => {
            stats.generated.fetch_add(1, Ordering::SeqCst);
            (gen::generate(*seed), format!("seed{seed}"), true)
        }
    };
    let file = match &job {
        // Run corpus programs in place: they may import modules next to them.
        Job::Corpus(p) => p.clone(),
        Job::Generated(_) => {
            let f = cfg.work.join(format!("w{w}.cx"));
            std::fs::write(&f, &src).unwrap();
            f
        }
    };
    if let Err(diag) = oracle::check(&cfg.bins, &file) {
        match job {
            Job::Generated(_) => {
                stats.rejected.fetch_add(1, Ordering::SeqCst);
                if let Some(d) = &cfg.save_rejects {
                    let _ = std::fs::write(d.join(format!("{name}.cx")), &src);
                    let _ = std::fs::write(d.join(format!("{name}.txt")), &diag);
                }
            }
            Job::Corpus(p) => println!("  corpus {} rejected by catx check (skipped)", p.display()),
        }
        return;
    }
    let (verdict, results) = oracle::judge(&cfg.bins, &cfg.modes, &file, cfg.timeout, require_success);
    match &job {
        Job::Corpus(_) => {
            stats.corpus_run.fetch_add(1, Ordering::SeqCst);
        }
        Job::Generated(_) if verdict == Verdict::Pass => {
            stats.passed.fetch_add(1, Ordering::SeqCst);
        }
        _ => {}
    }
    if verdict == Verdict::Slow {
        stats.slow.fetch_add(1, Ordering::SeqCst);
        println!("  slow: {name} (baseline near the timeout; not judged)");
        return;
    }
    let Verdict::Fail { kind, detail } = &verdict else {
        return;
    };
    stats.failed.fetch_add(1, Ordering::SeqCst);
    let slug: String = kind.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    let slug: String = slug.chars().take(60).collect();
    let base = cfg.out.join(format!("{slug}-{name}"));
    let cx = base.with_extension("cx");
    std::fs::write(&cx, &src).unwrap();
    std::fs::write(base.with_extension("log"), oracle::report(&verdict, &results)).unwrap();
    let first = {
        let mut kinds = stats.kinds.lock().unwrap();
        let e = kinds.entry(kind.clone()).or_insert((0, cx.display().to_string()));
        e.0 += 1;
        e.0 <= 2
    };
    println!("  FAIL {kind} in {name}: {}", detail.lines().next().unwrap_or(""));
    // Minimize the first couple of failures of each kind.
    if first && !cfg.minimize.is_zero() && src.lines().count() > 5 {
        let min = minimize_failure(cfg, &src, kind, require_success, w, cfg.minimize);
        let mf = cfg.out.join(format!("{slug}-{name}.min.cx"));
        std::fs::write(&mf, &min).unwrap();
        println!("       minimized {} -> {} lines: {}", src.lines().count(), min.lines().count(), mf.display());
    }
}
