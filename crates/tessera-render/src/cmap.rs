//! A bounded walk of a font's character map for the fallback coverage
//! index (#689).
//!
//! The index used swash's `Charmap::enumerate`, which walks each format-12
//! group from its start to its end as the font states them: a group
//! ending at `u32::MAX` costs four billion steps (1.66 s in release), a
//! 2 MiB `cmap` holds about 174 000 such groups, and a glyph offset near
//! `u32::MAX` overflows the glyph arithmetic, a panic in debug builds that
//! killed the index thread. [`enumerate`] reads the same subtable swash
//! picks and yields the same pairs for a well-formed table, but never
//! walks a code point at or past the code space, and never walks one
//! twice: each group or segment starts after the highest code point an
//! earlier one reached, as the format's sorted, non-overlapping order
//! requires of a valid font. A face therefore costs at most one step per
//! code point of the code space plus one per group or segment.

/// Format 4 (segment mapping to delta values, BMP only).
const FORMAT_SEGMENTS: u16 = 4;
/// Format 12 (segmented coverage, the full code space).
const FORMAT_GROUPS: u16 = 12;

/// What one [`enumerate`] call cost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Walk {
    /// Code points visited, whether or not they map to a glyph.
    pub(crate) code_points: u64,
    /// Format-12 groups or format-4 segments read.
    pub(crate) records: u64,
}

/// Calls `f(code_point, glyph)` for each code point below `limit` that
/// `cmap` maps, through the subtable swash's character map uses (a symbol
/// subtable first, else a Unicode format-12 one, else the first Unicode
/// format-4 one). `cmap` holds the `cmap` table from its first byte. A
/// subtable is read up to the end of `cmap`, not of the table's stated
/// length (swash reads to the end of the font data); the index passes
/// the table alone.
///
/// For a well-formed table the pairs are swash's `Charmap::enumerate`
/// pairs below `limit`, glyph 0 included where swash passes it (a
/// format-4 `idRangeOffset` glyph that `idDelta` takes to 0). Glyph
/// arithmetic wraps, as swash's does in release builds. A group or
/// segment is walked only above the highest code point an earlier one
/// reached, so overlapping or unsorted records (which a valid font never
/// has) are cut, not walked again.
pub(crate) fn enumerate(cmap: &[u8], limit: u32, mut f: impl FnMut(u32, u16)) -> Walk {
    let Some(subtable) = subtable(cmap).and_then(|offset| cmap.get(offset..)) else {
        return Walk::default();
    };
    match read_u16(subtable, 0).unwrap_or(0) {
        FORMAT_SEGMENTS => segments(subtable, limit, &mut f),
        FORMAT_GROUPS => groups(subtable, limit, &mut f),
        _ => Walk::default(),
    }
}

/// The offset, from the start of `cmap`, of the subtable swash's
/// `cmap::subtable` picks, with its quirks: a record whose subtable lies
/// outside the data ends the search with none, and the format is
/// compared by its low byte.
fn subtable(cmap: &[u8]) -> Option<usize> {
    let count = read_u16(cmap, 2)?;
    let mut best = None;
    for record in 0..usize::from(count) {
        let at = 4 + record * 8;
        let platform = read_u16(cmap, at)?;
        let encoding = read_u16(cmap, at + 2)?;
        let offset = usize::try_from(read_u32(cmap, at + 4)?).ok()?;
        let format = read_u16(cmap, offset)? & 0xFF;
        if format != FORMAT_SEGMENTS && format != FORMAT_GROUPS {
            continue;
        }
        let unicode = matches!((platform, encoding), (0, _) | (3, 1 | 10));
        if (platform, encoding) == (3, 0) {
            return Some(offset);
        }
        if unicode && (format == FORMAT_GROUPS || best.is_none()) {
            best = Some(offset);
        }
    }
    best
}

