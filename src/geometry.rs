//! Pixel positions and rectangular regions.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Pos {
    pub col: u32,
    pub row: u32,
}

impl fmt::Display for Pos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({}, {})", self.col, self.row)
    }
}

/// An axis-aligned rectangle of pixels. Displays as `col row width height`,
/// which is also its wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub col: u32,
    pub row: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn area(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    /// Written with subtraction rather than `col + width` so that huge
    /// client-supplied values cannot overflow.
    pub fn contains(&self, p: Pos) -> bool {
        p.col >= self.col
            && p.row >= self.row
            && p.col - self.col < self.width
            && p.row - self.row < self.height
    }

    /// Does `other` lie entirely inside `self`? Empty rects never do.
    pub fn contains_rect(&self, other: &Rect) -> bool {
        other.width >= 1
            && other.height >= 1
            && self.contains(Pos {
                col: other.col,
                row: other.row,
            })
            && other.width <= self.width - (other.col - self.col)
            && other.height <= self.height - (other.row - self.row)
    }

    /// Every position in the rect, row-major.
    pub fn positions(&self) -> impl Iterator<Item = Pos> + use<> {
        let r = *self;
        (0..r.height).flat_map(move |dy| {
            (0..r.width).map(move |dx| Pos {
                col: r.col + dx,
                row: r.row + dy,
            })
        })
    }
}

impl fmt::Display for Rect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} {} {}",
            self.col, self.row, self.width, self.height
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(col: u32, row: u32, width: u32, height: u32) -> Rect {
        Rect {
            col,
            row,
            width,
            height,
        }
    }

    #[test]
    fn contains_edges() {
        let r = rect(1, 1, 2, 2);
        assert!(r.contains(Pos { col: 2, row: 2 }));
        assert!(!r.contains(Pos { col: 3, row: 1 }));
        assert!(!r.contains(Pos { col: 0, row: 1 }));
    }

    #[test]
    fn contains_rect_is_overflow_safe() {
        let board = rect(0, 0, 64, 64);
        assert!(board.contains_rect(&rect(60, 60, 4, 4)));
        assert!(!board.contains_rect(&rect(60, 60, 5, 4)));
        assert!(!board.contains_rect(&rect(1, 1, u32::MAX, 1)));
        assert!(!board.contains_rect(&rect(0, 0, 0, 1)));
    }

    #[test]
    fn positions_are_row_major() {
        let ps: Vec<_> = rect(5, 7, 2, 2)
            .positions()
            .map(|p| (p.col, p.row))
            .collect();
        assert_eq!(ps, [(5, 7), (6, 7), (5, 8), (6, 8)]);
    }
}
