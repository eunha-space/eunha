//! Where `eunha accounts`, `domains`, `emoji` and `maintenance` say what they
//! do and ask what they have to: a [`Console`], so that the tests can read it
//! and answer its questions; the terminal is [`Terminal`].

use std::collections::VecDeque;
use std::future::Future;
use std::sync::Mutex;

use futures::StreamExt as _;

/// With `--tenants`, the instance a command acts on.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct Instance {
    /// With `--tenants`, the instance, by its domain or one of its aliases.
    #[arg(long = "instance", value_name = "HOST")]
    pub host: Option<String>,
}

/// Where a command says what it does, and asks what it has to: Thor's `say`,
/// `ask` and `yes?`.
pub trait Console: Sync {
    fn say(&self, line: &str);
    /// `ask(question, default:)`: the answer, or `default` for none.
    fn ask(&self, question: &str, default: &str) -> String;
    /// `yes?(question)`: whether the answer is `y` or `yes`.
    fn yes(&self, question: &str) -> bool;
}

/// The terminal the command was run from.
pub struct Terminal;

impl Terminal {
    fn read_line(question: &str) -> String {
        use std::io::Write as _;
        print!("{question} ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        line.trim().to_owned()
    }
}

impl Console for Terminal {
    fn say(&self, line: &str) {
        println!("{line}");
    }

    fn ask(&self, question: &str, default: &str) -> String {
        let answer = Self::read_line(&format!("{question} ({default})"));
        if answer.is_empty() {
            default.to_owned()
        } else {
            answer
        }
    }

    fn yes(&self, question: &str) -> bool {
        is_yes(&Self::read_line(question))
    }
}

/// Thor's `yes?`: `y` or `yes`, in any case.
fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// A console that keeps what is said and gives the answers it was handed, in
/// order, for the tests.
#[derive(Default)]
pub struct Recorder {
    lines: Mutex<Vec<String>>,
    answers: Mutex<VecDeque<String>>,
}

impl Recorder {
    /// A recorder that answers with `answers`, then with nothing.
    pub fn answering<I: IntoIterator<Item = S>, S: Into<String>>(answers: I) -> Self {
        Self {
            lines: Mutex::default(),
            answers: Mutex::new(answers.into_iter().map(Into::into).collect()),
        }
    }

    /// Everything said so far.
    pub fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Everything said so far, a line each.
    pub fn output(&self) -> String {
        self.lines().join("\n")
    }

    fn answer(&self) -> String {
        self.answers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
            .unwrap_or_default()
    }
}

impl Console for Recorder {
    fn say(&self, line: &str) {
        self.lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(line.to_owned());
    }

    fn ask(&self, question: &str, default: &str) -> String {
        self.say(question);
        let answer = self.answer();
        if answer.is_empty() {
            default.to_owned()
        } else {
            answer
        }
    }

    fn yes(&self, question: &str) -> bool {
        self.say(question);
        is_yes(&self.answer())
    }
}

/// `dry_run_mode_suffix`.
pub fn dry_run_suffix(dry_run: bool) -> &'static str {
    if dry_run {
        " (DRY RUN)"
    } else {
        ""
    }
}

/// `ProgressHelper#parallelize_with_progress`: `work` for each item, at most
/// `concurrency` at once. Returns how many items there were, and the sum of
/// what `work` counted; an item whose work fails is reported as Mastodon
/// reports it and counts nothing.
pub async fn parallelize<F, Fut>(
    console: &dyn Console,
    ids: Vec<i64>,
    concurrency: usize,
    verbose: bool,
    work: F,
) -> anyhow::Result<(u64, u64)>
where
    F: Fn(i64) -> Fut,
    Fut: Future<Output = anyhow::Result<u64>>,
{
    anyhow::ensure!(
        concurrency >= 1,
        "Cannot run with this concurrency setting, must be at least 1"
    );
    let total = ids.len() as u64;
    let work = &work;
    let aggregate = futures::stream::iter(ids)
        .map(|id| async move {
            if verbose {
                console.say(&format!("Processing {id}"));
            }
            match work(id).await {
                Ok(counted) => counted,
                Err(error) => {
                    console.say(&format!("Error processing {id}: {error:#}"));
                    0
                }
            }
        })
        .buffer_unordered(concurrency)
        .fold(0u64, |sum, counted| async move { sum + counted })
        .await;
    Ok((total, aggregate))
}

/// The connection pool a command acting `concurrency` items at a time needs:
/// `reset_connection_pools!`'s `concurrency + 1`.
pub fn connections_for(concurrency: usize) -> u32 {
    u32::try_from(concurrency)
        .unwrap_or(u32::MAX)
        .saturating_add(1)
}
