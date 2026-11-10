const MATCH: i64 = 16;
const BOUNDARY: i64 = 24;
const CONSECUTIVE: i64 = 20;
const GAP: i64 = 2;
const MAX_CANDIDATE: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyMatch {
    pub index: usize,
    pub score: i64,
    pub positions: Vec<usize>,
}

fn is_boundary(chars: &[(usize, char)], ix: usize) -> bool {
    if ix == 0 {
        return true;
    }
    let previous = chars[ix - 1].1;
    let current = chars[ix].1;
    matches!(previous, ' ' | '/' | '-' | '_' | ':' | '.' | '·' | '(')
        || (previous.is_lowercase() && current.is_uppercase())
}

pub fn fuzzy_match(query: &str, candidate: &str) -> Option<(i64, Vec<usize>)> {
    let needle: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    if needle.is_empty() {
        return Some((0, Vec::new()));
    }
    let chars: Vec<(usize, char)> = candidate.char_indices().take(MAX_CANDIDATE).collect();
    let lower: Vec<char> = chars
        .iter()
        .map(|(_, c)| c.to_lowercase().next().unwrap_or(*c))
        .collect();
    let n = chars.len();
    let m = needle.len();
    if m > n {
        return None;
    }
    let none = i64::MIN / 4;
    let mut score = vec![vec![none; n]; m];
    let mut back = vec![vec![usize::MAX; n]; m];
    for j in 0..n {
        if lower[j] == needle[0] {
            let bonus = if is_boundary(&chars, j) { BOUNDARY } else { 0 };
            score[0][j] = MATCH + bonus - j as i64;
        }
    }
    for i in 1..m {
        for j in i..n {
            if lower[j] != needle[i] {
                continue;
            }
            let bonus = if is_boundary(&chars, j) { BOUNDARY } else { 0 };
            let mut best = none;
            let mut from = usize::MAX;
            for (k, &previous) in score[i - 1].iter().enumerate().take(j).skip(i - 1) {
                if previous == none {
                    continue;
                }
                let step = if k + 1 == j {
                    CONSECUTIVE
                } else {
                    -GAP * (j - k - 1) as i64
                };
                let total = previous + step;
                if total > best {
                    best = total;
                    from = k;
                }
            }
            if from != usize::MAX {
                score[i][j] = best + MATCH + bonus;
                back[i][j] = from;
            }
        }
    }
    let (end, best) = (0..n)
        .map(|j| (j, score[m - 1][j]))
        .filter(|(_, s)| *s != none)
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))?;
    let mut positions = vec![0; m];
    let mut j = end;
    for i in (0..m).rev() {
        positions[i] = chars[j].0;
        if i > 0 {
            j = back[i][j];
        }
    }
    Some((best, positions))
}

pub fn filter<'a>(query: &str, candidates: impl IntoIterator<Item = &'a str>) -> Vec<FuzzyMatch> {
    let mut matches: Vec<FuzzyMatch> = candidates
        .into_iter()
        .enumerate()
        .filter_map(|(index, candidate)| {
            fuzzy_match(query, candidate).map(|(score, positions)| FuzzyMatch {
                index,
                score,
                positions,
            })
        })
        .collect();
    if !query.trim().is_empty() {
        matches.sort_by(|a, b| b.score.cmp(&a.score).then(a.index.cmp(&b.index)));
    }
    matches
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_subsequences_case_insensitively() {
        let (_, positions) = fuzzy_match("nse", "workspace: New Session").unwrap();
        assert_eq!(positions, vec![11, 15, 16]);
        assert!(fuzzy_match("xyz", "New Session").is_none());
        assert_eq!(fuzzy_match("", "anything"), Some((0, Vec::new())));
    }

    #[test]
    fn prefers_word_starts_and_runs() {
        let names = [
            "pane: split right",
            "workspace: new terminal",
            "workspace: toggle right dock",
        ];
        let matches = filter("tog", names);
        assert_eq!(matches[0].index, 2);
        let matches = filter("split", names);
        assert_eq!(matches[0].index, 0);
        assert!(matches[0].score > matches[1].score);
        let matches = filter("nt", names);
        assert_eq!(matches[0].index, 1);
    }

    #[test]
    fn keeps_order_without_a_query() {
        let matches = filter(" ", ["b", "a"]);
        assert_eq!(matches.iter().map(|m| m.index).collect::<Vec<_>>(), [0, 1]);
    }

    #[test]
    fn reports_byte_positions_for_multibyte_text() {
        let (_, positions) = fuzzy_match("세", "안녕하세요 Loom").unwrap();
        assert_eq!(positions, vec![9]);
    }
}
