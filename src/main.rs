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
        default_value = "8",
        short = "b",
        long = "buffersize",
        help = "size of buffer in MB for line buffering (default: 8MB)"
    )]
    buffersize_in_mb: usize,
}

// Pre-computed patterns for faster matching
struct FilterPatterns {
    excluded_patterns: Vec<Vec<u8>>,
    included_patterns: Vec<Vec<u8>>,
}

impl FilterPatterns {
    fn new(opts: &Options) -> Self {
        let excluded_patterns = opts
            .excluded_copy_blocks
            .iter()
            .map(|block| {
                format!("copy {}.{} ", opts.schema, block)
                    .to_lowercase()
                    .into_bytes()
            })
            .collect();

        let included_patterns = opts
            .included_copy_blocks
            .iter()
            .map(|block| {
                format!("copy {}.{} ", opts.schema, block)
                    .to_lowercase()
                    .into_bytes()
            })
            .collect();

        FilterPatterns {
            excluded_patterns,
            included_patterns,
        }
    }
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

// Case-insensitive contains for byte slices
#[inline]
fn contains_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}
impl State {
    fn next_state(&self, buf: &[u8], patterns: &FilterPatterns) -> Result<State> {
        match buf {
            // keep the lo_create calls (oid columns in tables must work)
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
                // Check exclusions using pre-computed patterns
                if patterns
                    .excluded_patterns
                    .iter()
                    .any(|pattern| contains_ignore_ascii_case(buf, pattern))
                {
                    return Ok(State::ExcludedCopyBlock);
                }

                // Check inclusions if specified
                if !patterns.included_patterns.is_empty()
                    && !patterns
                        .included_patterns
                        .iter()
                        .any(|pattern| contains_ignore_ascii_case(buf, pattern))
                {
                    return Ok(State::ExcludedCopyBlock);
                }

                Ok(State::Statement)
            }
            _ => match self {
                State::ExcludedCopyBlock => Ok(State::ExcludedCopyBlock),
                _ => Ok(State::Statement),
            },
        }
    }

    #[inline]
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

    let patterns = FilterPatterns::new(&opts);

    let mut prev_included_state: State = State::Init;
    let mut state: State = State::Init;

    let stdout = std::io::stdout();
    let stdout = stdout.lock();
    // Increase buffer sizes for better throughput
    let mut stdout = BufWriter::with_capacity(256 * 1024, stdout);

    let reader = io::stdin();
    let reader = reader.lock();
    let mut reader = BufReader::with_capacity(256 * 1024, reader);

    let mut buf: Vec<u8> = Vec::with_capacity(opts.buffersize_in_mb * 1024 * 1024);

    loop {
        buf.clear();

        let number_of_bytes_read = reader.read_until(b'\n', &mut buf)?;
        if number_of_bytes_read == 0 {
            break;
        }

        state = state.next_state(&buf, &patterns)?;
        if state.must_include(&opts, &prev_included_state) {
            prev_included_state = state;
            stdout.write_all(&buf)?;
        }
    }
    stdout.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_contains_ignore_ascii_case_basic() {
        let haystack = b"COPY public.users FROM stdin;\n";
        let needle = b"copy public.users ";
        assert!(contains_ignore_ascii_case(haystack, needle));
    }

    #[test]
    fn test_contains_ignore_ascii_case_mixed_case() {
        let haystack = b"Copy Public.Users FROM stdin;\n";
        let needle = b"copy public.users ";
        assert!(contains_ignore_ascii_case(haystack, needle));
    }

    #[test]
    fn test_contains_ignore_ascii_case_lowercase() {
        let haystack = b"copy public.users from stdin;\n";
        let needle = b"copy public.users ";
        assert!(contains_ignore_ascii_case(haystack, needle));
    }

    #[test]
    fn test_contains_ignore_ascii_case_not_found() {
        let haystack = b"COPY public.orders FROM stdin;\n";
        let needle = b"copy public.users ";
        assert!(!contains_ignore_ascii_case(haystack, needle));
    }

    #[test]
    fn test_contains_ignore_ascii_case_partial_match() {
        let haystack = b"COPY public.user_sessions FROM stdin;\n";
        let needle = b"copy public.users ";
        // Should not match - "user_sessions" contains "users" but pattern includes space
        assert!(!contains_ignore_ascii_case(haystack, needle));
    }

    #[test]
    fn test_filter_patterns_excluded() {
        let opts = Options {
            excluded_copy_blocks: vec!["users".to_string(), "sessions".to_string()],
            included_copy_blocks: vec![],
            exclude_large_objects: false,
            schema: "public".to_string(),
            buffersize_in_mb: 8,
        };

        let patterns = FilterPatterns::new(&opts);

        assert_eq!(patterns.excluded_patterns.len(), 2);
        assert_eq!(patterns.excluded_patterns[0], b"copy public.users ");
        assert_eq!(patterns.excluded_patterns[1], b"copy public.sessions ");
        assert_eq!(patterns.included_patterns.len(), 0);
    }

    #[test]
    fn test_filter_patterns_included() {
        let opts = Options {
            excluded_copy_blocks: vec![],
            included_copy_blocks: vec!["users".to_string(), "orders".to_string()],
            exclude_large_objects: false,
            schema: "public".to_string(),
            buffersize_in_mb: 8,
        };

        let patterns = FilterPatterns::new(&opts);

        assert_eq!(patterns.excluded_patterns.len(), 0);
        assert_eq!(patterns.included_patterns.len(), 2);
        assert_eq!(patterns.included_patterns[0], b"copy public.users ");
        assert_eq!(patterns.included_patterns[1], b"copy public.orders ");
    }

    #[test]
    fn test_filter_patterns_custom_schema() {
        let opts = Options {
            excluded_copy_blocks: vec!["users".to_string()],
            included_copy_blocks: vec![],
            exclude_large_objects: false,
            schema: "myschema".to_string(),
            buffersize_in_mb: 8,
        };

        let patterns = FilterPatterns::new(&opts);

        assert_eq!(patterns.excluded_patterns[0], b"copy myschema.users ");
    }

    #[test]
    fn test_state_transition_comment() {
        let opts = Options {
            excluded_copy_blocks: vec![],
            included_copy_blocks: vec![],
            exclude_large_objects: false,
            schema: "public".to_string(),
            buffersize_in_mb: 8,
        };
        let patterns = FilterPatterns::new(&opts);

        let state = State::Init;
        let line = b"-- This is a comment\n";
        let new_state = state.next_state(line, &patterns).unwrap();
        assert_eq!(new_state, State::Comment);
    }

    #[test]
    fn test_state_transition_copy_block_excluded() {
        let opts = Options {
            excluded_copy_blocks: vec!["users".to_string()],
            included_copy_blocks: vec![],
            exclude_large_objects: false,
            schema: "public".to_string(),
            buffersize_in_mb: 8,
        };
        let patterns = FilterPatterns::new(&opts);

        let state = State::Init;
        let line = b"COPY public.users (id, name) FROM stdin;\n";
        let new_state = state.next_state(line, &patterns).unwrap();
        assert_eq!(new_state, State::ExcludedCopyBlock);
    }

    #[test]
    fn test_state_transition_copy_block_included() {
        let opts = Options {
            excluded_copy_blocks: vec![],
            included_copy_blocks: vec!["users".to_string()],
            exclude_large_objects: false,
            schema: "public".to_string(),
            buffersize_in_mb: 8,
        };
        let patterns = FilterPatterns::new(&opts);

        let state = State::Init;
        let line = b"COPY public.users (id, name) FROM stdin;\n";
        let new_state = state.next_state(line, &patterns).unwrap();
        assert_eq!(new_state, State::Statement);
    }

    #[test]
    fn test_state_transition_copy_block_not_in_included_list() {
        let opts = Options {
            excluded_copy_blocks: vec![],
            included_copy_blocks: vec!["users".to_string()],
            exclude_large_objects: false,
            schema: "public".to_string(),
            buffersize_in_mb: 8,
        };
        let patterns = FilterPatterns::new(&opts);

        let state = State::Init;
        let line = b"COPY public.orders (id, total) FROM stdin;\n";
        let new_state = state.next_state(line, &patterns).unwrap();
        assert_eq!(new_state, State::ExcludedCopyBlock);
    }

    #[test]
    fn test_state_transition_large_object() {
        let opts = Options {
            excluded_copy_blocks: vec![],
            included_copy_blocks: vec![],
            exclude_large_objects: false,
            schema: "public".to_string(),
            buffersize_in_mb: 8,
        };
        let patterns = FilterPatterns::new(&opts);

        let state = State::Init;
        let line = b"SELECT pg_catalog.lo_open('12345', 131072);\n";
        let new_state = state.next_state(line, &patterns).unwrap();
        assert_eq!(new_state, State::LargeObject);
    }

    #[test]
    fn test_state_transition_lo_create() {
        let opts = Options {
            excluded_copy_blocks: vec![],
            included_copy_blocks: vec![],
            exclude_large_objects: false,
            schema: "public".to_string(),
            buffersize_in_mb: 8,
        };
        let patterns = FilterPatterns::new(&opts);

        let state = State::Init;
        let line = b"SELECT pg_catalog.lo_create('12345');\n";
        let new_state = state.next_state(line, &patterns).unwrap();
        assert_eq!(new_state, State::Statement);
    }

    #[test]
    fn test_must_include_comment() {
        let opts = Options {
            excluded_copy_blocks: vec![],
            included_copy_blocks: vec![],
            exclude_large_objects: false,
            schema: "public".to_string(),
            buffersize_in_mb: 8,
        };

        let state = State::Comment;
        assert!(!state.must_include(&opts, &State::Init));
    }

    #[test]
    fn test_must_include_excluded_copy_block() {
        let opts = Options {
            excluded_copy_blocks: vec![],
            included_copy_blocks: vec![],
            exclude_large_objects: false,
            schema: "public".to_string(),
            buffersize_in_mb: 8,
        };

        let state = State::ExcludedCopyBlock;
        assert!(!state.must_include(&opts, &State::Init));
    }

    #[test]
    fn test_must_include_large_object_when_excluded() {
        let opts = Options {
            excluded_copy_blocks: vec![],
            included_copy_blocks: vec![],
            exclude_large_objects: true,
            schema: "public".to_string(),
            buffersize_in_mb: 8,
        };

        let state = State::LargeObject;
        assert!(!state.must_include(&opts, &State::Init));
    }

    #[test]
    fn test_must_include_large_object_when_not_excluded() {
        let opts = Options {
            excluded_copy_blocks: vec![],
            included_copy_blocks: vec![],
            exclude_large_objects: false,
            schema: "public".to_string(),
            buffersize_in_mb: 8,
        };

        let state = State::LargeObject;
        assert!(state.must_include(&opts, &State::Init));
    }

    #[test]
    fn test_must_include_statement() {
        let opts = Options {
            excluded_copy_blocks: vec![],
            included_copy_blocks: vec![],
            exclude_large_objects: false,
            schema: "public".to_string(),
            buffersize_in_mb: 8,
        };

        let state = State::Statement;
        assert!(state.must_include(&opts, &State::Init));
    }

    #[test]
    fn test_must_include_consecutive_empty_lines() {
        let opts = Options {
            excluded_copy_blocks: vec![],
            included_copy_blocks: vec![],
            exclude_large_objects: false,
            schema: "public".to_string(),
            buffersize_in_mb: 8,
        };

        let state = State::ConsecutiveEmptyLine;
        assert!(!state.must_include(&opts, &State::Init));
    }

    #[test]
    fn test_must_include_empty_line_after_empty_line() {
        let opts = Options {
            excluded_copy_blocks: vec![],
            included_copy_blocks: vec![],
            exclude_large_objects: false,
            schema: "public".to_string(),
            buffersize_in_mb: 8,
        };

        let state = State::EmptyLine;
        assert!(!state.must_include(&opts, &State::EmptyLine));
        assert!(state.must_include(&opts, &State::Statement));
    }
}
