//! X11 colour names for the OSC colour specs (#187, ADR 0024 D-NAMES).
//!
//! [`lookup`] resolves a name the way xterm's `XParseColor` does through
//! the X server's `OsLookupColor` (`os/oscolor.c`): ASCII
//! case-insensitively, over the whole string, with spaces significant.
//! `dark slate gray` and `DarkSlateGray` both match because the table
//! lists both spellings; `dark slategray`, ` red` and `red ` do not. Only
//! A to Z fold, so a byte of 0x80 or above never matches (no Unicode case
//! folding: U+212A KELVIN SIGN is not `k`).
//!
//! The table is the X.org `rgb.txt`, lowercased: where X11 and CSS disagree (`gray`,
//! `grey`, `green`, `maroon`, `purple`) the X11 value wins, and the CSS
//! values stay reachable as `web gray`, `web green`, `web maroon` and
//! `web purple`.
//!
//! # Bounds
//!
//! A name longer than [`MAX_NAME_LEN`] bytes, the longest entry, is
//! rejected before the search (docs/security.md S5). The search is a
//! binary search, among the names with the input's first letter, over a
//! `static` array of 8-byte name prefixes computed at compile time, then a
//! whole compare of the few names that share the input's prefix, with
//! the input folded into a stack buffer: no allocation and no lazy
//! initialisation. Against the same flood of `rgb:` specs, a flood of
//! sets by name retires 0.95 times the instructions but takes 1.17 times
//! the cycles (1.30 for a random mix of all the names): the search's
//! dependent loads cost about 25 to 30 ns per name, once per sequence,
//! never per byte (ADR 0024 D-NAMES).
//!
//! # Source and licence
//!
//! [`NAMES`] holds every entry of `rgb.txt` from xorg-rgb
//! (<https://gitlab.freedesktop.org/xorg/app/rgb>), blob
//! `7096643760e1a9bcb7887b9a8b6c17f75bf500f6`, last changed by commit
//! `0d2caecebf0e2a10994c22960921f366dd98d19a` (2023-07-10), tag
//! `rgb-1.1.1`: 782 entries, the same set as the X server's built-in
//! table (`os/oscolor.c` `BuiltinColors[]`, xorg-server 21.1.13). The
//! names are ASCII-lowercased (the server's own table is too) and sorted
//! by byte. The table is distributed under the xorg-rgb notice:
//!
//! ```text
//! Copyright 1985, 1989, 1998  The Open Group
//!
//! Permission to use, copy, modify, distribute, and sell this software and its
//! documentation for any purpose is hereby granted without fee, provided that
//! the above copyright notice appear in all copies and that both that
//! copyright notice and this permission notice appear in supporting
//! documentation.
//!
//! The above copyright notice and this permission notice shall be included
//! in all copies or substantial portions of the Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
//! OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
//! MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
//! IN NO EVENT SHALL THE OPEN GROUP BE LIABLE FOR ANY CLAIM, DAMAGES OR
//! OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE,
//! ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR
//! OTHER DEALINGS IN THE SOFTWARE.
//!
//! Except as contained in this notice, the name of The Open Group shall
//! not be used in advertising or otherwise to promote the sale, use or
//! other dealings in this Software without prior written authorization
//! from The Open Group.
//!
//! Copyright (c) 1994, 2008, Oracle and/or its affiliates.
//!
//! Permission is hereby granted, free of charge, to any person obtaining a
//! copy of this software and associated documentation files (the "Software"),
//! to deal in the Software without restriction, including without limitation
//! the rights to use, copy, modify, merge, publish, distribute, sublicense,
//! and/or sell copies of the Software, and to permit persons to whom the
//! Software is furnished to do so, subject to the following conditions:
//!
//! The above copyright notice and this permission notice (including the next
//! paragraph) shall be included in all copies or substantial portions of the
//! Software.
//!
//! THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
//! IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
//! FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.  IN NO EVENT SHALL
//! THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
//! LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
//! FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
//! DEALINGS IN THE SOFTWARE.
//! ```

use crate::Rgb;

/// The longest name in [`NAMES`], in bytes (`light goldenrod yellow`). A
/// longer name is rejected before the search (docs/security.md S5).
pub(crate) const MAX_NAME_LEN: usize = 22;

/// The number of entries in [`NAMES`]: all of `rgb.txt` (752 before 2014,
/// then 10 CSS Color 4 names and 20 `x11 ...` / `web ...` aliases).
pub(crate) const NAME_COUNT: usize = 782;

/// The colour `name` stands for, or `None` if it is not an X11 colour
/// name. See the module documentation for the rule.
pub(crate) fn lookup(name: &[u8]) -> Option<Rgb> {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return None;
    }
    // Zero-padded, so its first 8 bytes are the name's `prefix_key`; folded
    // whole, a fixed length the compiler vectorises.
    let mut buf = [0; MAX_NAME_LEN];
    buf[..name.len()].copy_from_slice(name);
    buf.make_ascii_lowercase();
    let folded = &buf[..name.len()];
    let letter = usize::from(folded[0].checked_sub(b'a')?);
    let (&start, &end) = (FIRST.get(letter)?, FIRST.get(letter + 1)?);
    let (start, end) = (usize::from(start), usize::from(end));
    let (prefix, _) = buf.split_first_chunk::<8>()?;
    let key = u64::from_be_bytes(*prefix);
    let first = start + KEYS[start..end].partition_point(|&entry| entry < key);
    KEYS[first..end]
        .iter()
        .take_while(|&&entry| entry == key)
        .zip(&NAMES[first..])
        .find(|&(_, &(entry, _))| entry.as_bytes() == folded)
        .map(|(_, &(_, rgb))| rgb)
}

/// `FIRST[c]..FIRST[c + 1]` are the indexes in [`NAMES`] of the names
/// that start with the letter `b'a' + c` (every name starts with one).
static FIRST: [u16; 27] = first_letter_index();

/// Computes [`FIRST`] at compile time.
// `From` is not const: the casts are checked by the assert (782 names
// fit a u16) and the loop bound (26 letters fit a u8).
#[allow(clippy::cast_possible_truncation)]
const fn first_letter_index() -> [u16; 27] {
    assert!(NAME_COUNT < u16::MAX as usize);
    let mut index = [0; 27];
    let mut letter = 0;
    let mut i = 0;
    while letter < 26 {
        while i < NAME_COUNT && NAMES[i].0.as_bytes()[0] < b'a' + letter as u8 {
            i += 1;
        }
        index[letter] = i as u16;
        letter += 1;
    }
    index[26] = NAME_COUNT as u16;
    index
}

/// The first 8 bytes of `name`, big-endian, zero-padded: ordered like the
/// names themselves (no name holds a NUL), so [`KEYS`] is sorted too.
/// [`lookup`] reads the same key from its zero-padded buffer.
const fn prefix_key(name: &[u8]) -> u64 {
    let mut key = 0;
    let mut i = 0;
    while i < 8 {
        key <<= 8;
        if i < name.len() {
            key |= name[i] as u64;
        }
        i += 1;
    }
    key
}

/// [`prefix_key`] of every name in [`NAMES`], computed at compile time:
/// the binary search runs over these (a branch-free compare each step),
/// and only names with the input's key are compared whole.
static KEYS: [u64; NAME_COUNT] = prefix_keys();

/// Computes [`KEYS`].
const fn prefix_keys() -> [u64; NAME_COUNT] {
    let mut keys = [0; NAME_COUNT];
    let mut i = 0;
    while i < NAME_COUNT {
        keys[i] = prefix_key(NAMES[i].0.as_bytes());
        i += 1;
    }
    keys
}

