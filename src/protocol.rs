//! The line-based wire protocol: one request per line in, one reply per
//! message out (a snapshot reply spans several lines).
//!
//! ```text
//! hello <name>                          -> welcome <name>
//! paint <col> <row> <color>             -> painted <seq> | cooldown <ms>
//! stamp <col> <row> <w> <h> <codes>     -> stamped <seq> | cooldown <ms>
//! read <col> <row>                      -> pixel <col> <row> <color> <author|->
//! snapshot [<col> <row> <w> <h>]        -> snapshot <seq> <col> <row> <w> <h>
//!                                          <one line of colour codes per row>
//! subscribe <col> <row> <w> <h>         -> subscribed <seq>
//!                                          then: diff <seq> <col> <row> <color> <author>
//!                                          or:   lagged
//! unsubscribe                           -> unsubscribed
//! ```

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use crate::color::Color;
use crate::geometry::{Pos, Rect};

/// Longest nickname a user may pick.
pub const MAX_NAME_LEN: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Hello { name: String },
    Paint { pos: Pos, color: Color },
    Stamp { region: Rect, colors: Vec<Color> },
    Read { pos: Pos },
    Snapshot { region: Option<Rect> },
    Subscribe { region: Rect },
    Unsubscribe,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Welcome {
        name: Arc<str>,
    },
    Painted {
        seq: u64,
    },
    Stamped {
        seq: u64,
    },
    Cooldown {
        remaining_ms: u64,
    },
    Pixel {
        pos: Pos,
        color: Color,
        author: Option<Arc<str>>,
    },
    Snapshot {
        seq: u64,
        region: Rect,
        rows: Vec<Vec<Color>>,
    },
    Subscribed {
        seq: u64,
    },
    Unsubscribed,
    /// One changed pixel. Every diff from one commit shares its `seq` and
    /// they arrive back to back.
    Diff {
        seq: u64,
        pos: Pos,
        color: Color,
        author: Arc<str>,
    },
    /// The subscriber fell more than the lag budget behind and was dropped.
    Lagged,
    Error(String),
}

/// Nicknames are short and contain no whitespace, so they fit in a single
/// token of every reply that carries one.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn num(token: &str, what: &str) -> Result<u32, String> {
    token
        .parse()
        .map_err(|_| format!("{what} must be a non-negative number, got '{token}'"))
}

fn pos(col: &str, row: &str) -> Result<Pos, String> {
    Ok(Pos {
        col: num(col, "col")?,
        row: num(row, "row")?,
    })
}

fn rect(col: &str, row: &str, width: &str, height: &str) -> Result<Rect, String> {
    Ok(Rect {
        col: num(col, "col")?,
        row: num(row, "row")?,
        width: num(width, "width")?,
        height: num(height, "height")?,
    })
}

impl FromStr for Request {
    type Err = String;

    fn from_str(line: &str) -> Result<Self, Self::Err> {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let Some((&command, args)) = tokens.split_first() else {
            return Err("expected a command".to_string());
        };

        match (command, args) {
            ("hello", [name]) if valid_name(name) => Ok(Request::Hello {
                name: name.to_string(),
            }),
            ("hello", [_]) => Err(format!(
                "names are 1-{MAX_NAME_LEN} letters, digits, '_' or '-'"
            )),
            ("hello", _) => Err("usage: hello <name>".to_string()),

            ("paint", [col, row, color]) => Ok(Request::Paint {
                pos: pos(col, row)?,
                color: Color::parse(color)
                    .ok_or_else(|| format!("'{color}' is not a color name or code"))?,
            }),
            ("paint", _) => Err("usage: paint <col> <row> <color>".to_string()),

            ("stamp", [col, row, width, height, codes]) => {
                let region = rect(col, row, width, height)?;
                if region.width == 0 || region.height == 0 {
                    return Err("stamp dimensions must be at least 1".to_string());
                }
                let colors = codes
                    .chars()
                    .map(Color::from_code)
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| format!("'{codes}' is not a string of color codes"))?;
                if colors.len() as u64 != region.area() {
                    return Err(format!(
                        "a {}x{} stamp needs {} color codes, got {}",
                        region.width,
                        region.height,
                        region.area(),
                        colors.len()
                    ));
                }
                Ok(Request::Stamp { region, colors })
            }
            ("stamp", _) => Err("usage: stamp <col> <row> <width> <height> <codes>".to_string()),

            ("read", [col, row]) => Ok(Request::Read {
                pos: pos(col, row)?,
            }),
            ("read", _) => Err("usage: read <col> <row>".to_string()),

            ("snapshot", []) => Ok(Request::Snapshot { region: None }),
            ("snapshot", [col, row, width, height]) => Ok(Request::Snapshot {
                region: Some(rect(col, row, width, height)?),
            }),
            ("snapshot", _) => Err("usage: snapshot [<col> <row> <width> <height>]".to_string()),

            ("subscribe", [col, row, width, height]) => Ok(Request::Subscribe {
                region: rect(col, row, width, height)?,
            }),
            ("subscribe", _) => Err("usage: subscribe <col> <row> <width> <height>".to_string()),

            ("unsubscribe", []) => Ok(Request::Unsubscribe),
            ("unsubscribe", _) => Err("usage: unsubscribe".to_string()),

            _ => Err(format!("unknown command '{command}'")),
        }
    }
}

