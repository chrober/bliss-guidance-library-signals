use bliss_guidance_library_signals::{
    accumulate_catalogue, candidate_groups, ArtistIdentity, ListeningSource,
};
use bliss_guidance_library_signals::{HistoryAccumulator, TrackHistory};
use rusqlite::Connection;

#[test]
fn exact_credit_does_not_infer_artist_for_ambiguous_track_mappings() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("ATTACH ':memory:' AS history; ATTACH ':memory:' AS identities; ATTACH ':memory:' AS bliss;
        CREATE TABLE tracks(id INTEGER PRIMARY KEY, urlmd5 TEXT, album INTEGER, primary_artist INTEGER);
        CREATE TABLE contributor_track(track INTEGER, contributor INTEGER, role INTEGER);
        CREATE TABLE history.tracks_persistent(urlmd5 TEXT PRIMARY KEY, playCount INTEGER, lastPlayed INTEGER);
        CREATE TABLE identities.track_identity(database_file TEXT PRIMARY KEY, lms_track_id INTEGER);
        CREATE TABLE bliss.TracksV2(File TEXT PRIMARY KEY, Artist TEXT);
        INSERT INTO tracks VALUES(1,'a',10,1),(2,'b',10,1);
        INSERT INTO history.tracks_persistent VALUES('a',3,100),('b',5,200);
        INSERT INTO identities.track_identity VALUES('a1',1),('a2',1),('b1',2);
        INSERT INTO bliss.TracksV2 VALUES('a1','Artist A'),('a2','Artist B'),('b1','Artist A');").unwrap();
    let urls = vec!["a".to_owned(), "b".to_owned()];
    let totals = bliss_guidance_library_signals::accumulate_selected_catalogue(
        &db,
        ListeningSource::Lyrion,
        ArtistIdentity::ExactBlissCredit,
        &urls,
    )
    .unwrap();
    let groups = totals.exact_candidate_groups(&urls);
    assert!(
        groups["a"].artists.is_empty(),
        "conflicting file credits cannot identify one artist"
    );
    assert_eq!(
        totals.artist(groups["b"].artists[0]).unwrap().playcount_sum,
        5,
        "ambiguous track history must not be attributed to either credit"
    );
    assert_eq!(
        totals.album(10).unwrap().playcount_sum,
        8,
        "album history is unaffected"
    );
    db.execute(
        "UPDATE bliss.TracksV2 SET Artist='artist a' WHERE File='a2'",
        [],
    )
    .unwrap();
    let matching = bliss_guidance_library_signals::accumulate_selected_catalogue(
        &db,
        ListeningSource::Lyrion,
        ArtistIdentity::ExactBlissCredit,
        &urls,
    )
    .unwrap();
    let groups = matching.exact_candidate_groups(&urls);
    assert_eq!(
        matching
            .artist(groups["a"].artists[0])
            .unwrap()
            .playcount_sum,
        8,
        "duplicate files with the same normalized credit count only once"
    );
}

#[test]
fn selected_exact_credit_matches_full_aggregation_for_apc_and_unicode() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("ATTACH ':memory:' AS history; ATTACH ':memory:' AS identities; ATTACH ':memory:' AS bliss;
        CREATE TABLE tracks(id INTEGER PRIMARY KEY, urlmd5 TEXT, album INTEGER, primary_artist INTEGER);
        CREATE TABLE contributor_track(track INTEGER, contributor INTEGER, role INTEGER);
        CREATE TABLE history.tracks_persistent(urlmd5 TEXT PRIMARY KEY, playCount INTEGER, lastPlayed INTEGER);
        CREATE TABLE history.alternativeplaycount(urlmd5 TEXT PRIMARY KEY, playCount INTEGER, lastPlayed INTEGER);
        CREATE TABLE identities.track_identity(database_file TEXT PRIMARY KEY, lms_track_id INTEGER);
        CREATE TABLE bliss.TracksV2(File TEXT PRIMARY KEY, Artist TEXT);
        INSERT INTO tracks VALUES(1,'a',10,1),(2,'b',11,2),(3,'c',12,3);
        INSERT INTO history.alternativeplaycount VALUES('a',3,100),('b',5,200),('c',20,300);
        INSERT INTO identities.track_identity VALUES('a1',1),('b1',2),('c1',3);
        INSERT INTO bliss.TracksV2 VALUES('a1','Artist A'),('b1','Ａｒｔｉｓｔ Ａ'),('c1','Artist C');").unwrap();
    let all = accumulate_catalogue(
        &db,
        ListeningSource::AlternativePlayCount,
        ArtistIdentity::ExactBlissCredit,
    )
    .unwrap();
    let urls = vec!["a".to_owned()];
    let selected = bliss_guidance_library_signals::accumulate_selected_catalogue(
        &db,
        ListeningSource::AlternativePlayCount,
        ArtistIdentity::ExactBlissCredit,
        &urls,
    )
    .unwrap();
    let groups = selected.exact_candidate_groups(&urls);
    let artist = groups["a"].artists[0];
    assert_eq!(selected.artist(artist), all.artist(artist));
    assert_eq!(selected.artist(artist).unwrap().playcount_sum, 8);
    assert_eq!(selected.artist(artist).unwrap().last_played, Some(200));
    assert_eq!(selected.album(10), all.album(10));
    assert!(selected.album(12).is_none());
}

