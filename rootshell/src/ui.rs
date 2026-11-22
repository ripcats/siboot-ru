use std::io::Write as _;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

const ACCENT: &str = "36";
const STRONG: &str = "1;36";
const GOOD: &str = "1;32";
const WARN: &str = "1;33";
const BAD: &str = "1;31";
const DIM: &str = "2";
const BOLD: &str = "1";
const BADGE: &str = "1;30;46";

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const FRAME: Duration = Duration::from_millis(80);
const DEFAULT_COLUMNS: usize = 40;

struct Screen {
    fancy: bool,
    color: bool,
    measure: usize,
    live: Option<(&'static str, &'static str, String)>,
    frame: usize,
    last: String,
}

static SCREEN: Mutex<Screen> = Mutex::new(Screen {
    fancy: false,
    color: false,
    measure: DEFAULT_COLUMNS - 1,
    live: None,
    frame: 0,
    last: String::new(),
});

struct Record {
    lines: Vec<String>,
    last: String,
}

static RECORD: Mutex<Record> = Mutex::new(Record {
    lines: Vec::new(),
    last: String::new(),
});

fn record(text: &str) {
    let mut r = RECORD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for line in text.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() || line == r.last {
            continue;
        }
        r.last = line.to_string();
        if r.lines.len() >= 2000 {
            r.lines.remove(100);
        }
        r.lines.push(line.to_string());
    }
}

pub fn recorded() -> Vec<String> {
    RECORD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .lines
        .clone()
}

fn screen() -> MutexGuard<'static, Screen> {
    SCREEN
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn init(plain: bool) {
    let mut s = screen();
    s.fancy = !plain && !cfg!(test) && !matches!(std::env::var("TERM").as_deref(), Ok("dumb"));
    s.color = s.fancy && std::env::var_os("NO_COLOR").is_none();
    let columns = std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.trim().parse::<usize>().ok())
        .filter(|c| (20..=400).contains(c))
        .unwrap_or(DEFAULT_COLUMNS);
    s.measure = columns.min(72) - 1;
}

#[derive(Clone, Copy)]
pub enum Mark {
    Good,
    Warn,
    Info,
}

impl Mark {
    fn glyph(self) -> (&'static str, &'static str) {
        match self {
            Mark::Good => ("✓", GOOD),
            Mark::Warn => ("▲", WARN),
            Mark::Info => ("·", DIM),
        }
    }

    fn word(self) -> &'static str {
        match self {
            Mark::Good => "OK",
            Mark::Warn => "!!",
            Mark::Info => "  ",
        }
    }
}