impl fmt::Display for Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reply::Welcome { name } => write!(f, "welcome {name}"),
            Reply::Painted { seq } => write!(f, "painted {seq}"),
            Reply::Stamped { seq } => write!(f, "stamped {seq}"),
            Reply::Cooldown { remaining_ms } => write!(f, "cooldown {remaining_ms}"),
            Reply::Pixel { pos, color, author } => write!(
                f,
                "pixel {} {} {color} {}",
                pos.col,
                pos.row,
                author.as_deref().unwrap_or("-")
            ),
            Reply::Snapshot { seq, region, rows } => {
                write!(f, "snapshot {seq} {region}")?;
                for row in rows {
                    f.write_str("\n")?;
                    for color in row {
                        write!(f, "{}", color.code())?;
                    }
                }
                Ok(())
            }
            Reply::Subscribed { seq } => write!(f, "subscribed {seq}"),
            Reply::Unsubscribed => write!(f, "unsubscribed"),
            Reply::Diff {
                seq,
                pos,
                color,
                author,
            } => write!(f, "diff {seq} {} {} {color} {author}", pos.col, pos.row),
            Reply::Lagged => write!(f, "lagged"),
            Reply::Error(message) => write!(f, "error: {message}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hello_and_validates_names() {
        assert_eq!(
            "hello rose_1".parse(),
            Ok(Request::Hello {
                name: "rose_1".to_string()
            })
        );
        assert!("hello".parse::<Request>().is_err());
        assert!("hello a b".parse::<Request>().is_err());
        assert!("hello ab$".parse::<Request>().is_err());
        assert!("hello abcdefghijklmnopq".parse::<Request>().is_err());
    }

    #[test]
    fn parses_paint_by_name_and_code() {
        let expected = Ok(Request::Paint {
            pos: Pos { col: 3, row: 4 },
            color: Color::parse("red").unwrap(),
        });
        assert_eq!("paint 3 4 red".parse(), expected);
        assert_eq!("  paint 3 4 5 ".parse(), expected);
        assert!("paint 3 -4 red".parse::<Request>().is_err());
    }

    #[test]
    fn stamp_checks_shape() {
        assert!("stamp 1 2 2 2 5555".parse::<Request>().is_ok());
        assert!("stamp 1 2 2 2 555".parse::<Request>().is_err());
        assert!("stamp 1 2 0 2 55".parse::<Request>().is_err());
        assert!("stamp 1 2 1 1 z".parse::<Request>().is_err());
    }

    #[test]
    fn replies_display_as_wire_format() {
        let red = Color::parse("red").unwrap();
        let pos = Pos { col: 3, row: 4 };
        assert_eq!(
            Reply::Diff {
                seq: 9,
                pos,
                color: red,
                author: "ann".into()
            }
            .to_string(),
            "diff 9 3 4 red ann"
        );
        assert_eq!(
            Reply::Pixel {
                pos,
                color: red,
                author: None
            }
            .to_string(),
            "pixel 3 4 red -"
        );
        let snapshot = Reply::Snapshot {
            seq: 7,
            region: Rect {
                col: 0,
                row: 0,
                width: 2,
                height: 2,
            },
            rows: vec![vec![Color::WHITE, red], vec![red, Color::WHITE]],
        };
        assert_eq!(snapshot.to_string(), "snapshot 7 0 0 2 2\n05\n50");
    }
}