#[test]
fn exact_credit_supports_more_selected_albums_than_sqlite_parameter_limit() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("ATTACH ':memory:' AS history; ATTACH ':memory:' AS identities; ATTACH ':memory:' AS bliss;
        CREATE TABLE tracks(id INTEGER PRIMARY KEY, urlmd5 TEXT, album INTEGER, primary_artist INTEGER);
        CREATE INDEX tracks_url_idx ON tracks(urlmd5);
        CREATE TABLE contributor_track(track INTEGER, contributor INTEGER, role INTEGER);
        CREATE TABLE history.tracks_persistent(urlmd5 TEXT PRIMARY KEY, playCount INTEGER, lastPlayed INTEGER);
        CREATE TABLE identities.track_identity(lms_track_id INTEGER, database_file TEXT);
        CREATE TABLE bliss.TracksV2(File TEXT PRIMARY KEY, Artist TEXT);
        WITH RECURSIVE ids(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM ids WHERE n<1001)
        INSERT INTO tracks SELECT n, printf('track-%d', n), n, 1 FROM ids;
        INSERT INTO tracks VALUES(1002, 'unselected', 1002, 1);
        INSERT INTO history.tracks_persistent VALUES('track-1001', 7, 10);
        INSERT INTO identities.track_identity VALUES(1001, 'test.flac');
        INSERT INTO bliss.TracksV2 VALUES('test.flac', 'Artist');").unwrap();
    // Simulate a valid SQLite build with a smaller host-variable limit.
    unsafe {
        rusqlite::ffi::sqlite3_limit(
            db.handle(),
            rusqlite::ffi::SQLITE_LIMIT_VARIABLE_NUMBER,
            1000,
        )
    };
    let urls = (1..=1001).map(|n| format!("track-{n}")).collect::<Vec<_>>();
    let totals = bliss_guidance_library_signals::accumulate_selected_catalogue(
        &db,
        ListeningSource::Lyrion,
        ArtistIdentity::ExactBlissCredit,
        &urls,
    )
    .expect("a large candidate inventory must not exceed SQLite's host-variable limit");
    assert_eq!(totals.album(1001).unwrap().playcount_sum, 7);
    assert!(
        totals.album(1002).is_none(),
        "the scan does not retain unselected albums"
    );
}

