//! Domain types shared by the US News Map API and ingest pipeline.
//!
//! See `docs/design/` for the design these modules implement:
//! - [`american_stories`]: American Stories' second text of a page (05 §5.5.4, #218)
//! - [`ids`]: page keys and collision-free document ids (04 §4.2)
//! - [`time`]: day numbers and time buckets (05 §5.5, §5.7)
//! - [`text`]: text normalization shared by ingest and query parsing (04 §4.5)
//! - [`common_grams`]: common-word pairs, so phrases skip common words' positions (05 §5.5.3)
//! - [`decade`]: decade partitions, so date-limited searches skip splits (05 §5.5.5, #123)
//! - [`ja`]: Japanese tokens and folding for our own OCR (04 §4.8, #139)
//! - [`query`]: the user query language and its limits (06 §6.4)
//! - [`query_language`]: the language of a query's words, for its baseline (06 §6.3.3, #237)
//! - [`params`]: request parameters and canonical cache keys (06 §6.3)
//! - [`cube`]: the sparse place × bucket cube (06 §6.3.3)
//! - [`names`]: LoC's language and state names (04 §4.6)
//! - [`skew`]: where a term is printed more or less than corpus volume predicts (11)
//! - [`text_layout`]: a searched field per text, or one for both (05 §5.5.7, #283)

pub mod american_stories;
pub mod common_grams;
pub mod cube;
pub mod decade;
pub mod ids;
pub mod ja;
pub mod names;
pub mod params;
pub mod query;
pub mod query_language;
pub mod skew;
pub mod text;
pub mod text_layout;
pub mod time;
