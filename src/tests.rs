use crate::outcome::PackedOutcome;
use super::*;
use crate::egt_file::TableFile;
use crate::storage::Location;
use shakmaty::{Chess, Color, Position};
use std::collections::BTreeSet;
use std::path::Path;

fn test_generator(path: impl AsRef<Path>) -> Tablebase {
    let mut generator = Tablebase::new(path).unwrap();
    generator.set_threads(2);
    generator
}

fn table_path(root: &Path, name: &str, extension: &str) -> PathBuf {
    root.join(TableId::parse(name).unwrap().relative_path(extension))
}

fn write_artifact(root: &Path, name: &str, extension: &str, bytes: &[u8]) {
    let path = table_path(root, name, extension);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn aggregate(tables: &[EgtStats]) -> AggregateStats {
    let mut total = AggregateStats::default();
    for stats in tables {
        total.add(stats);
    }
    total
}

fn verify_internal_consistency(selector: &str) {
    let dir = TestDir::new(&format!("verify_internal_consistency_{selector}"));
    let mut tablebase = test_generator(&dir.0);
    tablebase.generate(&["K_K", selector]).unwrap();
    let mut tables = resolve_selectors(&[selector]).unwrap();
    tables.extend(tables.clone().into_iter().map(|id| id.twin().0));
    let names: Vec<_> = tables.iter().map(TableId::name).collect();
    tablebase
        .verify(&names.iter().map(|name| name.as_str()).collect::<Vec<_>>())
        .unwrap();
}

#[test]
fn test_verify_internal_consistency_k_k() {
    verify_internal_consistency("K_K");
}

#[test]
fn test_verify_internal_consistency_kr_k() {
    verify_internal_consistency("KR_K");
}

#[test]
fn test_verify_internal_consistency_kp_k() {
    let dir = TestDir::new("verify_internal_consistency_KP_K");
    let mut tablebase = test_generator(&dir.0);
    let tables = tablebase
        .generate(&["K_K", "KQ_K", "KR_K", "KB_K", "KN_K", "KP_K"])
        .unwrap();
    let pawn_tables: BTreeSet<_> = tables
        .iter()
        .filter(|stats| stats.tablename.contains('P'))
        .map(|stats| stats.tablename.as_str())
        .collect();
    let expected = resolve_selectors(&["KP_K", "K_KP"]).unwrap();
    let names: Vec<_> = expected.iter().map(TableId::name).collect();
    assert_eq!(pawn_tables, names.iter().map(|name| name.as_str()).collect());
    assert_eq!(pawn_tables.len(), 8);
    for id in &expected {
        let file = TableFile::open(&Location::parse(&dir.0).unwrap(), id).unwrap();
        assert_eq!(file.egt().id(), *id);
    }
    tablebase.verify(&["KP_K", "K_KP"]).unwrap();

    // Exact verification must not expand an identity to its material family.
    for id in expected
        .iter()
        .filter(|id| !["KPa_K", "K_KPa"].contains(&id.name().as_str()))
    {
        std::fs::remove_file(dir.0.join(id.relative_path("ggegt"))).unwrap();
    }
    tablebase.verify(&["KPa_K"]).unwrap();
    assert!(matches!(
        tablebase.verify(&["KP_K"]),
        Err(EgtError::FileNotFound(_))
    ));
}

#[test]
fn default_and_zero_threads_generate_and_verify() {
    for threads in [None, Some(0)] {
        let dir = TestDir::new(&format!("default_threads_{threads:?}"));
        let mut tablebase = Tablebase::new(&dir.0).unwrap();
        if let Some(threads) = threads {
            tablebase.set_threads(threads);
        }
        let tables = tablebase.generate(&["K_K"]).unwrap();
        assert_eq!(tables.len(), 1);
        tablebase.verify(&["K_K"]).unwrap();
    }
}

#[test]
fn bounded_cache_size_supports_probing_generated_and_verified_tables() {
    let dir = TestDir::new("bounded_cache_size");
    // Fit both queen-table frames, but force eviction when K_K is also probed.
    let cache_bytes = 2 * Egt::new(&crate::TableId::parse("KQ_K").unwrap()).unwrap().index_range() * 2;
    let mut tablebase = test_generator(&dir.0);
    let tables = tablebase.generate(&["K_K", "KQ_K"]).unwrap();
    assert_eq!(tables.len(), 3);
    tablebase.verify(&["KQ_K", "K_KQ"]).unwrap();
    tablebase.set_cache_size(cache_bytes).unwrap();

    let egt = Egt::new(&crate::TableId::parse("KQ_K").unwrap()).unwrap();
    let position = (0..egt.index_range())
        .find_map(|index| egt.position_from_index(index, Color::White))
        .unwrap();
    tablebase.probe(&position).unwrap();
}

#[test]
fn probe_with_moves_matches_scalar_probes() {
    use shakmaty::{CastlingMode, fen::Fen};

    let dir = TestDir::new("probe_with_moves");
    let mut prober = test_generator(&dir.0);
    prober
        .generate(&["K_K", "KQ_K", "KR_K", "KB_K", "KN_K", "KP_K"])
        .unwrap();
    prober.set_cache_size(0).unwrap();
    for fen in [
        "7k/8/8/8/8/8/P7/K7 w - - 0 1",
        "7k/P7/2K5/8/8/8/8/8 w - - 0 1",
        "8/8/8/8/8/1k6/P7/7K b - - 0 1",
        "7k/6Q1/5K2/8/8/8/8/8 b - - 0 1",
        "7k/5K2/6Q1/8/8/8/8/8 b - - 0 1",
    ] {
        let position: Chess = fen
            .parse::<Fen>()
            .unwrap()
            .into_position(CastlingMode::Standard)
            .unwrap();
        let result = prober.probe_with_moves(&position).unwrap();
        assert_eq!(result.outcome, prober.probe(&position).unwrap(), "{fen}");
        let expected: Vec<_> = position
            .legal_moves()
            .into_iter()
            .map(|m| {
                let mut successor = position.clone();
                successor.play_unchecked(m);
                (m, prober.probe(&successor).unwrap())
            })
            .collect();
        assert_eq!(result.moves, expected, "{fen}");
    }
}

#[test]
fn probing_rejects_castling_rights_before_table_lookup() {
    use shakmaty::{CastlingMode, fen::Fen};

    let dir = TestDir::new("probing_castling_rights");
    let prober = Tablebase::new(&dir.0).unwrap();
    for (board, rights) in [
        ("4k3/8/8/8/8/8/8/4K2R", "K"),
        ("4k3/8/8/8/8/8/8/R3K3", "Q"),
        ("4k2r/8/8/8/8/8/8/4K3", "k"),
        ("r3k3/8/8/8/8/8/8/4K3", "q"),
        ("4k3/8/8/8/8/8/8/4KB1R", "K"),
    ] {
        for turn in ["w", "b"] {
            let fen = format!("{board} {turn} {rights} - 0 1");
            let position = fen
                .parse::<Fen>()
                .unwrap()
                .into_position(CastlingMode::Standard)
                .unwrap();
            assert!(
                matches!(
                    prober.probe(&position),
                    Err(EgtError::UnsupportedPosition(_))
                ),
                "{fen}"
            );
        }
    }
}

#[test]
fn zero_cache_size_supports_probing_generated_and_verified_tables() {
    let dir = TestDir::new("zero_cache_size");
    let mut tablebase = test_generator(&dir.0);
    tablebase.generate(&["K_K"]).unwrap();
    tablebase.verify(&["K_K"]).unwrap();
    tablebase.set_cache_size(0).unwrap();
    let egt = Egt::new(&crate::TableId::parse("K_K").unwrap()).unwrap();
    let position = (0..egt.index_range())
        .find_map(|index| egt.position_from_index(index, Color::White))
        .unwrap();
    assert_eq!(tablebase.probe(&position).unwrap(), DtcOutcome::Draw);
}

#[test]
fn generation_invalidates_cached_probe_data() {
    let dir = TestDir::new("generation_invalidates_cache");
    let win = DtcOutcome::Win(ConversionType::Checkmate, 3);
    crate::egt_file::writer::write_fixture(&dir.0, "K_K", |_| {
        PackedOutcome::new_win(ConversionType::Checkmate, 3)
    });
    let mut tablebase = test_generator(&dir.0);
    let position = position_for("K_K");
    assert_eq!(tablebase.probe(&position).unwrap(), win);

    tablebase.generate(&["K_K"]).unwrap();
    assert_eq!(tablebase.probe(&position).unwrap(), DtcOutcome::Draw);
}

#[test]
fn generator_publishes_finalized_stats_with_shadowed_outputs() {
    use sha2::{Digest, Sha256};

    let output = TestDir::new("generator_stats_output");
    let input = TestDir::new("generator_stats_input");
    Location::parse(&input.0).unwrap().ensure_format().unwrap();
    // Output identities in the input overlay must not be read during generation.
    for name in ["KQ_K", "K_KQ"] {
        write_artifact(&input.0, name, "ggegt", &[0xff; 64]);
    }
    let mut generator = Tablebase::new(&output.0).unwrap();
    generator.set_input_path(&input.0).unwrap();
    generator.set_threads(2);
    generator.generate(&["K_K"]).unwrap();
    let tables = generator.generate(&["KQ_K"]).unwrap();
    assert_eq!(tables.len(), 2);
    assert!(table_path(&output.0, "K_K", "ggegt").exists());
    for stats in &tables {
        let data = std::fs::read(table_path(&output.0, &stats.tablename, "ggegt")).unwrap();
        assert_eq!(stats.totals.bytes, data.len() as u64);
        assert_eq!(stats.sha256, format!("{:x}", Sha256::digest(&data)));
        let json = std::fs::read(table_path(&output.0, &stats.tablename, "json")).unwrap();
        assert_eq!(serde_json::from_slice::<EgtStats>(&json).unwrap(), *stats);
    }
    for name in ["KQ_K", "K_KQ"] {
        let error = generator.probe(&position_for(name)).unwrap_err();
        assert!(!matches!(error, EgtError::FileNotFound(_)));
    }
    assert_eq!(
        generator.stats(&["KQ_K", "K_KQ"]).unwrap(),
        aggregate(&tables)
    );
}

#[test]
fn read_only_output_is_rejected_before_generation() {
    let mut generator = Tablebase::new("https://example.com/tables/").unwrap();
    for threads in [1, 4] {
        generator.set_threads(threads);
        assert!(matches!(generator.generate(&["K_K"]),
            Err(EgtError::InvalidStorageLocation(message)) if message.contains("read-only")));
    }
}

#[test]
fn in_memory_prefix_reuses_backend_for_dependencies_and_verification() {
    let location = Location::parse("memory:///nested/tables").unwrap();
    let mut tablebase = Tablebase::from_location(location.clone());
    tablebase.set_threads(2);
    tablebase.generate(&["K_K"]).unwrap();
    let tables = tablebase.generate(&["KQ_K"]).unwrap();
    assert_eq!(tables.len(), 2);

    tablebase.verify(&["KQ_K", "K_KQ"]).unwrap();
    assert_eq!(
        tablebase.probe(&position_for("K_K")).unwrap(),
        DtcOutcome::Draw
    );
    assert!(object_exists(&location, "2/1v1/op0/0pawns/K_K.ggegt"));
    assert_eq!(
        tablebase.stats(&["KQ_K", "K_KQ"]).unwrap(),
        aggregate(&tables)
    );
}

#[test]
fn input_lookup_falls_back_only_for_missing_files() {
    let output = TestDir::new("input_fallback_output");
    let input = TestDir::new("input_fallback_input");
    test_generator(&output.0).generate(&["K_K"]).unwrap();
    Location::parse(&input.0).unwrap().ensure_format().unwrap();

    let probe = || {
        let mut prober = Tablebase::new(&output.0).unwrap();
        prober.set_input_path(&input.0).unwrap();
        prober.probe(&position_for("K_K"))
    };
    assert!(probe().is_ok());
    write_artifact(&input.0, "K_K", "ggegt", &[0xff; 64]);
    assert!(
        matches!(probe(), Err(EgtError::InvalidData { location, .. })
        if location.contains("input_fallback_input") && location.ends_with("/K_K.ggegt"))
    );
    std::fs::remove_file(table_path(&input.0, "K_K", "ggegt")).unwrap();
    assert!(probe().is_ok());
}

#[test]
fn changing_input_path_invalidates_warmed_lookup_precedence() {
    let output = TestDir::new("changed_input_output");
    let input = TestDir::new("changed_input_input");
    test_generator(&output.0).generate(&["K_K"]).unwrap();
    Location::parse(&input.0).unwrap().ensure_format().unwrap();
    write_artifact(&input.0, "K_K", "ggegt", &[0xff; 64]);
    let egt = Egt::new(&crate::TableId::parse("K_K").unwrap()).unwrap();
    let position = (0..egt.index_range())
        .find_map(|index| egt.position_from_index(index, Color::White))
        .unwrap();
    let mut prober = Tablebase::new(&output.0).unwrap();
    assert_eq!(prober.probe(&position).unwrap(), DtcOutcome::Draw);
    prober.set_input_path(&input.0).unwrap();
    assert!(prober.probe(&position).is_err());
}

#[test]
fn generation_replaces_placeholders_and_invalidates_hierarchy_summaries() {
    let location = Location::parse("memory:///stats-publication").unwrap();
    let mut tablebase = Tablebase::from_location(location.clone());
    tablebase.set_threads(2);
    let missing = tablebase.cache_stats(&["2"]).unwrap();
    assert_eq!(missing.selected_tables, 1);
    assert_eq!(missing.available_tables, 0);
    assert_eq!(missing.totals, None);
    assert_eq!(tablebase.stats(&["2"]).unwrap(), missing);
    let tables = tablebase.generate(&["K_K"]).unwrap();
    for prefix in ["2", "2/1v1", "2/1v1/op0", "2/1v1/op0/0pawns"] {
        assert!(!object_exists(&location, &format!("{prefix}/stats.json")));
        assert_eq!(tablebase.stats(&[prefix]).unwrap(), aggregate(&tables));
    }
    assert_eq!(tablebase.cache_stats(&["2"]).unwrap(), aggregate(&tables));
    assert_eq!(tablebase.stats(&["2"]).unwrap(), aggregate(&tables));
    assert_eq!(tablebase.stats(&["K_K"]).unwrap(), aggregate(&tables));
}

#[test]
fn statistics_describe_one_root_independently_of_probing_overlays() {
    let location = Location::parse("memory:///stats-overlay").unwrap();
    location.ensure_format().unwrap();
    let mut tablebase = Tablebase::from_location(location.clone());
    tablebase.set_input_path("memory:///stats-input").unwrap();
    let missing = tablebase.stats(&["2"]).unwrap();
    assert_eq!((missing.selected_tables, missing.available_tables), (1, 0));
    assert!(!object_exists(&location, "2/1v1/op0/0pawns/K_K.json"));
    assert_eq!(tablebase.cache_stats(&["2"]).unwrap(), missing);
}

fn object_exists(location: &Location, path: &str) -> bool {
    use object_store::ObjectStoreExt;
    crate::storage::block_on(location.store.head(&location.key(path))).is_ok()
}

#[test]
fn selector_union_is_sorted_deduplicated_and_preserves_orientation() {
    let tables = resolve_selectors(&["KP_K", "K_KP", "KP_K"]).unwrap();
    assert_eq!(tables, resolve_selectors(&["KP_K", "K_KP"]).unwrap());
    assert_eq!(tables.len(), 8);
    assert!(tables.windows(2).all(|pair| pair[0] < pair[1]));
    let exact = resolve_selectors(&["KQ_K", "KQ_K", "KR_K"]).unwrap();
    assert_eq!(
        exact.iter().map(ToString::to_string).collect::<Vec<_>>(),
        ["KQ_K", "KR_K"]
    );
    assert!(!exact.contains(&TableId::parse("K_KQ").unwrap()));
}

fn position_for(name: &str) -> Chess {
    let egt = Egt::new(&crate::TableId::parse(name).unwrap()).unwrap();
    (0..egt.index_range())
        .find_map(|index| egt.position_from_index(index, Color::White))
        .unwrap()
}