#[test]
fn exact_credit_uses_bliss_credit_and_not_lyrion_primary_artist() {
    use sha2::{Digest, Sha256};
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};
    let dir = std::env::temp_dir().join(format!(
        "exact-credit-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let catalogue = dir.join("catalogue.db");
    let db = Connection::open(&catalogue).unwrap();
    db.execute_batch("CREATE TABLE tracks(id INTEGER PRIMARY KEY,urlmd5 TEXT,album INTEGER,primary_artist INTEGER); CREATE TABLE contributors(id INTEGER PRIMARY KEY,musicbrainz_id TEXT); CREATE TABLE contributor_track(track INTEGER,contributor INTEGER,role INTEGER); INSERT INTO tracks VALUES(1,'a',10,1),(2,'b',10,2),(3,'c',11,1),(4,'d',10,4);").unwrap();
    drop(db);
    let history = dir.join("history.db");
    let db = Connection::open(&history).unwrap();
    db.execute_batch("CREATE TABLE tracks_persistent(urlmd5 TEXT PRIMARY KEY,playCount INTEGER,lastPlayed INTEGER,added INTEGER); INSERT INTO tracks_persistent VALUES('a',2,100,0),('b',3,200,0),('c',11,300,0),('d',5,400,0);").unwrap();
    drop(db);
    let identities = dir.join("identities.db");
    let db = Connection::open(&identities).unwrap();
    db.execute_batch("CREATE TABLE track_identity(lms_track_id INTEGER, database_file TEXT); INSERT INTO track_identity VALUES(1,'a.flac'),(2,'b.flac'),(3,'c.flac');").unwrap();
    drop(db);
    let bliss = dir.join("bliss.db");
    let db = Connection::open(&bliss).unwrap();
    db.execute_batch("CREATE TABLE TracksV2(File TEXT PRIMARY KEY,Artist TEXT); INSERT INTO TracksV2 VALUES('a.flac','An Artist & Guest'),('b.flac','Ａｎ artist & guest'),('c.flac','An Artist');").unwrap();
    drop(db);
    let artifact = dir.join("candidates.json");
    let bytes = serde_json::to_vec(&serde_json::json!({"schema_version":1,"schema_identity":"eligible-candidate-identities-v1","candidates":[{"candidate_id":"one","lms_urlmd5":"a"}]})).unwrap();
    std::fs::write(&artifact, &bytes).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_bliss-guidance-library-signals"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut exchange = |request: serde_json::Value| -> serde_json::Value {
        writeln!(input, "{request}").unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };
    let invalid = exchange(
        serde_json::json!({"type":"prepare","spi_version":2,"job_id":"invalid",
        "options":{"aggregate_identity_mode":"exact_bliss_credit","as_of_unix_seconds":1000},
        "artifacts":[{"kind":"eligible-candidate-identities-v1","path":artifact,"sha256":format!("{:x}",Sha256::digest(&bytes))},
            {"kind":"lyrion-track-identities-v1","path":identities,"sha256":"0".repeat(64)}],
        "resources":[{"kind":"lms-persist-sqlite-v1","path":history,"access":"read_only"},
            {"kind":"lms-catalogue-sqlite-v1","path":catalogue,"access":"read_only"},
            {"kind":"lyrion-track-identities-sqlite-v1","path":identities,"access":"read_only"},
            {"kind":"bliss-analysis-sqlite-v1","path":bliss,"access":"read_only"}],"anchors":[]}),
    );
    assert_eq!(
        invalid["type"], "error",
        "identity snapshot must be hash verified"
    );
    let identity_sha = format!("{:x}", Sha256::digest(std::fs::read(&identities).unwrap()));
    let prepared = exchange(
        serde_json::json!({"type":"prepare","spi_version":2,"job_id":"fixture",
        "options":{"aggregate_identity_mode":"exact_bliss_credit","as_of_unix_seconds":1000},
        "artifacts":[{"kind":"eligible-candidate-identities-v1","path":artifact,"sha256":format!("{:x}",Sha256::digest(&bytes))},
            {"kind":"lyrion-track-identities-v1","path":identities,"sha256":identity_sha}],
        "resources":[{"kind":"lms-persist-sqlite-v1","path":history,"access":"read_only"},
            {"kind":"lms-catalogue-sqlite-v1","path":catalogue,"access":"read_only"},
            {"kind":"lyrion-track-identities-sqlite-v1","path":identities,"access":"read_only"},
            {"kind":"bliss-analysis-sqlite-v1","path":bliss,"access":"read_only"}],"anchors":[]}),
    );
    assert_eq!(prepared["type"], "prepared", "{prepared}");
    let scored = exchange(
        serde_json::json!({"type":"score","spi_version":2,"request_id":"one",
        "context":{"scope":"global","left_anchor_id":null,"right_anchor_id":null,"context_track_ids":[]},
        "candidates":[{"candidate_id":"one","lms_urlmd5":"a"}]}),
    );
    let artist = scored["signals"]
        .as_array()
        .unwrap()
        .iter()
        .find(|signal| signal["channel"] == "artist_playcount")
        .unwrap();
    assert_eq!(
        artist["observation"]["playcount_sum"], 5,
        "the two matching Bliss credits are grouped even with different Lyrion primary artists"
    );
    let album = scored["signals"]
        .as_array()
        .unwrap()
        .iter()
        .find(|signal| signal["channel"] == "album_playcount")
        .unwrap();
    assert_eq!(
        album["observation"]["playcount_sum"], 10,
        "album history includes tracks absent from the Bliss identity snapshot"
    );
    let closed = exchange(serde_json::json!({"type":"close","spi_version":2}));
    assert_eq!(closed["type"], "closed");
    drop(input);
    drop(output);
    assert!(child.wait().unwrap().success());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn native_jsonl_session_returns_candidate_scoped_aggregate_observations() {
    use sha2::{Digest, Sha256};
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};
    let dir = std::env::temp_dir().join(format!(
        "library signals #{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&dir).unwrap();
    let catalogue = dir.join("catalogue.sqlite");
    let history = dir.join("history #1.sqlite");
    let db = Connection::open(&catalogue).unwrap();
    db.execute_batch("CREATE TABLE tracks(id INTEGER PRIMARY KEY, urlmd5 TEXT, album INTEGER, primary_artist INTEGER); CREATE TABLE contributors(id INTEGER PRIMARY KEY, musicbrainz_id TEXT); CREATE TABLE contributor_track(track INTEGER, contributor INTEGER, role INTEGER); INSERT INTO tracks VALUES (1,'a',10,1),(2,'b',10,1); INSERT INTO contributor_track VALUES (1,1,1),(2,1,1);").unwrap();
    drop(db);
    let db = Connection::open(&history).unwrap();
    db.execute_batch("CREATE TABLE tracks_persistent(urlmd5 TEXT PRIMARY KEY, playCount INTEGER, lastPlayed INTEGER, added INTEGER); INSERT INTO tracks_persistent VALUES ('a',2,100,1),('b',3,200,1);").unwrap();
    drop(db);
    let artifact = dir.join("identities.json");
    let json = serde_json::json!({"schema_version":1,"schema_identity":"eligible-candidate-identities-v1","candidates":[{"candidate_id":"a-id","lms_urlmd5":"a"}]});
    let bytes = serde_json::to_vec(&json).unwrap();
    std::fs::write(&artifact, &bytes).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_bliss-guidance-library-signals"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut exchange = |request: serde_json::Value| -> serde_json::Value {
        writeln!(input, "{request}").unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    };
    let manifest = exchange(serde_json::json!({"type":"describe","spi_version":2}));
    assert_eq!(manifest["type"], "manifest");
    let prepared = exchange(
        serde_json::json!({"type":"prepare","spi_version":2,"job_id":"fixture","options":{"aggregate_identity_mode":"shared_artist","as_of_unix_seconds":1000},"artifacts":[{"kind":"eligible-candidate-identities-v1","path":artifact,"sha256":format!("{:x}",Sha256::digest(&bytes))}],"resources":[{"kind":"lms-persist-sqlite-v1","path":history,"access":"read_only"},{"kind":"lms-catalogue-sqlite-v1","path":catalogue,"access":"read_only"}],"anchors":[]}),
    );
    assert_eq!(prepared["type"], "prepared", "{prepared}");
    let scored = exchange(
        serde_json::json!({"type":"score","spi_version":2,"request_id":"one","context":{"scope":"global","left_anchor_id":null,"right_anchor_id":null,"context_track_ids":[]},"candidates":[{"candidate_id":"a-id","lms_urlmd5":"a"}]}),
    );
    assert_eq!(scored["type"], "scores", "{scored}");
    let signals = scored["signals"].as_array().unwrap();
    let album = signals
        .iter()
        .find(|signal| signal["channel"] == "album_playcount")
        .unwrap();
    assert_eq!(album["observation"]["playcount_sum"], 5);
    assert_eq!(
        album["observation"]["guidance_family_v1"],
        "listening_playcount"
    );
    assert!(signals
        .iter()
        .all(|signal| signal["candidate_id"] == "a-id"));
    let closed = exchange(serde_json::json!({"type":"close","spi_version":2}));
    assert_eq!(closed["type"], "closed");
    drop(input);
    drop(output);
    assert!(child.wait().unwrap().success());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "synthetic 200k-track performance measurement"]
fn benchmark_catalogue_aggregation_on_200k_tracks() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("ATTACH DATABASE ':memory:' AS history;
      CREATE TABLE tracks(id INTEGER PRIMARY KEY, urlmd5 TEXT, album INTEGER, primary_artist INTEGER);
      CREATE TABLE contributors(id INTEGER PRIMARY KEY, musicbrainz_id TEXT);
      CREATE TABLE contributor_track(track INTEGER, contributor INTEGER, role INTEGER);
      CREATE INDEX contributor_track_by_track ON contributor_track(track);
      CREATE INDEX contributor_track_by_contributor ON contributor_track(contributor);
      CREATE INDEX track_by_album ON tracks(album);
      CREATE INDEX track_by_urlmd5 ON tracks(urlmd5);
      CREATE TABLE history.tracks_persistent(urlmd5 TEXT PRIMARY KEY, playCount INTEGER, lastPlayed INTEGER);
      WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<200000)
      INSERT INTO tracks SELECT x, printf('file-%06d', x), (x-1)/12+1, (x-1)/200+1 FROM n;
      INSERT INTO contributor_track SELECT id, primary_artist, 1 FROM tracks;
      INSERT INTO history.tracks_persistent SELECT urlmd5, id%50, id*100 FROM tracks;").unwrap();
    let start = std::time::Instant::now();
    let totals =
        accumulate_catalogue(&db, ListeningSource::Lyrion, ArtistIdentity::SharedArtist).unwrap();
    eprintln!("200k catalogue aggregation elapsed: {:?}", start.elapsed());
    assert_eq!(totals.album(1).unwrap().track_count, 12);
    assert_eq!(totals.artist(1).unwrap().track_count, 200);
    let urls: Vec<String> = (1..=20)
        .map(|index| format!("file-{:06}", index * 10000))
        .collect();
    let started = std::time::Instant::now();
    let selected = bliss_guidance_library_signals::accumulate_selected_catalogue(
        &db,
        ListeningSource::Lyrion,
        ArtistIdentity::SharedArtist,
        &urls,
    )
    .unwrap();
    eprintln!(
        "200k catalogue, 20 candidate aggregation elapsed: {:?}",
        started.elapsed()
    );
    assert_eq!(selected.album(834), totals.album(834));
    db.execute_batch(
        "ATTACH DATABASE ':memory:' AS identities;
      ATTACH DATABASE ':memory:' AS bliss;
      CREATE TABLE identities.track_identity(database_file TEXT PRIMARY KEY,lms_track_id INTEGER);
      CREATE TABLE bliss.TracksV2(File TEXT PRIMARY KEY,Artist TEXT);
      INSERT INTO identities.track_identity SELECT urlmd5,id FROM tracks;
      INSERT INTO bliss.TracksV2 SELECT urlmd5,printf('Artist %d',primary_artist) FROM tracks;",
    )
    .unwrap();
    let started = std::time::Instant::now();
    let exact = bliss_guidance_library_signals::accumulate_selected_catalogue(
        &db,
        ListeningSource::Lyrion,
        ArtistIdentity::ExactBlissCredit,
        &urls,
    )
    .unwrap();
    let exact_elapsed = started.elapsed();
    eprintln!(
        "200k catalogue, exact credit, 20 candidate aggregation elapsed: {:?}",
        exact_elapsed
    );
    if let Ok(limit_ms) = std::env::var("BLISS_EXACT_CREDIT_BENCH_LIMIT_MS") {
        let limit = std::time::Duration::from_millis(limit_ms.parse().unwrap());
        assert!(
            exact_elapsed < limit,
            "exact-credit aggregation exceeded the experimental local performance target"
        );
    }
    assert_eq!(exact.album(834), totals.album(834));
    let cached = exact.exact_candidate_groups(&urls);
    assert_eq!(
        cached.len(),
        urls.len(),
        "prepared candidates have cached exact-credit identities"
    );
    assert!(
        exact
            .exact_candidate_groups(&["not-admitted".into()])
            .is_empty(),
        "tracks outside the frozen eligible inventory do not acquire a cached identity"
    );
}

