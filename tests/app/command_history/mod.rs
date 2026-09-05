use super::history_view_rows;

#[test]
fn lists_and_filters_commands_newest_last() {
    let history = vec![
        "git status".to_string(),
        "cargo check".to_string(),
        "git log".to_string(),
    ];

    let all: Vec<String> = history_view_rows(&history, "")
        .into_iter()
        .map(Into::into)
        .collect();
    assert_eq!(all, ["git status", "cargo check", "git log"]);

    let filtered: Vec<String> = history_view_rows(&history, "GIT")
        .into_iter()
        .map(Into::into)
        .collect();
    assert_eq!(filtered, ["git status", "git log"]);
}

#[test]
fn fuzzy_subsequence_matches_and_ranks_best_last() {
    let history = vec![
        "git commit -m".to_string(),
        "kubectl get pods".to_string(),
        "grep foo".to_string(),
    ];
    // "kbl" is a subsequence of "kubectl" but not of the others.
    let rows: Vec<String> = history_view_rows(&history, "kbl")
        .into_iter()
        .map(Into::into)
        .collect();
    assert_eq!(rows, ["kubectl get pods"]);
}

#[test]
fn fuzzy_prefers_substring_over_subsequence() {
    let history = vec![
        "git checkout".to_string(),      // subsequence: c…o across "checkout"
        "docker compose up".to_string(), // substring: "co" in "compose"
    ];
    let rows: Vec<String> = history_view_rows(&history, "co")
        .into_iter()
        .map(Into::into)
        .collect();
    // Best match (substring) is last (dropdown default selection).
    assert_eq!(rows.last().map(String::as_str), Some("docker compose up"));
}

#[test]
fn fuzzy_word_start_boosts_rank() {
    let history = vec![
        "echo log".to_string(), // "log" mid-word
        "git log".to_string(),  // "log" at word start
    ];
    let rows: Vec<String> = history_view_rows(&history, "log")
        .into_iter()
        .map(Into::into)
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows.last().map(String::as_str), Some("git log"));
}
