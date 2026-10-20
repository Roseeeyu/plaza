//! The 16-colour palette.

use std::fmt;

/// Name and sRGB value of every palette entry, indexed by colour code.
pub const PALETTE: [(&str, [u8; 3]); 16] = [
    ("white", [0xff, 0xff, 0xff]),
    ("silver", [0xd4, 0xd7, 0xd9]),
    ("gray", [0x89, 0x8d, 0x90]),
    ("black", [0x22, 0x22, 0x22]),
    ("pink", [0xff, 0x99, 0xaa]),
    ("red", [0xbe, 0x00, 0x39]),
    ("orange", [0xff, 0x96, 0x00]),
    ("brown", [0x9c, 0x69, 0x26]),
    ("yellow", [0xff, 0xd6, 0x35]),
    ("lime", [0x7e, 0xed, 0x56]),
    ("green", [0x00, 0xa3, 0x68]),
    ("cyan", [0x00, 0xcc, 0xc0]),
    ("blue", [0x36, 0x90, 0xea]),
    ("navy", [0x24, 0x50, 0xa4]),
    ("purple", [0x81, 0x1e, 0x9f]),
    ("magenta", [0xde, 0x10, 0x7f]),
];

/// A palette colour, stored as its index (0..16).
///
/// Each colour has a one-character hex *code* (`'0'`..`'f'`), used in
/// compact contexts like snapshot rows, and a lowercase *name*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Color(u8);

impl Color {
    pub const WHITE: Color = Color(0);

    pub fn from_index(index: u8) -> Option<Color> {
        (usize::from(index) < PALETTE.len()).then_some(Color(index))
    }

    pub fn index(self) -> u8 {
        self.0
    }

    pub fn code(self) -> char {
        char::from_digit(u32::from(self.0), 16).expect("index is below 16")
    }

    pub fn from_code(code: char) -> Option<Color> {
        code.to_digit(16).map(|d| Color(d as u8))
    }

    pub fn name(self) -> &'static str {
        PALETTE[usize::from(self.0)].0
    }

    pub fn from_name(name: &str) -> Option<Color> {
        PALETTE
            .iter()
            .position(|(n, _)| *n == name)
            .map(|i| Color(i as u8))
    }

    /// Accepts either a name (`red`) or a single hex code (`5`).
    pub fn parse(s: &str) -> Option<Color> {
        let mut chars = s.chars();
        match (chars.next(), chars.next()) {
            (Some(code), None) => Color::from_code(code),
            _ => Color::from_name(s),
        }
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_codes_round_trip() {
        for i in 0..16 {
            let color = Color::from_index(i).unwrap();
            assert_eq!(Color::parse(color.name()), Some(color));
            assert_eq!(Color::parse(&color.code().to_string()), Some(color));
        }
        assert_eq!(Color::from_index(16), None);
    }

    #[test]
    fn rejects_unknown_colors() {
        assert_eq!(Color::parse("chartreuse"), None);
        assert_eq!(Color::parse("g"), None);
        assert_eq!(Color::parse(""), None);
        assert_eq!(Color::parse("55"), None);
    }
}