#[test]
fn contributors_with_the_same_musicbrainz_artist_id_share_history_without_requiring_mbids() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("ATTACH DATABASE ':memory:' AS history;
      CREATE TABLE tracks(id INTEGER PRIMARY KEY, urlmd5 TEXT, album INTEGER, primary_artist INTEGER);
      CREATE TABLE contributors(id INTEGER PRIMARY KEY, musicbrainz_id TEXT);
      CREATE TABLE contributor_track(track INTEGER, contributor INTEGER, role INTEGER);
      CREATE TABLE history.tracks_persistent(urlmd5 TEXT PRIMARY KEY, playCount INTEGER, lastPlayed INTEGER);
      INSERT INTO contributors VALUES (1,'ABC-123'),(2,'abc-123'),(3,NULL);
      INSERT INTO tracks VALUES (1,'a',10,1),(2,'b',11,2),(3,'c',12,3);
      INSERT INTO history.tracks_persistent VALUES ('a',2,10),('b',3,20),('c',4,30);").unwrap();
    let totals =
        accumulate_catalogue(&db, ListeningSource::Lyrion, ArtistIdentity::SharedArtist).unwrap();
    assert_eq!(totals.artist(1).unwrap().playcount_sum, 5);
    assert_eq!(totals.artist(2).unwrap().playcount_sum, 5);
    assert_eq!(totals.canonical_artist(1), totals.canonical_artist(2));
    assert_eq!(totals.artist(3).unwrap().playcount_sum, 4);
}