impl Screen {
    fn paint(&self, style: &str, text: &str) -> String {
        if self.color && !style.is_empty() && !text.is_empty() {
            format!("\x1b[{style}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    fn live_line(&self) -> String {
        match &self.live {
            Some((mark, style, text)) => {
                let room = self.measure.saturating_sub(4);
                format!("  {} {}", self.paint(style, mark), cut(text, room))
            }
            None => String::new(),
        }
    }

    fn emit(&mut self, lines: &[String]) {
        let mut out = String::new();
        if self.fancy && self.live.is_some() {
            out.push_str("\r\x1b[2K");
        }
        for line in lines {
            out.push_str(line);
            out.push('\n');
        }
        if self.fancy && self.live.is_some() {
            out.push_str(&self.live_line());
        }
        print!("{out}");
        let _ = std::io::stdout().flush();
    }

    fn set_live(&mut self, mark: &'static str, style: &'static str, text: &str) {
        if !self.fancy {
            if self.last != text {
                self.last = text.to_string();
                println!("  {text}");
            }
            return;
        }
        self.live = Some((mark, style, text.to_string()));
        print!("\r\x1b[2K{}", self.live_line());
        let _ = std::io::stdout().flush();
    }

    fn hanging(&self, lead: &str, lead_width: usize, style: &str, text: &str) -> Vec<String> {
        let room = self.measure.saturating_sub(lead_width).max(10);
        wrap(text, room)
            .into_iter()
            .enumerate()
            .map(|(n, piece)| {
                let start = if n == 0 {
                    lead.to_string()
                } else {
                    " ".repeat(lead_width)
                };
                format!("{start}{}", self.paint(style, &piece))
            })
            .collect()
    }
}

pub fn header(version: &str) {
    record(&format!("siboot-pair {version} log"));
    let mut s = screen();
    let lines = if s.fancy {
        let mut lines = vec![
            String::new(),
            format!(
                "  {} {}",
                s.paint(BADGE, " siboot-pair "),
                s.paint(DIM, version)
            ),
        ];
        lines.extend(s.hanging(
            "  ",
            2,
            "",
            "Сопрягает этот iPhone с самим собой для siboot.",
        ));
        lines
    } else {
        vec![format!(
            "siboot-pair {version} — сопрягает этот iPhone с самим собой"
        )]
    };
    s.emit(&lines);
}

pub fn say(mark: Mark, text: &str) {
    record(&format!("{} {text}", mark.word()));
    let mut s = screen();
    let lines = if s.fancy {
        let (glyph, style) = mark.glyph();
        let lead = format!("  {} ", s.paint(style, glyph));
        s.hanging(&lead, 4, "", text)
    } else {
        vec![format!("{} {text}", mark.word())]
    };
    s.emit(&lines);
}

fn paragraph(style: &str, text: &str) {
    record(&format!("   {text}"));
    let mut s = screen();
    let lines = if s.fancy {
        s.hanging("  ", 2, style, text)
    } else {
        vec![text.to_string()]
    };
    s.emit(&lines);
}

pub fn text(text: &str) {
    paragraph("", text);
}

pub fn heading(text: &str) {
    paragraph(BOLD, text);
}

pub fn dim(text: &str) {
    paragraph(DIM, text);
}

pub fn numbered(n: usize, text: &str) {
    record(&format!("   {n}. {text}"));
    let mut s = screen();
    let lines = if s.fancy {
        let lead = format!("  {} ", s.paint(STRONG, &format!("{n:>2}")));
        s.hanging(&lead, 5, "", text)
    } else {
        vec![format!("  {n}. {text}")]
    };
    s.emit(&lines);
}

pub fn command(text: &str) {
    record(&format!("     {text}"));
    let mut s = screen();
    let line = format!("    {}", s.paint(ACCENT, text));
    s.emit(&[line]);
}

pub fn blank() {
    screen().emit(&[String::new()]);
}

pub fn live(text: &str) {
    record(&format!(" … {text}"));
    screen().set_live("◆", STRONG, text);
}

pub fn spin(text: &str, time: Duration) {
    record(&format!(" … {text}"));
    let frames = (time.as_millis() / FRAME.as_millis()).max(1);
    for _ in 0..frames {
        {
            let mut s = screen();
            if s.fancy {
                s.frame = s.frame.wrapping_add(1);
                let glyph = SPINNER[s.frame % SPINNER.len()];
                s.set_live(glyph, ACCENT, text);
            } else {
                s.set_live("", "", text);
            }
        }
        std::thread::sleep(FRAME);
    }
}

pub fn settle(mark: Mark, text: &str) {
    let mut s = screen();
    if s.fancy && s.live.take().is_some() {
        print!("\r\x1b[2K");
    }
    s.last.clear();
    drop(s);
    say(mark, text);
}

pub fn end() {
    let mut s = screen();
    if s.fancy && s.live.take().is_some() {
        println!();
        let _ = std::io::stdout().flush();
    }
}

pub fn stopped(problem: &str) {
    record(&format!("Остановлено: {problem}"));
    let mut s = screen();
    if s.fancy && s.live.take().is_some() {
        print!("\r\x1b[2K");
    }
    let lines = if s.fancy {
        let lead = format!("  {} ", s.paint(BAD, "✗"));
        let mut lines = vec![String::new()];
        lines.extend(s.hanging(&lead, 4, "", &format!("Остановлено: {problem}")));
        lines
    } else {
        vec![String::new(), format!("Остановлено: {problem}")]
    };
    s.emit(&lines);
}

pub fn big(code: &str) {
    record("     (показан шестизначный код крупными цифрами)");
    let mut s = screen();
    let spaced = spaced(code);
    if !s.fancy {
        s.emit(&[format!("    {spaced}")]);
        return;
    }
    let mut lines: Vec<String> = big_digits(code)
        .into_iter()
        .map(|row| format!("    {}", s.paint(STRONG, &row)))
        .collect();
    lines.push(format!("    {}", s.paint(DIM, &spaced)));
    s.emit(&lines);
}

pub fn notify(title: &str, body: &str) {
    if cfg!(test) {
        return;
    }
    let clean = |t: &str| t.replace([';', '\x07', '\x1b'], " ");
    print!("\x1b]777;notify;{};{}\x07", clean(title), clean(body));
    let _ = std::io::stdout().flush();
}

pub fn ask(question: &str, hint: &str) -> Option<String> {
    let s = screen();
    let prompt = if s.fancy {
        format!(
            "  {} {} {} ",
            s.paint(STRONG, "◆"),
            s.paint(BOLD, question),
            s.paint(DIM, hint)
        )
    } else {
        format!("{question} [{hint}] ")
    };
    drop(s);
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => {
            println!();
            None
        }
        Ok(_) => {
            let answer = line.trim().to_string();
            record(&format!(" ?  {question} {answer}"));
            Some(answer)
        }
    }
}

pub fn copy(text: &str) {
    if cfg!(test) {
        return;
    }
    print!("\x1b]52;c;{}\x07", base64(text.as_bytes()));
    let _ = std::io::stdout().flush();
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub fn spaced(code: &str) -> String {
    let (a, b) = code.split_at(code.len().min(3));
    format!("{a} {b}")
}

const DIGITS: [[&str; 3]; 10] = [
    ["┏━┓", "┃ ┃", "┗━┛"],
    ["╺┓ ", " ┃ ", "╺┻╸"],
    ["╺━┓", "┏━┛", "┗━╸"],
    ["╺━┓", "╺━┫", "╺━┛"],
    ["╻ ╻", "┗━┫", "  ╹"],
    ["┏━╸", "┗━┓", "╺━┛"],
    ["┏━╸", "┣━┓", "┗━┛"],
    ["╺━┓", "  ┃", "  ╹"],
    ["┏━┓", "┣━┫", "┗━┛"],
    ["┏━┓", "┗━┫", "╺━┛"],
];

fn big_digits(code: &str) -> [String; 3] {
    let mut rows: [String; 3] = Default::default();
    for (i, d) in code.chars().filter_map(|c| c.to_digit(10)).enumerate() {
        for (row, out) in rows.iter_mut().enumerate() {
            if i > 0 {
                out.push_str(if i % 3 == 0 { "   " } else { " " });
            }
            out.push_str(DIGITS[d as usize][row]);
        }
    }
    rows
}

fn char_width(c: char) -> usize {
    let cp = c as u32;
    if cp < 0x20 || (0x0300..=0x036f).contains(&cp) || (0xfe00..=0xfe0f).contains(&cp) {
        return 0;
    }
    let wide = (0x1100..=0x115f).contains(&cp)
        || (0x2e80..=0xa4cf).contains(&cp)
        || (0xac00..=0xd7a3).contains(&cp)
        || (0xf900..=0xfaff).contains(&cp)
        || (0xff00..=0xff60).contains(&cp)
        || (0x1f300..=0x1f6ff).contains(&cp)
        || (0x1f900..=0x1f9ff).contains(&cp);
    if wide {
        2
    } else {
        1
    }
}

fn width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

fn cut(text: &str, room: usize) -> String {
    if width(text) <= room {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 1;
    for c in text.chars() {
        let w = char_width(c);
        if used + w > room {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

fn wrap(text: &str, room: usize) -> Vec<String> {
    let room = room.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut used = 0;
    for word in text.split_whitespace() {
        let w = width(word);
        if used > 0 && used + 1 + w > room {
            lines.push(std::mem::take(&mut line));
            used = 0;
        }
        if w > room {
            for c in word.chars() {
                if used + char_width(c) > room {
                    lines.push(std::mem::take(&mut line));
                    used = 0;
                }
                line.push(c);
                used += char_width(c);
            }
            continue;
        }
        if used > 0 {
            line.push(' ');
            used += 1;
        }
        line.push_str(word);
        used += w;
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_is_the_standard_one_with_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(
            base64("Settings › Pair".as_bytes()),
            "U2V0dGluZ3Mg4oC6IFBhaXI="
        );
    }

    #[test]
    fn the_log_keeps_lines_not_codes() {
        header("9.9.9");
        live("Ожидание проверки журнала…");
        live("Ожидание проверки журнала…");
        big("975310");
        stopped("проверка завершена.");
        let log = recorded().join("\n");
        assert!(log.contains("siboot-pair 9.9.9 log"), "{log}");
        assert_eq!(log.matches("Ожидание проверки журнала").count(), 1, "{log}");
        assert!(!log.contains("975310") && !log.contains("975 310"), "{log}");
        assert!(log.contains("Остановлено: проверка завершена."), "{log}");
    }

    #[test]
    fn big_digits_are_three_rows_of_even_width() {
        let rows = big_digits("481516");
        assert_eq!(rows[0], "╻ ╻ ┏━┓ ╺┓    ┏━╸ ╺┓  ┏━╸");
        assert_eq!(rows[1], "┗━┫ ┣━┫  ┃    ┗━┓  ┃  ┣━┓");
        assert_eq!(rows[2], "  ╹ ┗━┛ ╺┻╸   ╺━┛ ╺┻╸ ┗━┛");
        for (d, glyph) in DIGITS.iter().enumerate() {
            assert!(glyph.iter().all(|row| width(row) == 3), "{d}");
        }
        assert!(rows.iter().all(|row| width(row) == 25));
    }

    #[test]
    fn codes_read_in_two_groups() {
        assert_eq!(spaced("481516"), "481 516");
    }

    #[test]
    fn lines_wrap_and_cut_to_the_screen() {
        assert_eq!(
            wrap("Open Settings › Privacy & Security › Developer Mode", 20),
            ["Open Settings ›", "Privacy & Security ›", "Developer Mode"]
        );
        assert_eq!(cut("Ожидание Настроек", 12), "Ожидание На…");
        assert_eq!(cut("short", 12), "short");
    }
}
