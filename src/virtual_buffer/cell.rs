//! Cell / Line model shared by `terminal_core` (live grid) and
//! `virtual_buffer` (scrollback history), plus the binary encoding used to
//! persist a [`Line`] into the disk cache without losing ANSI styling
//! (needed for "Copy as ANSI Color" and for repainting historical lines
//! with correct colors after a scroll).

use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use std::io::{self, Cursor, Read};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Attrs {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
    pub strikethrough: bool,
}

impl Attrs {
    fn to_bits(self) -> u8 {
        (self.bold as u8)
            | ((self.italic as u8) << 1)
            | ((self.underline as u8) << 2)
            | ((self.reverse as u8) << 3)
            | ((self.strikethrough as u8) << 4)
    }
    fn from_bits(b: u8) -> Self {
        Self {
            bold: b & 1 != 0,
            italic: b & 2 != 0,
            underline: b & 4 != 0,
            reverse: b & 8 != 0,
            strikethrough: b & 16 != 0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
}

impl Default for Cell {
    fn default() -> Self {
        Self { ch: ' ', fg: Color::Default, bg: Color::Default, attrs: Attrs::default() }
    }
}

/// One row of terminal cells. Once a row scrolls out of the live viewport
/// it is handed to [`crate::virtual_buffer::VirtualBuffer::push_line`] and
/// becomes immutable history.
#[derive(Clone, Debug, Default)]
pub struct Line {
    pub cells: Vec<Cell>,
    /// True if the next line is a soft-wrap continuation of this one
    /// (affects reflow-aware copy/search; Phase 2).
    pub wrapped: bool,
}

impl Line {
    pub fn plain_text(&self) -> String {
        self.cells.iter().map(|c| c.ch).collect::<String>().trim_end().to_string()
    }

    /// Renders the line back out as raw bytes containing real ANSI SGR
    /// escape sequences, suitable for "Copy as ANSI Color".
    pub fn to_ansi_string(&self) -> String {
        let mut out = String::new();
        let mut last_fg = Color::Default;
        let mut last_bg = Color::Default;
        let mut last_attrs = 0u8;
        for cell in &self.cells {
            let attrs_bits = cell.attrs.to_bits();
            if cell.fg != last_fg || cell.bg != last_bg || attrs_bits != last_attrs {
                out.push_str("\x1b[0");
                if cell.attrs.bold { out.push_str(";1"); }
                if cell.attrs.italic { out.push_str(";3"); }
                if cell.attrs.underline { out.push_str(";4"); }
                if cell.attrs.reverse { out.push_str(";7"); }
                if cell.attrs.strikethrough { out.push_str(";9"); }
                match cell.fg {
                    Color::Default => {}
                    Color::Indexed(i) => out.push_str(&format!(";38;5;{i}")),
                    Color::Rgb(r, g, b) => out.push_str(&format!(";38;2;{r};{g};{b}")),
                }
                match cell.bg {
                    Color::Default => {}
                    Color::Indexed(i) => out.push_str(&format!(";48;5;{i}")),
                    Color::Rgb(r, g, b) => out.push_str(&format!(";48;2;{r};{g};{b}")),
                }
                out.push('m');
                last_fg = cell.fg;
                last_bg = cell.bg;
                last_attrs = attrs_bits;
            }
            out.push(cell.ch);
        }
        out.push_str("\x1b[0m");
        out
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.cells.len() * 13 + 1);
        buf.push(self.wrapped as u8);
        for cell in &self.cells {
            let _ = write_cell(&mut buf, cell);
        }
        buf
    }

    pub fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.is_empty() {
            return Ok(Line::default());
        }
        let wrapped = bytes[0] != 0;
        let mut cursor = Cursor::new(&bytes[1..]);
        let mut cells = Vec::with_capacity(bytes.len() / 13);
        while (cursor.position() as usize) < cursor.get_ref().len() {
            cells.push(read_cell(&mut cursor)?);
        }
        Ok(Line { cells, wrapped })
    }
}

fn write_color(buf: &mut Vec<u8>, c: Color) {
    match c {
        Color::Default => { buf.push(0); buf.extend_from_slice(&[0, 0, 0]); }
        Color::Indexed(i) => { buf.push(1); buf.extend_from_slice(&[i, 0, 0]); }
        Color::Rgb(r, g, b) => { buf.push(2); buf.extend_from_slice(&[r, g, b]); }
    }
}

fn read_color(cur: &mut Cursor<&[u8]>) -> io::Result<Color> {
    let tag = cur.read_u8()?;
    let mut rgb = [0u8; 3];
    cur.read_exact(&mut rgb)?;
    Ok(match tag {
        0 => Color::Default,
        1 => Color::Indexed(rgb[0]),
        _ => Color::Rgb(rgb[0], rgb[1], rgb[2]),
    })
}

fn write_cell(buf: &mut Vec<u8>, cell: &Cell) -> io::Result<()> {
    buf.write_u32::<LittleEndian>(cell.ch as u32)?;
    write_color(buf, cell.fg);
    write_color(buf, cell.bg);
    buf.push(cell.attrs.to_bits());
    Ok(())
}

fn read_cell(cur: &mut Cursor<&[u8]>) -> io::Result<Cell> {
    let cp = cur.read_u32::<LittleEndian>()?;
    let ch = char::from_u32(cp).unwrap_or(' ');
    let fg = read_color(cur)?;
    let bg = read_color(cur)?;
    let attrs = Attrs::from_bits(cur.read_u8()?);
    Ok(Cell { ch, fg, bg, attrs })
}
