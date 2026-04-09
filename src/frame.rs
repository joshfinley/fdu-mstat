//! Cell-based frame buffer with cursor-home overwrite for live updates.
//!
//! The frame is a 2D grid of `Cell`s. Each cell stores one Unicode character
//! (up to 4 UTF-8 bytes). Rendering fills the grid via `put_*` methods, then
//! `write_full` serializes it to a byte buffer for a single `write()` syscall.

use crate::buf::Buf;

pub const FRAME_W: usize = 120;
pub const FRAME_H: usize = 80;

// -- Cell --------------------------------------------------------------------

/// One display-column-wide character stored as UTF-8 bytes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub b: [u8; 4],
    pub len: u8,
}

impl Cell {
    pub const SPACE: Cell = Cell {
        b: [b' ', 0, 0, 0],
        len: 1,
    };

    #[inline]
    pub fn from_char(ch: char) -> Cell {
        let mut b = [0u8; 4];
        let len = ch.encode_utf8(&mut b).len() as u8;
        Cell { b, len }
    }
}

// -- Divider style -----------------------------------------------------------

#[derive(Clone, Copy)]
pub enum DivStyle {
    Top,
    Mid,
    Bot,
}

// -- Frame -------------------------------------------------------------------

pub struct Frame {
    cells: [Cell; FRAME_W * FRAME_H],
    pub w: usize,
    pub h: usize,
    pub col: usize,
    pub row: usize,
}

impl Frame {
    pub fn new(w: usize, h: usize) -> Self {
        Frame {
            cells: [Cell::SPACE; FRAME_W * FRAME_H],
            w: w.min(FRAME_W),
            h: h.min(FRAME_H),
            col: 0,
            row: 0,
        }
    }

    // -- Low-level cell writers ------------------------------------------

    #[inline]
    pub fn put(&mut self, cell: Cell) {
        if self.col < self.w && self.row < self.h {
            self.cells[self.row * FRAME_W + self.col] = cell;
            self.col += 1;
        }
    }

    #[inline]
    pub fn put_char(&mut self, ch: char) {
        self.put(Cell::from_char(ch));
    }

    pub fn fill_char(&mut self, ch: char, n: usize) {
        let cell = Cell::from_char(ch);
        for _ in 0..n {
            self.put(cell);
        }
    }

    pub fn put_str(&mut self, s: &str) {
        for ch in s.chars() {
            self.put_char(ch);
        }
    }

    pub fn fill_spaces(&mut self, n: usize) {
        for _ in 0..n {
            self.put(Cell::SPACE);
        }
    }

    #[inline]
    pub fn newline(&mut self) {
        self.col = 0;
        self.row += 1;
    }

    // -- High-level report primitives ------------------------------------

    /// Top decoration: ┌────...────┐
    pub fn put_header(&mut self) {
        let iw = self.w - 2;
        self.put_char('┌');
        self.fill_char('─', iw);
        self.put_char('┐');
        self.newline();
    }

    /// Centered text row: │   text   │
    pub fn put_centered(&mut self, text: &str) {
        let iw = self.w - 2;
        let tlen = text.chars().count().min(iw);
        let pl = (iw - tlen) / 2;
        let pr = iw - tlen - pl;
        self.put_char('│');
        self.fill_spaces(pl);
        for (i, ch) in text.chars().enumerate() {
            if i >= iw {
                break;
            }
            self.put_char(ch);
        }
        self.fill_spaces(pr);
        self.put_char('│');
        self.newline();
    }

    /// Horizontal divider with a junction at the name/data column boundary.
    pub fn put_divider(&mut self, style: DivStyle, nc: usize) {
        let (l, m, r) = match style {
            DivStyle::Top => ('├', '┬', '┤'),
            DivStyle::Mid => ('├', '┼', '┤'),
            DivStyle::Bot => ('└', '┴', '┘'),
        };
        let iw = self.w - 2;
        let junc = nc + 2; // after "│ " + name_col + " "
        self.put_char(l);
        for i in 0..iw {
            if i == junc {
                self.put_char(m);
            } else {
                self.put_char('─');
            }
        }
        self.put_char(r);
        self.newline();
    }

    /// Data row: │ NAME          │ DATA                          │
    pub fn put_row(&mut self, name: &str, data: &str, nc: usize, dc: usize) {
        self.put_char('│');
        self.put(Cell::SPACE);
        self.put_str_padded(name, nc);
        self.put(Cell::SPACE);
        self.put_char('│');
        self.put(Cell::SPACE);
        self.put_str_padded(data, dc);
        self.put(Cell::SPACE);
        self.put_char('│');
        self.newline();
    }

    /// Data row sourced from a `Buf`.
    pub fn put_row_buf<const N: usize>(&mut self, name: &str, data: &Buf<N>, nc: usize, dc: usize) {
        self.put_row(name, data.as_str(), nc, dc);
    }

    /// Data row with a bar graph in the data column.
    pub fn put_bar_row(&mut self, name: &str, used: f64, total: f64, nc: usize, dc: usize) {
        self.put_char('│');
        self.put(Cell::SPACE);
        self.put_str_padded(name, nc);
        self.put(Cell::SPACE);
        self.put_char('│');
        self.put(Cell::SPACE);
        self.put_bar(used, total, dc);
        self.put(Cell::SPACE);
        self.put_char('│');
        self.newline();
    }

    /// Empty row for vertical fill.
    pub fn put_empty_row(&mut self, nc: usize, dc: usize) {
        self.put_char('│');
        self.put(Cell::SPACE);
        self.fill_spaces(nc);
        self.put(Cell::SPACE);
        self.put_char('│');
        self.put(Cell::SPACE);
        self.fill_spaces(dc);
        self.put(Cell::SPACE);
        self.put_char('│');
        self.newline();
    }

    /// Write a string padded/truncated to exactly `width` display columns.
    fn put_str_padded(&mut self, s: &str, width: usize) {
        let cc = s.chars().count();
        if cc > width {
            for (i, ch) in s.chars().enumerate() {
                if i >= width - 3 {
                    break;
                }
                self.put_char(ch);
            }
            self.put_char('.');
            self.put_char('.');
            self.put_char('.');
        } else {
            self.put_str(s);
            self.fill_spaces(width - cc);
        }
    }

    /// Render a bar graph: ████░░░░ proportional to used/total.
    fn put_bar(&mut self, used: f64, total: f64, width: usize) {
        let pct = if total > 0.0 {
            (used / total).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let filled = (pct * width as f64 + 0.5) as usize;
        let filled = filled.min(width);
        self.fill_char('█', filled);
        self.fill_char('░', width - filled);
    }

    // -- Serialization ---------------------------------------------------

    /// Serialize all cells to bytes. No trailing newline after the last row
    /// (prevents terminal scroll when frame fills the screen).
    pub fn write_full(&self, out: &mut [u8]) -> usize {
        let mut pos = 0;
        for row in 0..self.h {
            let base = row * FRAME_W;
            for col in 0..self.w {
                let cell = &self.cells[base + col];
                let len = cell.len as usize;
                unsafe {
                    std::ptr::copy_nonoverlapping(cell.b.as_ptr(), out.as_mut_ptr().add(pos), len);
                }
                pos += len;
            }
            if row + 1 < self.h {
                out[pos] = b'\n';
                pos += 1;
            }
        }
        pos
    }
}