/// Walks a format-12 subtable.
fn groups(table: &[u8], limit: u32, f: &mut impl FnMut(u32, u16)) -> Walk {
    let mut walk = Walk::default();
    let count = read_u32(table, 12).map_or(0, |n| usize::try_from(n).unwrap_or(usize::MAX));
    if !check_range(table, 16, count.saturating_mul(12)) {
        return walk;
    }
    // The lowest code point no earlier group reached.
    let mut next = 0_u32;
    for group in table[16..].as_chunks::<12>().0.iter().take(count) {
        walk.records += 1;
        let start = u32_at(group, 0);
        let end = u32_at(group, 4);
        let glyph = u32_at(group, 8);
        let first = start.max(next);
        let last = end.min(limit.saturating_sub(1));
        if first > last {
            continue;
        }
        for cp in first..=last {
            #[expect(
                clippy::cast_possible_truncation,
                reason = "glyph ids are 16 bits; swash keeps the low half too"
            )]
            let id = glyph.wrapping_add(cp - start) as u16;
            if id != 0 {
                f(cp, id);
            }
        }
        walk.code_points += u64::from(last - first) + 1;
        next = last + 1;
    }
    walk
}

/// Walks a format-4 subtable.
fn segments(table: &[u8], limit: u32, f: &mut impl FnMut(u32, u16)) -> Walk {
    let mut walk = Walk::default();
    let seg_x2 = usize::from(read_u16(table, 6).unwrap_or(0));
    let count = seg_x2 / 2;
    if !check_range(table, 0, 16 + seg_x2 * 4) {
        return walk;
    }
    let ends = 14;
    let starts = ends + seg_x2 + 2;
    let deltas = starts + seg_x2;
    let ranges = deltas + seg_x2;
    let mut next = 0_u32;
    for segment in 0..count {
        walk.records += 1;
        let (Some(start), Some(end), Some(delta), Some(range)) = (
            read_u16(table, starts + segment * 2),
            read_u16(table, ends + segment * 2),
            read_u16(table, deltas + segment * 2),
            read_u16(table, ranges + segment * 2),
        ) else {
            continue;
        };
        let first = u32::from(start).max(next);
        let last = u32::from(end).min(limit.saturating_sub(1));
        if first > last {
            continue;
        }
        let range_base = ranges + segment * 2 + usize::from(range);
        for cp in first..=last {
            if range == 0 {
                // `cp` is at most `end`, a `u16`.
                let id = u16::try_from(cp).unwrap_or(u16::MAX).wrapping_add(delta);
                if id != 0 {
                    f(cp, id);
                }
            } else if let Some(id) =
                read_u16(table, range_base + (cp - u32::from(start)) as usize * 2)
                && id != 0
            {
                f(cp, id.wrapping_add(delta));
            }
        }
        walk.code_points += u64::from(last - first) + 1;
        next = last + 1;
    }
    walk
}

/// swash's `Bytes::check_range`: `offset` lies inside `data` and `len`
/// bytes follow it.
fn check_range(data: &[u8], offset: usize, len: usize) -> bool {
    offset < data.len() && data.len() - offset >= len
}

