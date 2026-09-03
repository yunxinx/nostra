use super::*;

use std::{fs, path::Path};

#[test]
fn fixture_corpus_matches_expected_recognition() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui/math/fixtures");
    let mut count = 0;
    let mut entries = fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("read fixtures {dir:?}: {error}"))
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect::<Vec<_>>();
    entries.sort();
    for path in entries {
        let source = fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!("read {}: {error}", path.display());
        });
        let expected = fs::read_to_string(path.with_extension("expect")).unwrap_or_else(|error| {
            panic!("read expect for {}: {error}", path.display());
        });
        let expected = expected.trim();
        count += 1;
        match expected {
            "formula" => {
                assert!(
                    !scan_math(&source).tokens.is_empty(),
                    "{} should be a formula: {source:?}",
                    path.display()
                );
            }
            "text" => {
                assert!(
                    scan_math(&source).tokens.is_empty(),
                    "{} should stay text: {:?}",
                    path.display(),
                    scan_math(&source).tokens
                );
            }
            "pending" => {
                assert!(
                    scan_math(&source).tokens.is_empty(),
                    "{} pending source must not be a closed formula",
                    path.display()
                );
                assert!(
                    split_pending_math(source.trim_end()).is_some(),
                    "{} should look like pending math: {source:?}",
                    path.display()
                );
            }
            other => panic!("unknown expect {other:?} in {}", path.display()),
        }
    }
    assert!(
        count >= 10,
        "fixture corpus must contain at least 10 samples, found {count}"
    );
}
