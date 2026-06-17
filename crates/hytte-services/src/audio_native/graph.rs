//! `PipeWire` graph-edge helpers: stream→sink and stream→source routing.
//!
//! These functions are pure (no side-effects, no `Rc` borrows) and map a
//! link cache to routing ids. Isolated here so they can be unit-tested
//! without any `pipewire` daemon dependency.

use std::collections::HashMap;

use super::types::LinkEdge;

/// Resolve a playback stream's target sink id by scanning the link cache
/// for an edge whose `output_node` matches the stream. Returns the input
/// (sink) side of the first match. `PipeWire` usually creates one link per
/// stereo pair, so any of them works.
///
/// A stream typically has multiple ports → multiple links, but every
/// link goes to the same sink, so the first match is correct.
pub(super) fn resolve_link_dest(links: &HashMap<u32, LinkEdge>, stream_id: u32) -> u32 {
    links
        .values()
        .find(|e| e.output_node == stream_id)
        .map_or(0, |e| e.input_node)
}

/// Mirror of [`resolve_link_dest`] for record streams: the stream is the
/// link's *input*, so the source id is `output_node`.
pub(super) fn resolve_link_source(links: &HashMap<u32, LinkEdge>, stream_id: u32) -> u32 {
    links
        .values()
        .find(|e| e.input_node == stream_id)
        .map_or(0, |e| e.output_node)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Playback-stream routing: link goes stream → sink, so the link's
    /// `output_node` is the stream and `input_node` is the sink.
    #[test]
    fn resolve_link_dest_finds_sink() {
        let mut links = HashMap::new();
        links.insert(
            100,
            LinkEdge {
                output_node: 42,
                input_node: 10,
            },
        );
        assert_eq!(resolve_link_dest(&links, 42), 10);
    }

    /// Record-stream routing reverses: link goes source → stream, so the
    /// stream is the link's `input_node`.
    #[test]
    fn resolve_link_source_finds_source() {
        let mut links = HashMap::new();
        links.insert(
            100,
            LinkEdge {
                output_node: 5,
                input_node: 99,
            },
        );
        assert_eq!(resolve_link_source(&links, 99), 5);
    }

    /// No matching link → 0 sentinel, so the audio modal can show the
    /// stream without crashing the routing path. Stale state is allowed
    /// in the brief window between a link removal and the corresponding
    /// stream's WindowsChanged-equivalent event.
    #[test]
    fn resolve_link_returns_zero_when_no_match() {
        let links = HashMap::new();
        assert_eq!(resolve_link_dest(&links, 42), 0);
        assert_eq!(resolve_link_source(&links, 42), 0);
    }
}