#[test]
fn catalogue_and_persistent_history_join_preserves_coverage_and_roles() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("ATTACH DATABASE ':memory:' AS history;
      CREATE TABLE tracks(id INTEGER PRIMARY KEY, urlmd5 TEXT, album INTEGER, primary_artist INTEGER);
      CREATE TABLE contributors(id INTEGER PRIMARY KEY, musicbrainz_id TEXT);
      CREATE TABLE contributor_track(track INTEGER, contributor INTEGER, role INTEGER);
      CREATE TABLE history.tracks_persistent(urlmd5 TEXT PRIMARY KEY, playCount INTEGER, lastPlayed INTEGER);
      CREATE TABLE history.alternativeplaycount(urlmd5 TEXT PRIMARY KEY, playCount INTEGER, lastPlayed INTEGER);
      INSERT INTO tracks VALUES (1,'a',10,1),(2,'b',10,2),(3,'c',11,3),(4,'nulls',10,4);
      INSERT INTO contributor_track VALUES (1,1,1),(1,1,6),(1,2,6),(1,9,2),(2,2,1);
      INSERT INTO history.tracks_persistent VALUES ('a',3,100),('b',2,200),('nulls',NULL,NULL);
      INSERT INTO history.alternativeplaycount VALUES ('a',7,300),('c',4,400);").unwrap();
    let lyrion =
        accumulate_catalogue(&db, ListeningSource::Lyrion, ArtistIdentity::SharedArtist).unwrap();
    assert_eq!(lyrion.album(10).unwrap().playcount_sum, 5);
    assert_eq!(lyrion.album(10).unwrap().track_count, 3);
    assert_eq!(
        lyrion.album(10).unwrap().known_playcounts,
        2,
        "SQL NULL counts remain unknown instead of being counted as zero"
    );
    assert_eq!(
        lyrion.album(10).unwrap().known_last_played,
        2,
        "SQL NULL timestamps remain unknown instead of being counted as never played"
    );
    assert_eq!(lyrion.artist(1).unwrap().track_count, 1);
    assert_eq!(lyrion.artist(2).unwrap().playcount_sum, 5);
    assert!(lyrion.artist(9).is_none());
    assert_eq!(lyrion.album(11).unwrap().known_playcounts, 0);
    let primary =
        accumulate_catalogue(&db, ListeningSource::Lyrion, ArtistIdentity::PrimaryArtist).unwrap();
    assert_eq!(primary.artist(2).unwrap().playcount_sum, 2);
    let apc = accumulate_catalogue(
        &db,
        ListeningSource::AlternativePlayCount,
        ArtistIdentity::SharedArtist,
    )
    .unwrap();
    assert_eq!(
        (
            apc.album(10).unwrap().playcount_sum,
            apc.album(10).unwrap().known_playcounts
        ),
        (7, 1)
    );
    assert_eq!(apc.album(10).unwrap().last_played, Some(300));
    assert_eq!(
        apc.album(11).unwrap().playcount_sum,
        4,
        "APC history is available independently of a Lyrion persistent row"
    );
    assert_eq!(apc.album(11).unwrap().last_played, Some(400));
    let groups = candidate_groups(
        &db,
        &["a".into(), "b".into(), "missing".into()],
        ArtistIdentity::SharedArtist,
    )
    .unwrap();
    assert_eq!(groups["a"].album, Some(10));
    assert_eq!(groups["a"].artists, vec![1, 2]);
    assert!(!groups.contains_key("missing"));
    let primary_groups =
        candidate_groups(&db, &["a".into()], ArtistIdentity::PrimaryArtist).unwrap();
    assert_eq!(primary_groups["a"].artists, vec![1]);
}

