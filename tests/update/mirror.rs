use super::*;

const URL: &str =
    "https://github.com/agogo233/meatshell/releases/download/v0.7.9/meatshell-v0.7.9-windows-x86_64.zip";

#[test]
fn only_mainland_zones_count_as_china() {
    for zone in ["Asia/Shanghai", "asia/shanghai", "Asia/Urumqi", "PRC"] {
        assert!(is_mainland_zone(zone), "{zone}");
    }
    // UTC+8 elsewhere stays on GitHub.
    for zone in ["Asia/Singapore", "Asia/Hong_Kong", "Asia/Taipei", "Australia/Perth", "UTC", ""] {
        assert!(!is_mainland_zone(zone), "{zone}");
    }
}

#[test]
fn mirror_prefixes_are_normalised_and_checked() {
    assert_eq!(normalize(" https://v4.gh-proxy.org ").unwrap(), "https://v4.gh-proxy.org/");
    assert_eq!(normalize("https://m.example/gh/").unwrap(), "https://m.example/gh/");
    for bad in ["", "http://v4.gh-proxy.org/", "ftp://x/", "https://u:p@x/", "https://x/?a=1", "x"] {
        assert!(normalize(bad).is_err(), "{bad}");
    }
}

#[test]
fn built_in_mirrors_are_valid_and_distinct() {
    let defaults: Vec<String> = DEFAULT_MIRRORS.iter().map(|s| s.to_string()).collect();
    for mirror in &defaults {
        assert_eq!(&normalize(mirror).unwrap(), mirror);
    }
    let sources = Sources::with_mirrors(&defaults);
    assert_eq!(sources.prefixes.len(), defaults.len() + 1, "GitHub + every default");
}

#[test]
fn sources_keep_github_first_and_skip_duplicates() {
    let mirrors = vec![
        "https://v4.gh-proxy.org".to_string(),
        "https://v4.gh-proxy.org/".to_string(),
        "http://insecure.example/".to_string(),
    ];
    let sources = Sources::with_mirrors(&mirrors);
    assert_eq!(sources.prefixes, vec![String::new(), "https://v4.gh-proxy.org/".to_string()]);
    assert!(sources.has_mirrors());
    assert!(!Sources::with_mirrors(&[]).has_mirrors());
}

#[test]
fn github_only_is_never_probed() {
    // No network: with no mirrors `ranked` returns GitHub directly.
    assert_eq!(
        Sources::github().ranked(URL, 1),
        vec![("github.com".to_string(), URL.to_string())]
    );
}

#[test]
fn fastest_first_failed_mirrors_dropped_github_kept() {
    let ms = Duration::from_millis;
    let a = "https://a.example/".to_string();
    let b = "https://b.example/".to_string();
    assert_eq!(
        order(vec![(String::new(), Some(ms(900))), (a.clone(), Some(ms(120))), (b.clone(), None)]),
        vec![a.clone(), String::new()]
    );
    // GitHub down too: still tried last.
    assert_eq!(
        order(vec![(String::new(), None), (a.clone(), Some(ms(50))), (b, Some(ms(10)))]),
        vec!["https://b.example/".to_string(), a, String::new()]
    );
}

#[test]
fn a_picked_source_goes_first_without_probing() {
    let mirrors = vec!["https://a.example/".to_string(), "https://b.example/".to_string()];
    let urls = |sources: Sources| -> Vec<String> {
        // Picked sources never probe, so this needs no network.
        sources.ranked(URL, 1).into_iter().map(|(label, _)| label).collect()
    };
    // A mirror: it, the other mirrors in list order, then GitHub.
    let picked = Sources::with_mirrors(&mirrors).prefer(Some("https://b.example"));
    assert!(!picked.will_probe());
    assert_eq!(urls(picked), ["b.example", "a.example", "github.com"]);
    // GitHub: it, then the mirrors as fallbacks.
    let github = Sources::with_mirrors(&mirrors).prefer(Some(""));
    assert_eq!(urls(github), ["github.com", "a.example", "b.example"]);
}

#[test]
fn a_stale_pick_falls_back_to_automatic() {
    let mirrors = vec!["https://a.example/".to_string()];
    for choice in [None, Some("https://removed.example/"), Some("not a url")] {
        let sources = Sources::with_mirrors(&mirrors).prefer(choice);
        assert!(sources.will_probe(), "{choice:?}");
    }
}

#[test]
fn labels_show_hosts() {
    assert_eq!(label(""), "github.com");
    assert_eq!(label("https://v4.gh-proxy.org/"), "v4.gh-proxy.org");
}
