use std::{io, io::prelude::*};
use structopt::StructOpt;
use std::io::{BufWriter, Write};

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

#[derive(StructOpt, Debug)]
#[structopt(name = "pgdump_filter")]
struct Options {
    /// Exclude the listed copy block(s)
    #[structopt(short = "e", long = "excluded_copy_blocks", conflicts_with="included_copy_blocks")]
    excluded_copy_blocks: Vec<String>,
    /// Include the listed copy block(s)
    #[structopt(short = "i", long = "included_copy_blocks")]
    included_copy_blocks: Vec<String>,
    /// Flag to exclude large object operations (lo_read, lowrite, lo_open, ...)
    #[structopt(short = "l", long = "exclude_large_objects")]
    exclude_large_objects: bool,
    /// Schema of the objects
    #[structopt(default_value = "public", short = "s", long = "schema")]
    schema: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum State {
    Init,
    Comment,
    EmptyLine,
    ConsecutiveEmptyLine,
    ExcludedCopyBlock,
    EndOfExcludedCopyBlock,
    LargeObject,
    Statement,
}

impl State {
    fn next_state(&self, line: &String, opts: &Options) -> State {
        match line {
            l if l.is_empty() => match self {
                State::EmptyLine => State::ConsecutiveEmptyLine,
                State::ConsecutiveEmptyLine => State::ConsecutiveEmptyLine,
                _ => State::EmptyLine
            }
            l if l.starts_with("--") => State::Comment,
            l if l.starts_with("COPY ") && l.ends_with("FROM stdin;") && opts.excluded_copy_blocks
                .iter()
                .find(|&excluded_block| l.to_lowercase().contains(&format!("COPY {}.{} ", opts.schema, excluded_block).to_lowercase()))
                .is_some() => State::ExcludedCopyBlock,
            l if l.starts_with("COPY ") && l.ends_with("FROM stdin;") && !opts.included_copy_blocks.is_empty() && opts.included_copy_blocks
                .iter()
                .find(|&included_block| l.to_lowercase().contains(&format!("COPY {}.{} ", opts.schema, included_block).to_lowercase()))
                .is_none() => State::ExcludedCopyBlock,
            l if l.starts_with("\\.") => match self {
                State::ExcludedCopyBlock => State::EndOfExcludedCopyBlock,
                state => state.clone()
            },
            // keep the lo_create calls (oid colums in tables must work)
            l if l.to_lowercase().starts_with("select pg_catalog.lo_create") => State::Statement,
            l if l.to_lowercase().starts_with("select pg_catalog.lo_") ||  l.to_lowercase().starts_with("select pg_catalog.lowrite") => State::LargeObject,
            _ => match self {
                State::ExcludedCopyBlock => State::ExcludedCopyBlock,
                _ => State::Statement
            }
        }
    }

    fn must_include(&self, opts: &Options, prev_included_state: &State) -> bool {
        match self {
            State::Comment => false,
            State::ConsecutiveEmptyLine => false,
            State::ExcludedCopyBlock => false,
            State::EndOfExcludedCopyBlock => false,
            State::LargeObject if opts.exclude_large_objects => false,
            State::EmptyLine if prev_included_state == &State::EmptyLine => false,
            _ => true,
        }
    }
}

pub fn main() -> Result<()> {
    let opts: Options = Options::from_args();
    let mut prev_included_state: State = State::Init;
    let mut state: State = State::Init;

    let stdout = std::io::stdout();
    let lock = stdout.lock();
    let mut out = BufWriter::new(lock);

    for line in io::stdin().lock().lines() {
        match line {
            Ok(line) => {
                state = state.next_state(&line, &opts);
                if state.must_include(&opts, &prev_included_state) {
                    prev_included_state = state.clone();
                    writeln!(out, "{}", line)?;
                }
            }
            Err(e) => panic!("An IO error occurred {}", e)
        }
    }
    out.flush()?;
    Ok(())
}