#[test]
fn candidate_scoped_aggregation_preserves_full_album_and_artist_totals() {
    use bliss_guidance_library_signals::accumulate_selected_catalogue;
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("ATTACH DATABASE ':memory:' AS history;
      CREATE TABLE tracks(id INTEGER PRIMARY KEY, urlmd5 TEXT, album INTEGER, primary_artist INTEGER);
      CREATE TABLE contributors(id INTEGER PRIMARY KEY, musicbrainz_id TEXT);
      CREATE TABLE contributor_track(track INTEGER, contributor INTEGER, role INTEGER);
      CREATE TABLE history.tracks_persistent(urlmd5 TEXT PRIMARY KEY, playCount INTEGER, lastPlayed INTEGER);
      INSERT INTO contributors VALUES (1,'abc'),(2,'abc'),(3,NULL),(4,NULL);
      INSERT INTO tracks VALUES (1,'a',10,1),(2,'b',10,3),(3,'c',11,2),(4,'d',12,4);
      INSERT INTO contributor_track VALUES (1,1,1),(2,3,1),(3,2,1),(4,4,1);
      INSERT INTO history.tracks_persistent VALUES ('a',2,10),('b',3,20),('c',4,30),('d',9,40);").unwrap();
    let full =
        accumulate_catalogue(&db, ListeningSource::Lyrion, ArtistIdentity::SharedArtist).unwrap();
    let selected = accumulate_selected_catalogue(
        &db,
        ListeningSource::Lyrion,
        ArtistIdentity::SharedArtist,
        &["a".into()],
    )
    .unwrap();
    assert_eq!(selected.album(10), full.album(10));
    assert_eq!(selected.artist(1), full.artist(1));
    assert_eq!(selected.artist(2), full.artist(2));
    assert!(selected.album(12).is_none());
    assert!(selected.artist(4).is_none());
}

#[test]
fn albums_and_collaborators_accumulate_track_values_once_per_group() {
    let mut totals = HistoryAccumulator::default();
    totals.add(&TrackHistory {
        album: Some(10),
        artists: &[1, 1, 2],
        playcount: Some(3),
        last_played: Some(100),
    });
    totals.add(&TrackHistory {
        album: Some(10),
        artists: &[1],
        playcount: Some(2),
        last_played: Some(200),
    });
    assert_eq!(totals.album(10).unwrap().playcount_sum, 5);
    assert_eq!(totals.album(10).unwrap().last_played, Some(200));
    assert_eq!(totals.artist(1).unwrap().playcount_sum, 5);
    assert_eq!(totals.artist(2).unwrap().playcount_sum, 3);
    assert_eq!(totals.artist(1).unwrap().track_count, 2);
}

#[test]
fn missing_values_are_distinct_from_zero_and_coverage_is_retained() {
    let mut totals = HistoryAccumulator::default();
    totals.add(&TrackHistory {
        album: Some(3),
        artists: &[7],
        playcount: None,
        last_played: None,
    });
    totals.add(&TrackHistory {
        album: Some(3),
        artists: &[7],
        playcount: Some(0),
        last_played: Some(0),
    });
    let album = totals.album(3).unwrap();
    assert_eq!(
        (
            album.track_count,
            album.known_playcounts,
            album.playcount_sum
        ),
        (2, 1, 0)
    );
    assert_eq!((album.known_last_played, album.last_played), (1, Some(0)));
    assert_eq!(totals.artist(7).unwrap(), album);
    assert!(totals.album(99).is_none());
}

#[test]
fn missing_album_or_artist_does_not_discard_other_identity() {
    let mut totals = HistoryAccumulator::default();
    totals.add(&TrackHistory {
        album: None,
        artists: &[9],
        playcount: Some(1),
        last_played: None,
    });
    totals.add(&TrackHistory {
        album: Some(4),
        artists: &[],
        playcount: Some(2),
        last_played: None,
    });
    assert_eq!(totals.artist(9).unwrap().playcount_sum, 1);
    assert_eq!(totals.album(4).unwrap().playcount_sum, 2);
}

#[test]
fn zero_or_negative_lyrion_ids_do_not_create_shared_unknown_groups() {
    let mut totals = HistoryAccumulator::default();
    totals.add(&TrackHistory {
        album: Some(0),
        artists: &[0, -1],
        playcount: Some(5),
        last_played: Some(7),
    });
    assert!(totals.album(0).is_none());
    assert!(totals.artist(0).is_none());
    assert!(totals.artist(-1).is_none());
}
