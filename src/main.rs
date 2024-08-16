use std::io::{BufReader, BufWriter, Write};
use std::{io, io::prelude::*};
use structopt::StructOpt;

#[macro_use]
extern crate lazy_static;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;

#[derive(StructOpt, Debug)]
#[structopt(name = "pgdump_filter")]
struct Options {
    /// Exclude the listed copy block(s)
    #[structopt(
        short = "e",
        long = "excluded_copy_blocks",
        conflicts_with = "included_copy_blocks"
    )]
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
    #[structopt(
        default_value = "32",
        short = "b",
        long = "buffersize",
        help = "size of buffer in MB.  make sure this is enough to hold the longest line in the file."
    )]
    buffersize_in_mb: usize,
}

#[derive(Debug, Clone, PartialEq, Copy)]
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

lazy_static! {
    static ref COMMENT: &'static [u8] = b"--";
    static ref END_OF_COPY_BLOCK: &'static [u8] = b"\\.";
    static ref NEWLINE: &'static [u8] = b"\n";
    static ref COPY_BLOCK_PREFIX: &'static [u8] = b"COPY ";
    static ref COPY_BLOCK_SUFFIX: &'static [u8] = b"FROM stdin;\n";
    static ref LO_CREATE: &'static [u8] = b"SELECT pg_catalog.lo_create";
    static ref LO_FN: &'static [u8] = b"SELECT pg_catalog.lo_";
    static ref LO_WRITE: &'static [u8] = b"SELECT pg_catalog.lowrite";
}
impl State {
    fn next_state(&self, buf: &[u8], opts: &Options) -> Result<State> {
        match buf {
            // keep the lo_create calls (oid colums in tables must work)
            buf if buf.starts_with(*LO_CREATE) => Ok(State::Statement),
            buf if buf.starts_with(*LO_FN) || buf.starts_with(*LO_WRITE) => Ok(State::LargeObject),
            buf if buf.starts_with(*NEWLINE) => match self {
                State::EmptyLine => Ok(State::ConsecutiveEmptyLine),
                State::ConsecutiveEmptyLine => Ok(State::ConsecutiveEmptyLine),
                _ => Ok(State::EmptyLine),
            },
            buf if buf.starts_with(*COMMENT) => Ok(State::Comment),
            buf if buf.starts_with(*END_OF_COPY_BLOCK) => match self {
                State::ExcludedCopyBlock => Ok(State::EndOfExcludedCopyBlock),
                state => Ok(*state),
            },
            buf if buf.starts_with(*COPY_BLOCK_PREFIX) && buf.ends_with(*COPY_BLOCK_SUFFIX) => {
                let l = String::from_utf8(buf.to_vec())?.to_lowercase();
                match l {
                    l if opts.excluded_copy_blocks.iter().any(|excluded_block| {
                        l.contains(
                            &format!("COPY {}.{} ", opts.schema, excluded_block).to_lowercase(),
                        )
                    }) =>
                    {
                        Ok(State::ExcludedCopyBlock)
                    }
                    l if !opts.included_copy_blocks.is_empty()
                        && opts
                            .included_copy_blocks
                            .iter()
                            .find(|&included_block| {
                                l.to_ascii_lowercase().contains(
                                    &format!("COPY {}.{} ", opts.schema, included_block)
                                        .to_lowercase(),
                                )
                            })
                            .is_none() =>
                    {
                        Ok(State::ExcludedCopyBlock)
                    }
                    _ => Ok(State::Statement),
                }
            }
            _ => match self {
                State::ExcludedCopyBlock => Ok(State::ExcludedCopyBlock),
                _ => Ok(State::Statement),
            },
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
    let stdout = stdout.lock();
    let mut stdout = BufWriter::with_capacity(64 * 1024, stdout);

    let reader = io::stdin();
    let reader = reader.lock();
    let mut reader = BufReader::with_capacity(64 * 1024, reader);
    let mut buf: Vec<u8> = Vec::with_capacity(opts.buffersize_in_mb * 1024 * 1024);

    loop {
        buf.clear();

        let number_of_bytes_read = reader.read_until(b'\n', &mut buf)?;
        if number_of_bytes_read == 0 {
            break;
        }
        state = state.next_state(&buf, &opts)?;
        if state.must_include(&opts, &prev_included_state) {
            prev_included_state = state;
            stdout.write_all(&buf)?;
        }
    }
    stdout.flush()?;
    Ok(())
}
