pub const DEFAULT_MAX_LINES: usize = 2000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_lines: usize,
    pub max_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_lines: DEFAULT_MAX_LINES,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tail {
    pub text: String,
    pub lines: usize,
    pub truncated: bool,
}

pub fn tail(text: &str, limits: Limits) -> Tail {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut start = lines.len();
    let mut bytes = 0;
    while start > 0 && lines.len() - start < limits.max_lines {
        let line = lines[start - 1];
        if bytes + line.len() > limits.max_bytes {
            break;
        }
        bytes += line.len();
        start -= 1;
    }
    if start == lines.len() && !lines.is_empty() {
        let last = lines[lines.len() - 1];
        let cut = ceil_char_boundary(last, last.len().saturating_sub(limits.max_bytes));
        return Tail {
            text: last[cut..].to_string(),
            lines: 1,
            truncated: true,
        };
    }
    Tail {
        text: lines[start..].concat(),
        lines: lines.len() - start,
        truncated: start > 0,
    }
}

pub fn ceil_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

pub fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_short_text() {
        let result = tail("a\nb\n", Limits::default());
        assert_eq!(result.text, "a\nb\n");
        assert_eq!(result.lines, 2);
        assert!(!result.truncated);
    }

    #[test]
    fn limits_lines() {
        let limits = Limits {
            max_lines: 2,
            max_bytes: 100,
        };
        let result = tail("a\nb\nc\nd", limits);
        assert_eq!(result.text, "c\nd");
        assert_eq!(result.lines, 2);
        assert!(result.truncated);
    }

    #[test]
    fn limits_bytes() {
        let limits = Limits {
            max_lines: 100,
            max_bytes: 6,
        };
        let result = tail("aaaa\nbb\ncc\n", limits);
        assert_eq!(result.text, "bb\ncc\n");
        assert!(result.truncated);
    }

    #[test]
    fn cuts_oversized_last_line_on_char_boundary() {
        let limits = Limits {
            max_lines: 100,
            max_bytes: 4,
        };
        let result = tail("한글한글", limits);
        assert_eq!(result.text, "글");
        assert!(result.truncated);
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_size(10), "10B");
        assert_eq!(format_size(2048), "2.0KB");
        assert_eq!(format_size(3 * 1024 * 1024), "3.0MB");
    }
}
