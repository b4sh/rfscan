//! Large 5-row block font for the frequency (digits and the decimal point).

/// Glyph rows: `#` is a filled cell (rendered as two block characters).
fn glyph(c: char) -> Option<[&'static str; 5]> {
    Some(match c {
        '0' => ["###", "#.#", "#.#", "#.#", "###"],
        '1' => [".#.", "##.", ".#.", ".#.", "###"],
        '2' => ["###", "..#", "###", "#..", "###"],
        '3' => ["###", "..#", ".##", "..#", "###"],
        '4' => ["#.#", "#.#", "###", "..#", "..#"],
        '5' => ["###", "#..", "###", "..#", "###"],
        '6' => ["###", "#..", "###", "#.#", "###"],
        '7' => ["###", "..#", "..#", "..#", "..#"],
        '8' => ["###", "#.#", "###", "#.#", "###"],
        '9' => ["###", "#.#", "###", "..#", "###"],
        '.' => [".", ".", ".", ".", "#"],
        // A blank of the dot's width (keeps "SCAN..." in place while the dots appear).
        ' ' => [".", ".", ".", ".", "."],
        'S' => ["###", "#..", "###", "..#", "###"],
        'C' => ["###", "#..", "#..", "#..", "###"],
        'A' => ["###", "#.#", "###", "#.#", "#.#"],
        'N' => ["#..#", "##.#", "#.##", "#..#", "#..#"],
        'I' => ["###", ".#.", ".#.", ".#.", "###"],
        'G' => ["###", "#..", "#.#", "#.#", "###"],
        _ => return None,
    })
}

pub const HEIGHT: usize = 5;

/// The 5 rows of `text` in the large font (unknown characters are skipped).
pub fn render(text: &str) -> [String; HEIGHT] {
    let mut rows: [String; HEIGHT] = Default::default();
    for (n, g) in text.chars().filter_map(glyph).enumerate() {
        for (row, pattern) in rows.iter_mut().zip(g) {
            if n > 0 {
                row.push(' ');
            }
            for cell in pattern.chars() {
                row.push_str(if cell == '#' { "██" } else { "  " });
            }
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_frequency() {
        let r = render("145.500");
        assert_eq!(r[0], "  ██   ██  ██ ██████    ██████ ██████ ██████");
        assert_eq!(r[4], "██████     ██ ██████ ██ ██████ ██████ ██████");
        assert_eq!(r[0].chars().count(), 44);
    }
}
