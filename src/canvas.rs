//! The canvas: pixel colours, who painted each pixel, and the commit counter.

use std::collections::HashMap;
use std::sync::Arc;

use crate::color::Color;
use crate::geometry::{Pos, Rect};

/// Marks a pixel nobody has painted yet.
const NO_AUTHOR: u32 = u32::MAX;

/// One atomic write to the canvas. A `paint` is a 1x1 commit; a `stamp` is
/// a larger one. Commits are also the unit the persistence log records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub seq: u64,
    pub author: Arc<str>,
    pub region: Rect,
    pub colors: Vec<Color>,
}

impl Commit {
    pub fn cells(&self) -> impl Iterator<Item = (Pos, Color)> + '_ {
        self.region.positions().zip(self.colors.iter().copied())
    }
}

#[derive(Debug, Clone)]
pub struct Canvas {
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// Row-major colours.
    pub(crate) pixels: Vec<Color>,
    /// Row-major indices into `names`, or `NO_AUTHOR`. Interning keeps this
    /// at 4 bytes per pixel however long nicknames are.
    pub(crate) authors: Vec<u32>,
    pub(crate) names: Vec<Arc<str>>,
    name_ids: HashMap<Arc<str>, u32>,
    pub(crate) seq: u64,
}

impl Canvas {
    pub fn new(width: u32, height: u32) -> Self {
        let len = width as usize * height as usize;
        Canvas {
            width,
            height,
            pixels: vec![Color::WHITE; len],
            authors: vec![NO_AUTHOR; len],
            names: Vec::new(),
            name_ids: HashMap::new(),
            seq: 0,
        }
    }

    /// Rebuilds a canvas from its raw parts (used when loading a snapshot).
    pub(crate) fn from_parts(
        width: u32,
        height: u32,
        seq: u64,
        names: Vec<Arc<str>>,
        pixels: Vec<Color>,
        authors: Vec<u32>,
    ) -> Self {
        let name_ids = names
            .iter()
            .enumerate()
            .map(|(i, n)| (Arc::clone(n), i as u32))
            .collect();
        Canvas {
            width,
            height,
            pixels,
            authors,
            names,
            name_ids,
            seq,
        }
    }

    pub fn board(&self) -> Rect {
        Rect {
            col: 0,
            row: 0,
            width: self.width,
            height: self.height,
        }
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    fn index(&self, pos: Pos) -> usize {
        pos.row as usize * self.width as usize + pos.col as usize
    }

    fn intern(&mut self, name: &Arc<str>) -> u32 {
        if let Some(&id) = self.name_ids.get(name) {
            return id;
        }
        let id = self.names.len() as u32;
        self.names.push(Arc::clone(name));
        self.name_ids.insert(Arc::clone(name), id);
        id
    }

    /// Validates and applies a write, returning the commit it became.
    pub fn commit(
        &mut self,
        author: &Arc<str>,
        region: Rect,
        colors: Vec<Color>,
    ) -> Result<Commit, String> {
        if !self.board().contains_rect(&region) {
            return Err(if region.area() == 1 {
                format!(
                    "{} is off the canvas",
                    Pos {
                        col: region.col,
                        row: region.row
                    }
                )
            } else {
                format!("region ({region}) is off the canvas")
            });
        }
        let commit = Commit {
            seq: self.seq + 1,
            author: Arc::clone(author),
            region,
            colors,
        };
        self.apply(&commit);
        Ok(commit)
    }

    /// Applies an already-validated commit, e.g. one replayed from the log.
    /// Commits at or before the current seq are already reflected and are
    /// skipped, which makes replay idempotent.
    pub fn apply(&mut self, commit: &Commit) {
        if commit.seq <= self.seq || !self.board().contains_rect(&commit.region) {
            return;
        }
        let author = self.intern(&commit.author);
        for (pos, color) in commit.cells() {
            let i = self.index(pos);
            self.pixels[i] = color;
            self.authors[i] = author;
        }
        self.seq = commit.seq;
    }

    pub fn read(&self, pos: Pos) -> Result<(Color, Option<Arc<str>>), String> {
        if !self.board().contains(pos) {
            return Err(format!("{pos} is off the canvas"));
        }
        let i = self.index(pos);
        let author = self.names.get(self.authors[i] as usize).map(Arc::clone);
        Ok((self.pixels[i], author))
    }

    pub fn snapshot(&self, region: Option<Rect>) -> Result<(Rect, Vec<Vec<Color>>), String> {
        let region = region.unwrap_or_else(|| self.board());
        if !self.board().contains_rect(&region) {
            return Err(format!("region ({region}) is off the canvas"));
        }
        let rows = (region.row..region.row + region.height)
            .map(|row| {
                let start = self.index(Pos {
                    col: region.col,
                    row,
                });
                self.pixels[start..start + region.width as usize].to_vec()
            })
            .collect();
        Ok((region, rows))
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
    fn commits_bump_seq_and_record_author() {
        let mut canvas = Canvas::new(4, 4);
        let ann: Arc<str> = "ann".into();
        let red = Color::parse("red").unwrap();
        let c = canvas
            .commit(&ann, rect(1, 1, 2, 1), vec![red, red])
            .unwrap();
        assert_eq!(c.seq, 1);
        assert_eq!(canvas.seq(), 1);
        assert_eq!(
            canvas.read(Pos { col: 2, row: 1 }).unwrap(),
            (red, Some(ann))
        );
        assert_eq!(
            canvas.read(Pos { col: 0, row: 0 }).unwrap(),
            (Color::WHITE, None)
        );
    }

    #[test]
    fn rejects_off_canvas_writes() {
        let mut canvas = Canvas::new(4, 4);
        let ann: Arc<str> = "ann".into();
        assert!(
            canvas
                .commit(&ann, rect(4, 0, 1, 1), vec![Color::WHITE])
                .is_err()
        );
        assert!(
            canvas
                .commit(&ann, rect(3, 3, 2, 2), vec![Color::WHITE; 4])
                .is_err()
        );
        assert_eq!(canvas.seq(), 0);
    }

    #[test]
    fn apply_skips_old_commits() {
        let mut canvas = Canvas::new(2, 2);
        let ann: Arc<str> = "ann".into();
        let red = Color::parse("red").unwrap();
        let c = canvas.commit(&ann, rect(0, 0, 1, 1), vec![red]).unwrap();
        canvas
            .commit(&ann, rect(0, 0, 1, 1), vec![Color::WHITE])
            .unwrap();
        canvas.apply(&c);
        assert_eq!(canvas.read(Pos { col: 0, row: 0 }).unwrap().0, Color::WHITE);
    }

    #[test]
    fn snapshot_returns_rows() {
        let mut canvas = Canvas::new(3, 2);
        let ann: Arc<str> = "ann".into();
        let red = Color::parse("red").unwrap();
        canvas.commit(&ann, rect(1, 1, 1, 1), vec![red]).unwrap();
        let (_, rows) = canvas.snapshot(Some(rect(1, 0, 2, 2))).unwrap();
        assert_eq!(rows, vec![vec![Color::WHITE; 2], vec![red, Color::WHITE]]);
    }
}