/// The X.org `rgb.txt` names, ASCII-lowercased and sorted by byte, and
/// their colours (see the module documentation for the source).
static NAMES: [(&str, Rgb); NAME_COUNT] = [
    ("alice blue", Rgb::new(240, 248, 255)),
    ("aliceblue", Rgb::new(240, 248, 255)),
    ("antique white", Rgb::new(250, 235, 215)),
    ("antiquewhite", Rgb::new(250, 235, 215)),
    ("antiquewhite1", Rgb::new(255, 239, 219)),
    ("antiquewhite2", Rgb::new(238, 223, 204)),
    ("antiquewhite3", Rgb::new(205, 192, 176)),
    ("antiquewhite4", Rgb::new(139, 131, 120)),
    ("aqua", Rgb::new(0, 255, 255)),
    ("aquamarine", Rgb::new(127, 255, 212)),
    ("aquamarine1", Rgb::new(127, 255, 212)),
    ("aquamarine2", Rgb::new(118, 238, 198)),
    ("aquamarine3", Rgb::new(102, 205, 170)),
    ("aquamarine4", Rgb::new(69, 139, 116)),
    ("azure", Rgb::new(240, 255, 255)),
    ("azure1", Rgb::new(240, 255, 255)),
    ("azure2", Rgb::new(224, 238, 238)),
    ("azure3", Rgb::new(193, 205, 205)),
    ("azure4", Rgb::new(131, 139, 139)),
    ("beige", Rgb::new(245, 245, 220)),
    ("bisque", Rgb::new(255, 228, 196)),
    ("bisque1", Rgb::new(255, 228, 196)),
    ("bisque2", Rgb::new(238, 213, 183)),
    ("bisque3", Rgb::new(205, 183, 158)),
    ("bisque4", Rgb::new(139, 125, 107)),
    ("black", Rgb::new(0, 0, 0)),
    ("blanched almond", Rgb::new(255, 235, 205)),
    ("blanchedalmond", Rgb::new(255, 235, 205)),
    ("blue", Rgb::new(0, 0, 255)),
    ("blue violet", Rgb::new(138, 43, 226)),
    ("blue1", Rgb::new(0, 0, 255)),
    ("blue2", Rgb::new(0, 0, 238)),
    ("blue3", Rgb::new(0, 0, 205)),
    ("blue4", Rgb::new(0, 0, 139)),
    ("blueviolet", Rgb::new(138, 43, 226)),
    ("brown", Rgb::new(165, 42, 42)),
    ("brown1", Rgb::new(255, 64, 64)),
    ("brown2", Rgb::new(238, 59, 59)),
    ("brown3", Rgb::new(205, 51, 51)),
    ("brown4", Rgb::new(139, 35, 35)),
    ("burlywood", Rgb::new(222, 184, 135)),
    ("burlywood1", Rgb::new(255, 211, 155)),
    ("burlywood2", Rgb::new(238, 197, 145)),
    ("burlywood3", Rgb::new(205, 170, 125)),
    ("burlywood4", Rgb::new(139, 115, 85)),
    ("cadet blue", Rgb::new(95, 158, 160)),
    ("cadetblue", Rgb::new(95, 158, 160)),
    ("cadetblue1", Rgb::new(152, 245, 255)),
    ("cadetblue2", Rgb::new(142, 229, 238)),
    ("cadetblue3", Rgb::new(122, 197, 205)),
    ("cadetblue4", Rgb::new(83, 134, 139)),
    ("chartreuse", Rgb::new(127, 255, 0)),
    ("chartreuse1", Rgb::new(127, 255, 0)),
    ("chartreuse2", Rgb::new(118, 238, 0)),
    ("chartreuse3", Rgb::new(102, 205, 0)),
    ("chartreuse4", Rgb::new(69, 139, 0)),
    ("chocolate", Rgb::new(210, 105, 30)),
    ("chocolate1", Rgb::new(255, 127, 36)),
    ("chocolate2", Rgb::new(238, 118, 33)),
    ("chocolate3", Rgb::new(205, 102, 29)),
    ("chocolate4", Rgb::new(139, 69, 19)),
    ("coral", Rgb::new(255, 127, 80)),
    ("coral1", Rgb::new(255, 114, 86)),
    ("coral2", Rgb::new(238, 106, 80)),
    ("coral3", Rgb::new(205, 91, 69)),
    ("coral4", Rgb::new(139, 62, 47)),
    ("cornflower blue", Rgb::new(100, 149, 237)),
    ("cornflowerblue", Rgb::new(100, 149, 237)),
    ("cornsilk", Rgb::new(255, 248, 220)),
    ("cornsilk1", Rgb::new(255, 248, 220)),
    ("cornsilk2", Rgb::new(238, 232, 205)),
    ("cornsilk3", Rgb::new(205, 200, 177)),
    ("cornsilk4", Rgb::new(139, 136, 120)),
    ("crimson", Rgb::new(220, 20, 60)),
    ("cyan", Rgb::new(0, 255, 255)),
    ("cyan1", Rgb::new(0, 255, 255)),
    ("cyan2", Rgb::new(0, 238, 238)),
    ("cyan3", Rgb::new(0, 205, 205)),
    ("cyan4", Rgb::new(0, 139, 139)),
    ("dark blue", Rgb::new(0, 0, 139)),
    ("dark cyan", Rgb::new(0, 139, 139)),
    ("dark goldenrod", Rgb::new(184, 134, 11)),
    ("dark gray", Rgb::new(169, 169, 169)),
    ("dark green", Rgb::new(0, 100, 0)),
    ("dark grey", Rgb::new(169, 169, 169)),
    ("dark khaki", Rgb::new(189, 183, 107)),
    ("dark magenta", Rgb::new(139, 0, 139)),
    ("dark olive green", Rgb::new(85, 107, 47)),
    ("dark orange", Rgb::new(255, 140, 0)),
    ("dark orchid", Rgb::new(153, 50, 204)),
    ("dark red", Rgb::new(139, 0, 0)),
    ("dark salmon", Rgb::new(233, 150, 122)),
    ("dark sea green", Rgb::new(143, 188, 143)),
    ("dark slate blue", Rgb::new(72, 61, 139)),
    ("dark slate gray", Rgb::new(47, 79, 79)),
    ("dark slate grey", Rgb::new(47, 79, 79)),
    ("dark turquoise", Rgb::new(0, 206, 209)),
    ("dark violet", Rgb::new(148, 0, 211)),
    ("darkblue", Rgb::new(0, 0, 139)),
    ("darkcyan", Rgb::new(0, 139, 139)),
    ("darkgoldenrod", Rgb::new(184, 134, 11)),
    ("darkgoldenrod1", Rgb::new(255, 185, 15)),
    ("darkgoldenrod2", Rgb::new(238, 173, 14)),
    ("darkgoldenrod3", Rgb::new(205, 149, 12)),
    ("darkgoldenrod4", Rgb::new(139, 101, 8)),
    ("darkgray", Rgb::new(169, 169, 169)),
    ("darkgreen", Rgb::new(0, 100, 0)),
    ("darkgrey", Rgb::new(169, 169, 169)),
    ("darkkhaki", Rgb::new(189, 183, 107)),
    ("darkmagenta", Rgb::new(139, 0, 139)),
    ("darkolivegreen", Rgb::new(85, 107, 47)),
    ("darkolivegreen1", Rgb::new(202, 255, 112)),
    ("darkolivegreen2", Rgb::new(188, 238, 104)),
    ("darkolivegreen3", Rgb::new(162, 205, 90)),
    ("darkolivegreen4", Rgb::new(110, 139, 61)),
    ("darkorange", Rgb::new(255, 140, 0)),
    ("darkorange1", Rgb::new(255, 127, 0)),
    ("darkorange2", Rgb::new(238, 118, 0)),
    ("darkorange3", Rgb::new(205, 102, 0)),
    ("darkorange4", Rgb::new(139, 69, 0)),
    ("darkorchid", Rgb::new(153, 50, 204)),
    ("darkorchid1", Rgb::new(191, 62, 255)),
    ("darkorchid2", Rgb::new(178, 58, 238)),
    ("darkorchid3", Rgb::new(154, 50, 205)),
    ("darkorchid4", Rgb::new(104, 34, 139)),
    ("darkred", Rgb::new(139, 0, 0)),
    ("darksalmon", Rgb::new(233, 150, 122)),
    ("darkseagreen", Rgb::new(143, 188, 143)),
    ("darkseagreen1", Rgb::new(193, 255, 193)),
    ("darkseagreen2", Rgb::new(180, 238, 180)),
    ("darkseagreen3", Rgb::new(155, 205, 155)),
    ("darkseagreen4", Rgb::new(105, 139, 105)),
    ("darkslateblue", Rgb::new(72, 61, 139)),
    ("darkslategray", Rgb::new(47, 79, 79)),
    ("darkslategray1", Rgb::new(151, 255, 255)),
    ("darkslategray2", Rgb::new(141, 238, 238)),
    ("darkslategray3", Rgb::new(121, 205, 205)),
    ("darkslategray4", Rgb::new(82, 139, 139)),
    ("darkslategrey", Rgb::new(47, 79, 79)),
    ("darkturquoise", Rgb::new(0, 206, 209)),
    ("darkviolet", Rgb::new(148, 0, 211)),
    ("deep pink", Rgb::new(255, 20, 147)),
    ("deep sky blue", Rgb::new(0, 191, 255)),
    ("deeppink", Rgb::new(255, 20, 147)),
    ("deeppink1", Rgb::new(255, 20, 147)),
    ("deeppink2", Rgb::new(238, 18, 137)),
    ("deeppink3", Rgb::new(205, 16, 118)),
    ("deeppink4", Rgb::new(139, 10, 80)),
    ("deepskyblue", Rgb::new(0, 191, 255)),
    ("deepskyblue1", Rgb::new(0, 191, 255)),
    ("deepskyblue2", Rgb::new(0, 178, 238)),
    ("deepskyblue3", Rgb::new(0, 154, 205)),
    ("deepskyblue4", Rgb::new(0, 104, 139)),
    ("dim gray", Rgb::new(105, 105, 105)),
    ("dim grey", Rgb::new(105, 105, 105)),
    ("dimgray", Rgb::new(105, 105, 105)),
    ("dimgrey", Rgb::new(105, 105, 105)),
    ("dodger blue", Rgb::new(30, 144, 255)),
    ("dodgerblue", Rgb::new(30, 144, 255)),
    ("dodgerblue1", Rgb::new(30, 144, 255)),
    ("dodgerblue2", Rgb::new(28, 134, 238)),
    ("dodgerblue3", Rgb::new(24, 116, 205)),
    ("dodgerblue4", Rgb::new(16, 78, 139)),
    ("firebrick", Rgb::new(178, 34, 34)),
    ("firebrick1", Rgb::new(255, 48, 48)),
    ("firebrick2", Rgb::new(238, 44, 44)),
    ("firebrick3", Rgb::new(205, 38, 38)),
    ("firebrick4", Rgb::new(139, 26, 26)),
    ("floral white", Rgb::new(255, 250, 240)),
    ("floralwhite", Rgb::new(255, 250, 240)),
    ("forest green", Rgb::new(34, 139, 34)),
    ("forestgreen", Rgb::new(34, 139, 34)),
    ("fuchsia", Rgb::new(255, 0, 255)),
    ("gainsboro", Rgb::new(220, 220, 220)),
    ("ghost white", Rgb::new(248, 248, 255)),
    ("ghostwhite", Rgb::new(248, 248, 255)),
    ("gold", Rgb::new(255, 215, 0)),
    ("gold1", Rgb::new(255, 215, 0)),
    ("gold2", Rgb::new(238, 201, 0)),
    ("gold3", Rgb::new(205, 173, 0)),
    ("gold4", Rgb::new(139, 117, 0)),
    ("goldenrod", Rgb::new(218, 165, 32)),
    ("goldenrod1", Rgb::new(255, 193, 37)),
    ("goldenrod2", Rgb::new(238, 180, 34)),
    ("goldenrod3", Rgb::new(205, 155, 29)),
    ("goldenrod4", Rgb::new(139, 105, 20)),
    ("gray", Rgb::new(190, 190, 190)),
    ("gray0", Rgb::new(0, 0, 0)),
    ("gray1", Rgb::new(3, 3, 3)),
    ("gray10", Rgb::new(26, 26, 26)),
    ("gray100", Rgb::new(255, 255, 255)),
    ("gray11", Rgb::new(28, 28, 28)),
    ("gray12", Rgb::new(31, 31, 31)),
    ("gray13", Rgb::new(33, 33, 33)),
    ("gray14", Rgb::new(36, 36, 36)),
    ("gray15", Rgb::new(38, 38, 38)),
    ("gray16", Rgb::new(41, 41, 41)),
    ("gray17", Rgb::new(43, 43, 43)),
    ("gray18", Rgb::new(46, 46, 46)),
    ("gray19", Rgb::new(48, 48, 48)),
    ("gray2", Rgb::new(5, 5, 5)),
    ("gray20", Rgb::new(51, 51, 51)),
    ("gray21", Rgb::new(54, 54, 54)),
    ("gray22", Rgb::new(56, 56, 56)),
    ("gray23", Rgb::new(59, 59, 59)),
    ("gray24", Rgb::new(61, 61, 61)),
    ("gray25", Rgb::new(64, 64, 64)),
    ("gray26", Rgb::new(66, 66, 66)),
    ("gray27", Rgb::new(69, 69, 69)),
    ("gray28", Rgb::new(71, 71, 71)),
    ("gray29", Rgb::new(74, 74, 74)),
    ("gray3", Rgb::new(8, 8, 8)),
    ("gray30", Rgb::new(77, 77, 77)),
    ("gray31", Rgb::new(79, 79, 79)),
    ("gray32", Rgb::new(82, 82, 82)),
    ("gray33", Rgb::new(84, 84, 84)),
    ("gray34", Rgb::new(87, 87, 87)),
    ("gray35", Rgb::new(89, 89, 89)),
    ("gray36", Rgb::new(92, 92, 92)),
    ("gray37", Rgb::new(94, 94, 94)),
    ("gray38", Rgb::new(97, 97, 97)),
    ("gray39", Rgb::new(99, 99, 99)),
    ("gray4", Rgb::new(10, 10, 10)),
    ("gray40", Rgb::new(102, 102, 102)),
    ("gray41", Rgb::new(105, 105, 105)),
    ("gray42", Rgb::new(107, 107, 107)),
    ("gray43", Rgb::new(110, 110, 110)),
    ("gray44", Rgb::new(112, 112, 112)),
    ("gray45", Rgb::new(115, 115, 115)),
    ("gray46", Rgb::new(117, 117, 117)),
    ("gray47", Rgb::new(120, 120, 120)),
    ("gray48", Rgb::new(122, 122, 122)),
    ("gray49", Rgb::new(125, 125, 125)),
    ("gray5", Rgb::new(13, 13, 13)),
    ("gray50", Rgb::new(127, 127, 127)),
    ("gray51", Rgb::new(130, 130, 130)),
    ("gray52", Rgb::new(133, 133, 133)),
    ("gray53", Rgb::new(135, 135, 135)),
    ("gray54", Rgb::new(138, 138, 138)),
    ("gray55", Rgb::new(140, 140, 140)),
    ("gray56", Rgb::new(143, 143, 143)),
    ("gray57", Rgb::new(145, 145, 145)),
    ("gray58", Rgb::new(148, 148, 148)),
    ("gray59", Rgb::new(150, 150, 150)),
    ("gray6", Rgb::new(15, 15, 15)),
    ("gray60", Rgb::new(153, 153, 153)),
    ("gray61", Rgb::new(156, 156, 156)),
    ("gray62", Rgb::new(158, 158, 158)),
    ("gray63", Rgb::new(161, 161, 161)),
    ("gray64", Rgb::new(163, 163, 163)),
    ("gray65", Rgb::new(166, 166, 166)),
    ("gray66", Rgb::new(168, 168, 168)),
    ("gray67", Rgb::new(171, 171, 171)),
    ("gray68", Rgb::new(173, 173, 173)),
    ("gray69", Rgb::new(176, 176, 176)),
    ("gray7", Rgb::new(18, 18, 18)),
    ("gray70", Rgb::new(179, 179, 179)),
    ("gray71", Rgb::new(181, 181, 181)),
    ("gray72", Rgb::new(184, 184, 184)),
    ("gray73", Rgb::new(186, 186, 186)),
    ("gray74", Rgb::new(189, 189, 189)),
    ("gray75", Rgb::new(191, 191, 191)),
    ("gray76", Rgb::new(194, 194, 194)),
    ("gray77", Rgb::new(196, 196, 196)),
    ("gray78", Rgb::new(199, 199, 199)),
    ("gray79", Rgb::new(201, 201, 201)),
    ("gray8", Rgb::new(20, 20, 20)),
    ("gray80", Rgb::new(204, 204, 204)),
    ("gray81", Rgb::new(207, 207, 207)),
    ("gray82", Rgb::new(209, 209, 209)),
    ("gray83", Rgb::new(212, 212, 212)),
    ("gray84", Rgb::new(214, 214, 214)),
    ("gray85", Rgb::new(217, 217, 217)),
    ("gray86", Rgb::new(219, 219, 219)),
    ("gray87", Rgb::new(222, 222, 222)),
    ("gray88", Rgb::new(224, 224, 224)),
    ("gray89", Rgb::new(227, 227, 227)),
    ("gray9", Rgb::new(23, 23, 23)),
    ("gray90", Rgb::new(229, 229, 229)),
    ("gray91", Rgb::new(232, 232, 232)),
    ("gray92", Rgb::new(235, 235, 235)),
    ("gray93", Rgb::new(237, 237, 237)),
    ("gray94", Rgb::new(240, 240, 240)),
    ("gray95", Rgb::new(242, 242, 242)),
    ("gray96", Rgb::new(245, 245, 245)),
    ("gray97", Rgb::new(247, 247, 247)),
    ("gray98", Rgb::new(250, 250, 250)),
    ("gray99", Rgb::new(252, 252, 252)),
    ("green", Rgb::new(0, 255, 0)),
    ("green yellow", Rgb::new(173, 255, 47)),
    ("green1", Rgb::new(0, 255, 0)),
    ("green2", Rgb::new(0, 238, 0)),
    ("green3", Rgb::new(0, 205, 0)),
    ("green4", Rgb::new(0, 139, 0)),
    ("greenyellow", Rgb::new(173, 255, 47)),
    ("grey", Rgb::new(190, 190, 190)),
    ("grey0", Rgb::new(0, 0, 0)),
    ("grey1", Rgb::new(3, 3, 3)),
    ("grey10", Rgb::new(26, 26, 26)),
    ("grey100", Rgb::new(255, 255, 255)),
    ("grey11", Rgb::new(28, 28, 28)),
    ("grey12", Rgb::new(31, 31, 31)),
    ("grey13", Rgb::new(33, 33, 33)),
    ("grey14", Rgb::new(36, 36, 36)),
    ("grey15", Rgb::new(38, 38, 38)),
    ("grey16", Rgb::new(41, 41, 41)),
    ("grey17", Rgb::new(43, 43, 43)),
    ("grey18", Rgb::new(46, 46, 46)),
    ("grey19", Rgb::new(48, 48, 48)),
    ("grey2", Rgb::new(5, 5, 5)),
    ("grey20", Rgb::new(51, 51, 51)),
    ("grey21", Rgb::new(54, 54, 54)),
    ("grey22", Rgb::new(56, 56, 56)),
    ("grey23", Rgb::new(59, 59, 59)),
    ("grey24", Rgb::new(61, 61, 61)),
    ("grey25", Rgb::new(64, 64, 64)),
    ("grey26", Rgb::new(66, 66, 66)),
    ("grey27", Rgb::new(69, 69, 69)),
    ("grey28", Rgb::new(71, 71, 71)),
    ("grey29", Rgb::new(74, 74, 74)),
    ("grey3", Rgb::new(8, 8, 8)),
    ("grey30", Rgb::new(77, 77, 77)),
    ("grey31", Rgb::new(79, 79, 79)),
    ("grey32", Rgb::new(82, 82, 82)),
    ("grey33", Rgb::new(84, 84, 84)),
    ("grey34", Rgb::new(87, 87, 87)),
    ("grey35", Rgb::new(89, 89, 89)),
    ("grey36", Rgb::new(92, 92, 92)),
    ("grey37", Rgb::new(94, 94, 94)),
    ("grey38", Rgb::new(97, 97, 97)),
    ("grey39", Rgb::new(99, 99, 99)),
    ("grey4", Rgb::new(10, 10, 10)),
    ("grey40", Rgb::new(102, 102, 102)),
    ("grey41", Rgb::new(105, 105, 105)),
    ("grey42", Rgb::new(107, 107, 107)),
    ("grey43", Rgb::new(110, 110, 110)),
    ("grey44", Rgb::new(112, 112, 112)),
    ("grey45", Rgb::new(115, 115, 115)),
    ("grey46", Rgb::new(117, 117, 117)),
    ("grey47", Rgb::new(120, 120, 120)),
    ("grey48", Rgb::new(122, 122, 122)),
    ("grey49", Rgb::new(125, 125, 125)),
    ("grey5", Rgb::new(13, 13, 13)),
    ("grey50", Rgb::new(127, 127, 127)),
    ("grey51", Rgb::new(130, 130, 130)),
    ("grey52", Rgb::new(133, 133, 133)),
    ("grey53", Rgb::new(135, 135, 135)),
    ("grey54", Rgb::new(138, 138, 138)),
    ("grey55", Rgb::new(140, 140, 140)),
    ("grey56", Rgb::new(143, 143, 143)),
    ("grey57", Rgb::new(145, 145, 145)),
    ("grey58", Rgb::new(148, 148, 148)),
    ("grey59", Rgb::new(150, 150, 150)),
    ("grey6", Rgb::new(15, 15, 15)),
    ("grey60", Rgb::new(153, 153, 153)),
    ("grey61", Rgb::new(156, 156, 156)),
    ("grey62", Rgb::new(158, 158, 158)),
    ("grey63", Rgb::new(161, 161, 161)),
    ("grey64", Rgb::new(163, 163, 163)),
    ("grey65", Rgb::new(166, 166, 166)),
    ("grey66", Rgb::new(168, 168, 168)),
    ("grey67", Rgb::new(171, 171, 171)),
    ("grey68", Rgb::new(173, 173, 173)),
    ("grey69", Rgb::new(176, 176, 176)),
    ("grey7", Rgb::new(18, 18, 18)),
    ("grey70", Rgb::new(179, 179, 179)),
    ("grey71", Rgb::new(181, 181, 181)),
    ("grey72", Rgb::new(184, 184, 184)),
    ("grey73", Rgb::new(186, 186, 186)),
    ("grey74", Rgb::new(189, 189, 189)),
    ("grey75", Rgb::new(191, 191, 191)),
    ("grey76", Rgb::new(194, 194, 194)),
    ("grey77", Rgb::new(196, 196, 196)),
    ("grey78", Rgb::new(199, 199, 199)),
    ("grey79", Rgb::new(201, 201, 201)),
    ("grey8", Rgb::new(20, 20, 20)),
    ("grey80", Rgb::new(204, 204, 204)),
    ("grey81", Rgb::new(207, 207, 207)),
    ("grey82", Rgb::new(209, 209, 209)),
    ("grey83", Rgb::new(212, 212, 212)),
    ("grey84", Rgb::new(214, 214, 214)),
    ("grey85", Rgb::new(217, 217, 217)),
    ("grey86", Rgb::new(219, 219, 219)),
    ("grey87", Rgb::new(222, 222, 222)),
    ("grey88", Rgb::new(224, 224, 224)),
    ("grey89", Rgb::new(227, 227, 227)),
    ("grey9", Rgb::new(23, 23, 23)),
    ("grey90", Rgb::new(229, 229, 229)),
    ("grey91", Rgb::new(232, 232, 232)),
    ("grey92", Rgb::new(235, 235, 235)),
    ("grey93", Rgb::new(237, 237, 237)),
    ("grey94", Rgb::new(240, 240, 240)),
    ("grey95", Rgb::new(242, 242, 242)),
    ("grey96", Rgb::new(245, 245, 245)),
    ("grey97", Rgb::new(247, 247, 247)),
    ("grey98", Rgb::new(250, 250, 250)),
    ("grey99", Rgb::new(252, 252, 252)),
    ("honeydew", Rgb::new(240, 255, 240)),
    ("honeydew1", Rgb::new(240, 255, 240)),
    ("honeydew2", Rgb::new(224, 238, 224)),
    ("honeydew3", Rgb::new(193, 205, 193)),
    ("honeydew4", Rgb::new(131, 139, 131)),
    ("hot pink", Rgb::new(255, 105, 180)),
    ("hotpink", Rgb::new(255, 105, 180)),
    ("hotpink1", Rgb::new(255, 110, 180)),
    ("hotpink2", Rgb::new(238, 106, 167)),
    ("hotpink3", Rgb::new(205, 96, 144)),
    ("hotpink4", Rgb::new(139, 58, 98)),
    ("indian red", Rgb::new(205, 92, 92)),
    ("indianred", Rgb::new(205, 92, 92)),
    ("indianred1", Rgb::new(255, 106, 106)),
    ("indianred2", Rgb::new(238, 99, 99)),
    ("indianred3", Rgb::new(205, 85, 85)),
    ("indianred4", Rgb::new(139, 58, 58)),
    ("indigo", Rgb::new(75, 0, 130)),
    ("ivory", Rgb::new(255, 255, 240)),
    ("ivory1", Rgb::new(255, 255, 240)),
    ("ivory2", Rgb::new(238, 238, 224)),
    ("ivory3", Rgb::new(205, 205, 193)),
    ("ivory4", Rgb::new(139, 139, 131)),
    ("khaki", Rgb::new(240, 230, 140)),
    ("khaki1", Rgb::new(255, 246, 143)),
    ("khaki2", Rgb::new(238, 230, 133)),
    ("khaki3", Rgb::new(205, 198, 115)),
    ("khaki4", Rgb::new(139, 134, 78)),
    ("lavender", Rgb::new(230, 230, 250)),
    ("lavender blush", Rgb::new(255, 240, 245)),
    ("lavenderblush", Rgb::new(255, 240, 245)),
    ("lavenderblush1", Rgb::new(255, 240, 245)),
    ("lavenderblush2", Rgb::new(238, 224, 229)),
    ("lavenderblush3", Rgb::new(205, 193, 197)),
    ("lavenderblush4", Rgb::new(139, 131, 134)),
    ("lawn green", Rgb::new(124, 252, 0)),
    ("lawngreen", Rgb::new(124, 252, 0)),
    ("lemon chiffon", Rgb::new(255, 250, 205)),
    ("lemonchiffon", Rgb::new(255, 250, 205)),
    ("lemonchiffon1", Rgb::new(255, 250, 205)),
    ("lemonchiffon2", Rgb::new(238, 233, 191)),
    ("lemonchiffon3", Rgb::new(205, 201, 165)),
    ("lemonchiffon4", Rgb::new(139, 137, 112)),
    ("light blue", Rgb::new(173, 216, 230)),
    ("light coral", Rgb::new(240, 128, 128)),
    ("light cyan", Rgb::new(224, 255, 255)),
    ("light goldenrod", Rgb::new(238, 221, 130)),
    ("light goldenrod yellow", Rgb::new(250, 250, 210)),
    ("light gray", Rgb::new(211, 211, 211)),
    ("light green", Rgb::new(144, 238, 144)),
    ("light grey", Rgb::new(211, 211, 211)),
    ("light pink", Rgb::new(255, 182, 193)),
    ("light salmon", Rgb::new(255, 160, 122)),
    ("light sea green", Rgb::new(32, 178, 170)),
    ("light sky blue", Rgb::new(135, 206, 250)),
    ("light slate blue", Rgb::new(132, 112, 255)),
    ("light slate gray", Rgb::new(119, 136, 153)),
    ("light slate grey", Rgb::new(119, 136, 153)),
    ("light steel blue", Rgb::new(176, 196, 222)),
    ("light yellow", Rgb::new(255, 255, 224)),
    ("lightblue", Rgb::new(173, 216, 230)),
    ("lightblue1", Rgb::new(191, 239, 255)),
    ("lightblue2", Rgb::new(178, 223, 238)),
    ("lightblue3", Rgb::new(154, 192, 205)),
    ("lightblue4", Rgb::new(104, 131, 139)),
    ("lightcoral", Rgb::new(240, 128, 128)),
    ("lightcyan", Rgb::new(224, 255, 255)),
    ("lightcyan1", Rgb::new(224, 255, 255)),
    ("lightcyan2", Rgb::new(209, 238, 238)),
    ("lightcyan3", Rgb::new(180, 205, 205)),
    ("lightcyan4", Rgb::new(122, 139, 139)),
    ("lightgoldenrod", Rgb::new(238, 221, 130)),
    ("lightgoldenrod1", Rgb::new(255, 236, 139)),
    ("lightgoldenrod2", Rgb::new(238, 220, 130)),
    ("lightgoldenrod3", Rgb::new(205, 190, 112)),
    ("lightgoldenrod4", Rgb::new(139, 129, 76)),
    ("lightgoldenrodyellow", Rgb::new(250, 250, 210)),
    ("lightgray", Rgb::new(211, 211, 211)),
    ("lightgreen", Rgb::new(144, 238, 144)),
    ("lightgrey", Rgb::new(211, 211, 211)),
    ("lightpink", Rgb::new(255, 182, 193)),
    ("lightpink1", Rgb::new(255, 174, 185)),
    ("lightpink2", Rgb::new(238, 162, 173)),
    ("lightpink3", Rgb::new(205, 140, 149)),
    ("lightpink4", Rgb::new(139, 95, 101)),
    ("lightsalmon", Rgb::new(255, 160, 122)),
    ("lightsalmon1", Rgb::new(255, 160, 122)),
    ("lightsalmon2", Rgb::new(238, 149, 114)),
    ("lightsalmon3", Rgb::new(205, 129, 98)),
    ("lightsalmon4", Rgb::new(139, 87, 66)),
    ("lightseagreen", Rgb::new(32, 178, 170)),
    ("lightskyblue", Rgb::new(135, 206, 250)),
    ("lightskyblue1", Rgb::new(176, 226, 255)),
    ("lightskyblue2", Rgb::new(164, 211, 238)),
    ("lightskyblue3", Rgb::new(141, 182, 205)),
    ("lightskyblue4", Rgb::new(96, 123, 139)),
    ("lightslateblue", Rgb::new(132, 112, 255)),
    ("lightslategray", Rgb::new(119, 136, 153)),
    ("lightslategrey", Rgb::new(119, 136, 153)),
    ("lightsteelblue", Rgb::new(176, 196, 222)),
    ("lightsteelblue1", Rgb::new(202, 225, 255)),
    ("lightsteelblue2", Rgb::new(188, 210, 238)),
    ("lightsteelblue3", Rgb::new(162, 181, 205)),
    ("lightsteelblue4", Rgb::new(110, 123, 139)),
    ("lightyellow", Rgb::new(255, 255, 224)),
    ("lightyellow1", Rgb::new(255, 255, 224)),
    ("lightyellow2", Rgb::new(238, 238, 209)),
    ("lightyellow3", Rgb::new(205, 205, 180)),
    ("lightyellow4", Rgb::new(139, 139, 122)),
    ("lime", Rgb::new(0, 255, 0)),
    ("lime green", Rgb::new(50, 205, 50)),
    ("limegreen", Rgb::new(50, 205, 50)),
    ("linen", Rgb::new(250, 240, 230)),
    ("magenta", Rgb::new(255, 0, 255)),
    ("magenta1", Rgb::new(255, 0, 255)),
    ("magenta2", Rgb::new(238, 0, 238)),
    ("magenta3", Rgb::new(205, 0, 205)),
    ("magenta4", Rgb::new(139, 0, 139)),
    ("maroon", Rgb::new(176, 48, 96)),
    ("maroon1", Rgb::new(255, 52, 179)),
    ("maroon2", Rgb::new(238, 48, 167)),
    ("maroon3", Rgb::new(205, 41, 144)),
    ("maroon4", Rgb::new(139, 28, 98)),
    ("medium aquamarine", Rgb::new(102, 205, 170)),
    ("medium blue", Rgb::new(0, 0, 205)),
    ("medium orchid", Rgb::new(186, 85, 211)),
    ("medium purple", Rgb::new(147, 112, 219)),
    ("medium sea green", Rgb::new(60, 179, 113)),
    ("medium slate blue", Rgb::new(123, 104, 238)),
    ("medium spring green", Rgb::new(0, 250, 154)),
    ("medium turquoise", Rgb::new(72, 209, 204)),
    ("medium violet red", Rgb::new(199, 21, 133)),
    ("mediumaquamarine", Rgb::new(102, 205, 170)),
    ("mediumblue", Rgb::new(0, 0, 205)),
    ("mediumorchid", Rgb::new(186, 85, 211)),
    ("mediumorchid1", Rgb::new(224, 102, 255)),
    ("mediumorchid2", Rgb::new(209, 95, 238)),
    ("mediumorchid3", Rgb::new(180, 82, 205)),
    ("mediumorchid4", Rgb::new(122, 55, 139)),
    ("mediumpurple", Rgb::new(147, 112, 219)),
    ("mediumpurple1", Rgb::new(171, 130, 255)),
    ("mediumpurple2", Rgb::new(159, 121, 238)),
    ("mediumpurple3", Rgb::new(137, 104, 205)),
    ("mediumpurple4", Rgb::new(93, 71, 139)),
    ("mediumseagreen", Rgb::new(60, 179, 113)),
    ("mediumslateblue", Rgb::new(123, 104, 238)),
    ("mediumspringgreen", Rgb::new(0, 250, 154)),
    ("mediumturquoise", Rgb::new(72, 209, 204)),
    ("mediumvioletred", Rgb::new(199, 21, 133)),
    ("midnight blue", Rgb::new(25, 25, 112)),
    ("midnightblue", Rgb::new(25, 25, 112)),
    ("mint cream", Rgb::new(245, 255, 250)),
    ("mintcream", Rgb::new(245, 255, 250)),
    ("misty rose", Rgb::new(255, 228, 225)),
    ("mistyrose", Rgb::new(255, 228, 225)),
    ("mistyrose1", Rgb::new(255, 228, 225)),
    ("mistyrose2", Rgb::new(238, 213, 210)),
    ("mistyrose3", Rgb::new(205, 183, 181)),
    ("mistyrose4", Rgb::new(139, 125, 123)),
    ("moccasin", Rgb::new(255, 228, 181)),
    ("navajo white", Rgb::new(255, 222, 173)),
    ("navajowhite", Rgb::new(255, 222, 173)),
    ("navajowhite1", Rgb::new(255, 222, 173)),
    ("navajowhite2", Rgb::new(238, 207, 161)),
    ("navajowhite3", Rgb::new(205, 179, 139)),
    ("navajowhite4", Rgb::new(139, 121, 94)),
    ("navy", Rgb::new(0, 0, 128)),
    ("navy blue", Rgb::new(0, 0, 128)),
    ("navyblue", Rgb::new(0, 0, 128)),
    ("old lace", Rgb::new(253, 245, 230)),
    ("oldlace", Rgb::new(253, 245, 230)),
    ("olive", Rgb::new(128, 128, 0)),
    ("olive drab", Rgb::new(107, 142, 35)),
    ("olivedrab", Rgb::new(107, 142, 35)),
    ("olivedrab1", Rgb::new(192, 255, 62)),
    ("olivedrab2", Rgb::new(179, 238, 58)),
    ("olivedrab3", Rgb::new(154, 205, 50)),
    ("olivedrab4", Rgb::new(105, 139, 34)),
    ("orange", Rgb::new(255, 165, 0)),
    ("orange red", Rgb::new(255, 69, 0)),
    ("orange1", Rgb::new(255, 165, 0)),
    ("orange2", Rgb::new(238, 154, 0)),
    ("orange3", Rgb::new(205, 133, 0)),
    ("orange4", Rgb::new(139, 90, 0)),
    ("orangered", Rgb::new(255, 69, 0)),
    ("orangered1", Rgb::new(255, 69, 0)),
    ("orangered2", Rgb::new(238, 64, 0)),
    ("orangered3", Rgb::new(205, 55, 0)),
    ("orangered4", Rgb::new(139, 37, 0)),
    ("orchid", Rgb::new(218, 112, 214)),
    ("orchid1", Rgb::new(255, 131, 250)),
    ("orchid2", Rgb::new(238, 122, 233)),
    ("orchid3", Rgb::new(205, 105, 201)),
    ("orchid4", Rgb::new(139, 71, 137)),
    ("pale goldenrod", Rgb::new(238, 232, 170)),
    ("pale green", Rgb::new(152, 251, 152)),
    ("pale turquoise", Rgb::new(175, 238, 238)),
    ("pale violet red", Rgb::new(219, 112, 147)),
    ("palegoldenrod", Rgb::new(238, 232, 170)),
    ("palegreen", Rgb::new(152, 251, 152)),
    ("palegreen1", Rgb::new(154, 255, 154)),
    ("palegreen2", Rgb::new(144, 238, 144)),
    ("palegreen3", Rgb::new(124, 205, 124)),
    ("palegreen4", Rgb::new(84, 139, 84)),
    ("paleturquoise", Rgb::new(175, 238, 238)),
    ("paleturquoise1", Rgb::new(187, 255, 255)),
    ("paleturquoise2", Rgb::new(174, 238, 238)),
    ("paleturquoise3", Rgb::new(150, 205, 205)),
    ("paleturquoise4", Rgb::new(102, 139, 139)),
    ("palevioletred", Rgb::new(219, 112, 147)),
    ("palevioletred1", Rgb::new(255, 130, 171)),
    ("palevioletred2", Rgb::new(238, 121, 159)),
    ("palevioletred3", Rgb::new(205, 104, 137)),
    ("palevioletred4", Rgb::new(139, 71, 93)),
    ("papaya whip", Rgb::new(255, 239, 213)),
    ("papayawhip", Rgb::new(255, 239, 213)),
    ("peach puff", Rgb::new(255, 218, 185)),
    ("peachpuff", Rgb::new(255, 218, 185)),
    ("peachpuff1", Rgb::new(255, 218, 185)),
    ("peachpuff2", Rgb::new(238, 203, 173)),
    ("peachpuff3", Rgb::new(205, 175, 149)),
    ("peachpuff4", Rgb::new(139, 119, 101)),
    ("peru", Rgb::new(205, 133, 63)),
    ("pink", Rgb::new(255, 192, 203)),
    ("pink1", Rgb::new(255, 181, 197)),
    ("pink2", Rgb::new(238, 169, 184)),
    ("pink3", Rgb::new(205, 145, 158)),
    ("pink4", Rgb::new(139, 99, 108)),
    ("plum", Rgb::new(221, 160, 221)),
    ("plum1", Rgb::new(255, 187, 255)),
    ("plum2", Rgb::new(238, 174, 238)),
    ("plum3", Rgb::new(205, 150, 205)),
    ("plum4", Rgb::new(139, 102, 139)),
    ("powder blue", Rgb::new(176, 224, 230)),
    ("powderblue", Rgb::new(176, 224, 230)),
    ("purple", Rgb::new(160, 32, 240)),
    ("purple1", Rgb::new(155, 48, 255)),
    ("purple2", Rgb::new(145, 44, 238)),
    ("purple3", Rgb::new(125, 38, 205)),
    ("purple4", Rgb::new(85, 26, 139)),
    ("rebecca purple", Rgb::new(102, 51, 153)),
    ("rebeccapurple", Rgb::new(102, 51, 153)),
    ("red", Rgb::new(255, 0, 0)),
    ("red1", Rgb::new(255, 0, 0)),
    ("red2", Rgb::new(238, 0, 0)),
    ("red3", Rgb::new(205, 0, 0)),
    ("red4", Rgb::new(139, 0, 0)),
    ("rosy brown", Rgb::new(188, 143, 143)),
    ("rosybrown", Rgb::new(188, 143, 143)),
    ("rosybrown1", Rgb::new(255, 193, 193)),
    ("rosybrown2", Rgb::new(238, 180, 180)),
    ("rosybrown3", Rgb::new(205, 155, 155)),
    ("rosybrown4", Rgb::new(139, 105, 105)),
    ("royal blue", Rgb::new(65, 105, 225)),
    ("royalblue", Rgb::new(65, 105, 225)),
    ("royalblue1", Rgb::new(72, 118, 255)),
    ("royalblue2", Rgb::new(67, 110, 238)),
    ("royalblue3", Rgb::new(58, 95, 205)),
    ("royalblue4", Rgb::new(39, 64, 139)),
    ("saddle brown", Rgb::new(139, 69, 19)),
    ("saddlebrown", Rgb::new(139, 69, 19)),
    ("salmon", Rgb::new(250, 128, 114)),
    ("salmon1", Rgb::new(255, 140, 105)),
    ("salmon2", Rgb::new(238, 130, 98)),
    ("salmon3", Rgb::new(205, 112, 84)),
    ("salmon4", Rgb::new(139, 76, 57)),
    ("sandy brown", Rgb::new(244, 164, 96)),
    ("sandybrown", Rgb::new(244, 164, 96)),
    ("sea green", Rgb::new(46, 139, 87)),
    ("seagreen", Rgb::new(46, 139, 87)),
    ("seagreen1", Rgb::new(84, 255, 159)),
    ("seagreen2", Rgb::new(78, 238, 148)),
    ("seagreen3", Rgb::new(67, 205, 128)),
    ("seagreen4", Rgb::new(46, 139, 87)),
    ("seashell", Rgb::new(255, 245, 238)),
    ("seashell1", Rgb::new(255, 245, 238)),
    ("seashell2", Rgb::new(238, 229, 222)),
    ("seashell3", Rgb::new(205, 197, 191)),
    ("seashell4", Rgb::new(139, 134, 130)),
    ("sienna", Rgb::new(160, 82, 45)),
    ("sienna1", Rgb::new(255, 130, 71)),
    ("sienna2", Rgb::new(238, 121, 66)),
    ("sienna3", Rgb::new(205, 104, 57)),
    ("sienna4", Rgb::new(139, 71, 38)),
    ("silver", Rgb::new(192, 192, 192)),
    ("sky blue", Rgb::new(135, 206, 235)),
    ("skyblue", Rgb::new(135, 206, 235)),
    ("skyblue1", Rgb::new(135, 206, 255)),
    ("skyblue2", Rgb::new(126, 192, 238)),
    ("skyblue3", Rgb::new(108, 166, 205)),
    ("skyblue4", Rgb::new(74, 112, 139)),
    ("slate blue", Rgb::new(106, 90, 205)),
    ("slate gray", Rgb::new(112, 128, 144)),
    ("slate grey", Rgb::new(112, 128, 144)),
    ("slateblue", Rgb::new(106, 90, 205)),
    ("slateblue1", Rgb::new(131, 111, 255)),
    ("slateblue2", Rgb::new(122, 103, 238)),
    ("slateblue3", Rgb::new(105, 89, 205)),
    ("slateblue4", Rgb::new(71, 60, 139)),
    ("slategray", Rgb::new(112, 128, 144)),
    ("slategray1", Rgb::new(198, 226, 255)),
    ("slategray2", Rgb::new(185, 211, 238)),
    ("slategray3", Rgb::new(159, 182, 205)),
    ("slategray4", Rgb::new(108, 123, 139)),
    ("slategrey", Rgb::new(112, 128, 144)),
    ("snow", Rgb::new(255, 250, 250)),
    ("snow1", Rgb::new(255, 250, 250)),
    ("snow2", Rgb::new(238, 233, 233)),
    ("snow3", Rgb::new(205, 201, 201)),
    ("snow4", Rgb::new(139, 137, 137)),
    ("spring green", Rgb::new(0, 255, 127)),
    ("springgreen", Rgb::new(0, 255, 127)),
    ("springgreen1", Rgb::new(0, 255, 127)),
    ("springgreen2", Rgb::new(0, 238, 118)),
    ("springgreen3", Rgb::new(0, 205, 102)),
    ("springgreen4", Rgb::new(0, 139, 69)),
    ("steel blue", Rgb::new(70, 130, 180)),
    ("steelblue", Rgb::new(70, 130, 180)),
    ("steelblue1", Rgb::new(99, 184, 255)),
    ("steelblue2", Rgb::new(92, 172, 238)),
    ("steelblue3", Rgb::new(79, 148, 205)),
    ("steelblue4", Rgb::new(54, 100, 139)),
    ("tan", Rgb::new(210, 180, 140)),
    ("tan1", Rgb::new(255, 165, 79)),
    ("tan2", Rgb::new(238, 154, 73)),
    ("tan3", Rgb::new(205, 133, 63)),
    ("tan4", Rgb::new(139, 90, 43)),
    ("teal", Rgb::new(0, 128, 128)),
    ("thistle", Rgb::new(216, 191, 216)),
    ("thistle1", Rgb::new(255, 225, 255)),
    ("thistle2", Rgb::new(238, 210, 238)),
    ("thistle3", Rgb::new(205, 181, 205)),
    ("thistle4", Rgb::new(139, 123, 139)),
    ("tomato", Rgb::new(255, 99, 71)),
    ("tomato1", Rgb::new(255, 99, 71)),
    ("tomato2", Rgb::new(238, 92, 66)),
    ("tomato3", Rgb::new(205, 79, 57)),
    ("tomato4", Rgb::new(139, 54, 38)),
    ("turquoise", Rgb::new(64, 224, 208)),
    ("turquoise1", Rgb::new(0, 245, 255)),
    ("turquoise2", Rgb::new(0, 229, 238)),
    ("turquoise3", Rgb::new(0, 197, 205)),
    ("turquoise4", Rgb::new(0, 134, 139)),
    ("violet", Rgb::new(238, 130, 238)),
    ("violet red", Rgb::new(208, 32, 144)),
    ("violetred", Rgb::new(208, 32, 144)),
    ("violetred1", Rgb::new(255, 62, 150)),
    ("violetred2", Rgb::new(238, 58, 140)),
    ("violetred3", Rgb::new(205, 50, 120)),
    ("violetred4", Rgb::new(139, 34, 82)),
    ("web gray", Rgb::new(128, 128, 128)),
    ("web green", Rgb::new(0, 128, 0)),
    ("web grey", Rgb::new(128, 128, 128)),
    ("web maroon", Rgb::new(128, 0, 0)),
    ("web purple", Rgb::new(128, 0, 128)),
    ("webgray", Rgb::new(128, 128, 128)),
    ("webgreen", Rgb::new(0, 128, 0)),
    ("webgrey", Rgb::new(128, 128, 128)),
    ("webmaroon", Rgb::new(128, 0, 0)),
    ("webpurple", Rgb::new(128, 0, 128)),
    ("wheat", Rgb::new(245, 222, 179)),
    ("wheat1", Rgb::new(255, 231, 186)),
    ("wheat2", Rgb::new(238, 216, 174)),
    ("wheat3", Rgb::new(205, 186, 150)),
    ("wheat4", Rgb::new(139, 126, 102)),
    ("white", Rgb::new(255, 255, 255)),
    ("white smoke", Rgb::new(245, 245, 245)),
    ("whitesmoke", Rgb::new(245, 245, 245)),
    ("x11 gray", Rgb::new(190, 190, 190)),
    ("x11 green", Rgb::new(0, 255, 0)),
    ("x11 grey", Rgb::new(190, 190, 190)),
    ("x11 maroon", Rgb::new(176, 48, 96)),
    ("x11 purple", Rgb::new(160, 32, 240)),
    ("x11gray", Rgb::new(190, 190, 190)),
    ("x11green", Rgb::new(0, 255, 0)),
    ("x11grey", Rgb::new(190, 190, 190)),
    ("x11maroon", Rgb::new(176, 48, 96)),
    ("x11purple", Rgb::new(160, 32, 240)),
    ("yellow", Rgb::new(255, 255, 0)),
    ("yellow green", Rgb::new(154, 205, 50)),
    ("yellow1", Rgb::new(255, 255, 0)),
    ("yellow2", Rgb::new(238, 238, 0)),
    ("yellow3", Rgb::new(205, 205, 0)),
    ("yellow4", Rgb::new(139, 139, 0)),
    ("yellowgreen", Rgb::new(154, 205, 50)),
];

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// The binary search needs the table lowercase and sorted, without
    /// duplicates; the cap must be the longest entry exactly; the prefix
    /// keys must be sorted like the names, and the letter index must cover
    /// every name under its first letter.
    #[test]
    fn the_table_is_sorted_unique_and_capped() {
        assert_eq!(NAMES.len(), 782);
        for pair in NAMES.windows(2) {
            let [(a, _), (b, _)] = pair else {
                unreachable!("windows of two")
            };
            assert!(a < b, "{a:?} before {b:?}");
        }
        let longest = NAMES.iter().map(|(name, _)| name.len()).max();
        assert_eq!(longest, Some(MAX_NAME_LEN));
        for (name, _) in &NAMES {
            assert!(
                name.bytes()
                    .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b' '))
            );
            assert!(name.as_bytes()[0].is_ascii_lowercase(), "{name:?}");
        }
        assert!(KEYS.is_sorted(), "the keys follow the names' order");
        for (i, (name, _)) in NAMES.iter().enumerate() {
            let letter = usize::from(name.as_bytes()[0] - b'a');
            let range = usize::from(FIRST[letter])..usize::from(FIRST[letter + 1]);
            assert!(range.contains(&i), "{name:?} at {i} outside {range:?}");
        }
    }

    /// The table is `rgb.txt` exactly: an FNV-1a hash of every
    /// `name TAB r g b NL` (lowercased name, the channels as bytes) in table
    /// order, computed from the researcher's copy of `rgb.txt` (blob
    /// `7096643`, see the module documentation), so one wrong name or
    /// colour anywhere fails.
    #[test]
    fn the_table_matches_rgb_txt() {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for (name, rgb) in &NAMES {
            for &byte in name
                .as_bytes()
                .iter()
                .chain(&[b'\t', rgb.r, rgb.g, rgb.b, b'\n'])
            {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        assert_eq!(NAMES.len(), 782);
        assert_eq!(hash, 0xd50f_bfdb_74bc_4d85);
    }

    /// Every entry resolves to itself lowercase, uppercase and with each
    /// word capitalised.
    #[test]
    fn every_entry_is_found_in_any_ascii_case() {
        for &(name, rgb) in &NAMES {
            let capitalised: String = name
                .split(' ')
                .map(|word| word[..1].to_ascii_uppercase() + &word[1..])
                .collect::<Vec<_>>()
                .join(" ");
            for variant in [name.to_owned(), name.to_ascii_uppercase(), capitalised] {
                assert_eq!(lookup(variant.as_bytes()), Some(rgb), "{variant:?}");
            }
        }
    }

    /// X11, not CSS, for the five names they disagree on; CSS through the
    /// `web` aliases.
    #[test]
    fn x11_values_win_over_css() {
        let rgb = |name: &str| lookup(name.as_bytes());
        assert_eq!(rgb("gray"), Some(Rgb::new(190, 190, 190)));
        assert_eq!(rgb("grey"), Some(Rgb::new(190, 190, 190)));
        assert_eq!(rgb("green"), Some(Rgb::new(0, 255, 0)));
        assert_eq!(rgb("maroon"), Some(Rgb::new(176, 48, 96)));
        assert_eq!(rgb("purple"), Some(Rgb::new(160, 32, 240)));
        assert_eq!(rgb("web gray"), Some(Rgb::new(128, 128, 128)));
        assert_eq!(rgb("WebGreen"), Some(Rgb::new(0, 128, 0)));
        assert_eq!(rgb("gray50"), Some(Rgb::new(127, 127, 127)));
        assert_eq!(rgb("DarkSlateGray"), Some(Rgb::new(47, 79, 79)));
    }

    /// Spaces are significant, nothing is trimmed, only ASCII folds and
    /// anything past the cap is rejected before the search.
    #[test]
    fn everything_else_is_unknown() {
        for name in [
            &b""[..],
            b" ",
            b"dark slategray",
            b"darkslate gray",
            b"dark  slate gray",
            b" red",
            b"red ",
            b"re",
            b"reds",
            b"gray101",
            b"transparent",
            b"cms red",
            b"\xe2\x84\xaahaki",
            b"r\xc3\xa9d",
            b"red\x00",
            b"light goldenrod yellow ",
            b"light goldenrod yellowx",
        ] {
            assert_eq!(lookup(name), None, "{:?}", String::from_utf8_lossy(name));
        }
        assert_eq!(lookup(&vec![b'a'; 1 << 16]), None);
    }

    proptest! {
        /// Arbitrary bytes, short and over the cap, never panic, and only
        /// names of the table's lengths can match.
        #[test]
        fn arbitrary_names_do_not_panic(name in prop::collection::vec(any::<u8>(), 0..40)) {
            if lookup(&name).is_some() {
                prop_assert!((3..=MAX_NAME_LEN).contains(&name.len()));
            }
        }

        /// The binary search agrees with a linear scan of the table.
        #[test]
        fn the_search_agrees_with_a_scan(
            index in 0..NAME_COUNT,
            flips in prop::collection::vec(any::<bool>(), MAX_NAME_LEN),
            edit in prop::option::of((0..MAX_NAME_LEN, any::<u8>())),
        ) {
            let (name, _) = NAMES[index];
            let mut bytes: Vec<u8> = name
                .bytes()
                .zip(&flips)
                .map(|(b, &up)| if up { b.to_ascii_uppercase() } else { b })
                .collect();
            if let Some((at, byte)) = edit {
                let at = at.min(bytes.len() - 1);
                bytes[at] = byte;
            }
            let scan = NAMES
                .iter()
                .find(|(entry, _)| entry.as_bytes().eq_ignore_ascii_case(&bytes))
                .map(|&(_, rgb)| rgb);
            prop_assert_eq!(lookup(&bytes), scan);
        }
    }
}