fn read_u16(data: &[u8], at: usize) -> Option<u16> {
    let bytes = data.get(at..at.checked_add(2)?)?;
    Some(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn read_u32(data: &[u8], at: usize) -> Option<u32> {
    let bytes = data.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// The big-endian `u32` at `at` in a record known to hold it.
fn u32_at(record: &[u8], at: usize) -> u32 {
    read_u32(record, at).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// The code space the coverage index walks.
    const LIMIT: u32 = 0x11_0000;

    /// A `cmap` table whose records are `(platform, encoding, subtable)`,
    /// the subtables laid out after the records in order.
    fn cmap_of(records: &[(u16, u16, Vec<u8>)]) -> Vec<u8> {
        let count = u16::try_from(records.len()).expect("a test table");
        let mut cmap = [0_u16.to_be_bytes(), count.to_be_bytes()].concat();
        let mut offset = 4 + 8 * records.len();
        let mut tables = Vec::new();
        for (platform, encoding, table) in records {
            cmap.extend_from_slice(&platform.to_be_bytes());
            cmap.extend_from_slice(&encoding.to_be_bytes());
            cmap.extend_from_slice(&u32::try_from(offset).expect("small").to_be_bytes());
            offset += table.len();
            tables.extend_from_slice(table);
        }
        cmap.extend_from_slice(&tables);
        cmap
    }

    /// A format-12 (or, with `format` 13, format-13) subtable of
    /// `groups`, each (first, last, glyph).
    fn groups_table(format: u16, groups: &[(u32, u32, u32)]) -> Vec<u8> {
        let count = u32::try_from(groups.len()).expect("a test table");
        let mut table = [format.to_be_bytes(), 0_u16.to_be_bytes()].concat();
        for field in [16 + 12 * count, 0, count] {
            table.extend_from_slice(&field.to_be_bytes());
        }
        for &(start, end, glyph) in groups {
            for field in [start, end, glyph] {
                table.extend_from_slice(&field.to_be_bytes());
            }
        }
        table
    }

    /// A format-4 segment: start, end, idDelta, idRangeOffset.
    type Segment = (u16, u16, u16, u16);

    /// A format-4 subtable of `segments`, each (start, end, delta,
    /// idRangeOffset), followed by `glyphs` (the glyph id array).
    fn segments_table(segments: &[Segment], glyphs: &[u16]) -> Vec<u8> {
        let seg_x2 = u16::try_from(segments.len() * 2).expect("a test table");
        let len = u16::try_from(16 + segments.len() * 8 + glyphs.len() * 2).unwrap_or(u16::MAX);
        let mut words = vec![4, len, 0, seg_x2, 0, 0, 0];
        words.extend(segments.iter().map(|s| s.1));
        words.push(0);
        words.extend(segments.iter().map(|s| s.0));
        words.extend(segments.iter().map(|s| s.2));
        words.extend(segments.iter().map(|s| s.3));
        words.extend_from_slice(glyphs);
        words.iter().flat_map(|w| w.to_be_bytes()).collect()
    }

    /// What [`enumerate`] yields for `cmap`, and its cost.
    fn ours(cmap: &[u8]) -> (Vec<(u32, u16)>, Walk) {
        let mut pairs = Vec::new();
        let walk = enumerate(cmap, LIMIT, |cp, glyph| pairs.push((cp, glyph)));
        (pairs, walk)
    }

    /// What swash's `Charmap::enumerate` yields for `cmap` below the code
    /// space, read as the one table of a font, as the index read it before.
    fn swash(cmap: &[u8]) -> Vec<(u32, u16)> {
        let mut font = Vec::new();
        font.extend_from_slice(&0x0001_0000_u32.to_be_bytes());
        font.extend_from_slice(&1_u16.to_be_bytes());
        font.extend_from_slice(&[0; 6]);
        font.extend_from_slice(b"cmap");
        font.extend_from_slice(&[0; 4]);
        font.extend_from_slice(&28_u32.to_be_bytes());
        font.extend_from_slice(&u32::try_from(cmap.len()).expect("small").to_be_bytes());
        font.extend_from_slice(cmap);
        let mut pairs = Vec::new();
        swash::FontRef::from_offset(&font, 0)
            .expect("a one-table font")
            .charmap()
            .enumerate(|cp, glyph| {
                if cp < LIMIT {
                    pairs.push((cp, glyph));
                }
            });
        pairs
    }

    /// #689: a glyph offset of `u32::MAX` wraps (swash's arithmetic
    /// panicked in debug builds): 'A' is glyph 0xFFFF, 'B' glyph 0 and
    /// left out, 'C' glyph 1.
    #[test]
    fn a_glyph_offset_of_u32_max_wraps() {
        let cmap = cmap_of(&[(3, 10, groups_table(12, &[(0x41, 0x43, u32::MAX)]))]);
        let (pairs, walk) = ours(&cmap);
        assert_eq!(pairs, [(0x41, 0xFFFF), (0x43, 1)]);
        assert_eq!(
            walk,
            Walk {
                code_points: 3,
                records: 1
            }
        );
    }

    /// #689: a group reaching `u32::MAX` is cut at the code space, and the
    /// most groups a 2 MiB `cmap` holds, all overlapping, cost one walk of
    /// the code space plus one step per group.
    #[test]
    fn overlapping_groups_reaching_u32_max_cost_one_walk_of_the_code_space() {
        let count = (2 << 20) / 12;
        let cmap = cmap_of(&[(3, 10, groups_table(12, &vec![(0, u32::MAX, 0); count]))]);
        let mut last = None;
        let mut calls = 0_u64;
        let walk = enumerate(&cmap, LIMIT, |cp, _| {
            assert!(last.is_none_or(|last| cp > last), "{cp:#x} after {last:?}");
            last = Some(cp);
            calls += 1;
        });
        assert_eq!(
            walk,
            Walk {
                code_points: u64::from(LIMIT),
                records: count as u64
            }
        );
        assert_eq!(last, Some(LIMIT - 1));
        // The glyph, the code point's low 16 bits, is 0 at each 0x10000th.
        assert_eq!(calls, u64::from(LIMIT) - 17);
    }

    /// #689: so do the most format-4 segments, all overlapping: one walk
    /// of the BMP plus one step per segment.
    #[test]
    fn overlapping_format4_segments_cost_one_walk_of_the_bmp() {
        let count = 0x7FFF;
        let cmap = cmap_of(&[(3, 1, segments_table(&vec![(0, 0xFFFF, 1, 0); count], &[]))]);
        let (pairs, walk) = ours(&cmap);
        assert_eq!(
            walk,
            Walk {
                code_points: 0x1_0000,
                records: count as u64
            }
        );
        assert_eq!(pairs.len(), 0xFFFF, "all but 0xFFFF, whose glyph is 0");
    }

    /// #689: swash never picks a format-13 subtable, so neither does the
    /// walk; a format-13 group reaching `u32::MAX` costs nothing.
    #[test]
    fn a_format13_subtable_is_not_walked() {
        let cmap = cmap_of(&[(3, 10, groups_table(13, &[(0, u32::MAX, 1); 8]))]);
        assert_eq!(ours(&cmap), (Vec::new(), Walk::default()));
    }

    /// The subtable is swash's pick: a symbol (3, 0) subtable first, else
    /// a Unicode format-12 one over the first Unicode format-4 one; a
    /// format compared by its low byte (0x010C reads as 12) is picked but
    /// walks nothing.
    #[test]
    fn the_subtable_is_the_one_swash_picks() {
        let bmp = segments_table(&[(0x41, 0x41, 0, 0)], &[]);
        let full = groups_table(12, &[(0x42, 0x42, 7)]);
        let symbol = groups_table(12, &[(0x43, 0x43, 9)]);
        let mut odd = full.clone();
        odd[0] = 1;
        for (records, expected) in [
            (
                vec![(3, 1, bmp.clone()), (0, 4, full.clone())],
                vec![(0x42, 7)],
            ),
            (
                vec![(3, 1, bmp.clone()), (1, 0, full.clone())],
                vec![(0x41, 0x41)],
            ),
            (vec![(0, 4, full.clone()), (3, 0, symbol)], vec![(0x43, 9)]),
            (vec![(3, 1, bmp), (3, 10, odd)], vec![]),
        ] {
            let cmap = cmap_of(&records);
            assert_eq!(ours(&cmap).0, expected);
            assert_eq!(swash(&cmap), expected);
        }
    }

    /// Sorted, non-overlapping format-12 groups whose glyph arithmetic
    /// does not overflow (swash panics in debug builds when it does),
    /// some reaching past the code space.
    fn valid_groups() -> impl Strategy<Value = Vec<(u32, u32, u32)>> {
        (
            0_u32..0x11_0100,
            prop::collection::vec((0_u32..0x8000, 0_u32..300, any::<u32>()), 0..24),
        )
            .prop_map(|(base, spec)| {
                let mut next = base;
                spec.into_iter()
                    .map(|(gap, len, glyph)| {
                        let start = next + gap;
                        let end = start + len;
                        next = end + 1;
                        (start, end, glyph.min(u32::MAX - end))
                    })
                    .collect()
            })
    }

    /// Sorted, non-overlapping format-4 segments, any deltas and
    /// idRangeOffsets, and a glyph id array.
    fn valid_segments() -> impl Strategy<Value = (Vec<Segment>, Vec<u16>)> {
        (
            prop::collection::vec(
                (
                    0_u16..2000,
                    0_u16..64,
                    any::<u16>(),
                    any::<u16>(),
                    any::<bool>(),
                ),
                0..24,
            ),
            prop::collection::vec(any::<u16>(), 0..64),
        )
            .prop_map(|(spec, glyphs)| {
                let mut next = 0_u16;
                let mut segments = Vec::new();
                let count = spec.len();
                for (i, (gap, len, delta, range, ranged)) in spec.into_iter().enumerate() {
                    let Some(start) = next.checked_add(gap) else {
                        break;
                    };
                    let Some(end) = start.checked_add(len) else {
                        break;
                    };
                    // A ranged segment's offset points into the glyph array
                    // (or past it, which both walks read as nothing).
                    let range = if ranged {
                        u16::try_from((count - i) * 2)
                            .unwrap_or(0)
                            .wrapping_add(range % 160)
                    } else {
                        0
                    };
                    segments.push((start, end, delta, range));
                    let Some(after) = end.checked_add(1) else {
                        break;
                    };
                    next = after;
                }
                (segments, glyphs)
            })
    }

    proptest! {
        /// For a well-formed format-12 table the walk yields swash's pairs
        /// below the code space.
        #[test]
        fn format12_walk_matches_swash(groups in valid_groups()) {
            let cmap = cmap_of(&[(3, 10, groups_table(12, &groups))]);
            prop_assert_eq!(ours(&cmap).0, swash(&cmap));
        }

        /// For a well-formed format-4 table the walk yields swash's pairs.
        #[test]
        fn format4_walk_matches_swash((segments, glyphs) in valid_segments()) {
            let cmap = cmap_of(&[(3, 1, segments_table(&segments, &glyphs))]);
            prop_assert_eq!(ours(&cmap).0, swash(&cmap));
        }

        /// Any groups (unsorted, overlapping, reaching `u32::MAX`, any
        /// glyph offset): no panic, code points strictly rising and below
        /// the code space, at most one walk of it.
        #[test]
        fn any_groups_walk_at_most_the_code_space_once(
            groups in prop::collection::vec(
                (any::<u32>(), any::<u32>(), any::<u32>()).prop_map(|(a, b, g)| (a % 0x12_0000, b, g)),
                0..16,
            ),
        ) {
            let cmap = cmap_of(&[(0, 4, groups_table(12, &groups))]);
            let mut last = None;
            let walk = enumerate(&cmap, LIMIT, |cp, _| {
                assert!(cp < LIMIT && last.is_none_or(|last| cp > last));
                last = Some(cp);
            });
            prop_assert!(walk.code_points <= u64::from(LIMIT));
            prop_assert_eq!(walk.records, groups.len() as u64);
        }

        /// Any bytes: no panic, and at most one walk of the code space.
        #[test]
        fn any_bytes_walk_at_most_the_code_space_once(
            bytes in prop::collection::vec(any::<u8>(), 0..512),
        ) {
            let walk = enumerate(&bytes, LIMIT, |cp, _| assert!(cp < LIMIT));
            prop_assert!(walk.code_points <= u64::from(LIMIT));
        }
    }
}
