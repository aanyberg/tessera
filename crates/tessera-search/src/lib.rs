//! Search over terminal content.
//!
//! Compiles literal and regex queries under bounded size
//! ([`MAX_QUERY_BYTES`], [`REGEX_SIZE_LIMIT`], [`DFA_SIZE_LIMIT`]), builds
//! the text of a run of grid rows ([`Haystack`]) and maps each regex match
//! back to the cells it covers ([`scan`]). The search unit is the logical
//! line: rows a soft wrap joins are one text, a hard line break ends it,
//! so `^` and `$` anchor to the logical line (ADR 0038).
//!
//! The crate holds no terminal lock and keeps no state between calls: the
//! application core copies rows out of the terminal a bounded chunk at a
//! time and runs the query on the copy.

#![forbid(unsafe_code)]

mod engine;

pub use engine::{
    DFA_SIZE_LIMIT, Haystack, MAX_HAYSTACK_BYTES, MAX_PIECE_CELLS, MAX_QUERY_BYTES,
    MAX_SEGMENT_PERIOD, MAX_SEGMENT_ROWS, MIN_PIECE_BYTES, Match, Piece, PieceLimit, PieceScan,
    Query, QueryError, QueryOptions, REGEX_SIZE_LIMIT, Rows, Segment, SegmentMatch,
    is_segment_start, scan, scan_piece, scan_segment, segment_period, segments,
};
