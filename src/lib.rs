// SPDX-License-Identifier: GPL-3.0-only

//! Accumulation primitives for source-provided listening history.
//!
//! The caller supplies one record per catalogue track; no recording-level
//! deduplication or inference of independent listening events is performed.

use rusqlite::{params_from_iter, Connection};
use std::collections::{BTreeSet, HashMap, HashSet};
use unicode_normalization::UnicodeNormalization;

/// Listening source tables live in a trusted, read-only `history` database.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ListeningSource {
    Lyrion,
    AlternativePlayCount,
}

/// Supported catalogue-backed artist identity strategies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtistIdentity {
    SharedArtist,
    PrimaryArtist,
    ExactBlissCredit,
}

/// Lyrion groups associated with one candidate file, without inferring that
/// its separate credited artists share a single listening history.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CandidateGroups {
    pub album: Option<i64>,
    pub artists: Vec<i64>,
}

pub fn candidate_groups(
    db: &Connection,
    urlmd5s: &[String],
    identity: ArtistIdentity,
) -> rusqlite::Result<HashMap<String, CandidateGroups>> {
    candidate_groups_with_schema(db, urlmd5s, identity, "main")
}

pub fn candidate_groups_attached(
    db: &Connection,
    urlmd5s: &[String],
    identity: ArtistIdentity,
) -> rusqlite::Result<HashMap<String, CandidateGroups>> {
    candidate_groups_with_schema(db, urlmd5s, identity, "catalogue")
}

fn candidate_groups_with_schema(
    db: &Connection,
    urlmd5s: &[String],
    identity: ArtistIdentity,
    schema: &str,
) -> rusqlite::Result<HashMap<String, CandidateGroups>> {
    let mut groups = HashMap::<String, CandidateGroups>::new();
    let unique: Vec<_> = urlmd5s
        .iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    for batch in unique.chunks(900) {
        let placeholders = std::iter::repeat_n("?", batch.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = if identity == ArtistIdentity::ExactBlissCredit {
            format!("SELECT t.urlmd5,t.album,b.rowid,NULL FROM {schema}.tracks t LEFT JOIN identities.track_identity i ON i.lms_track_id=t.id LEFT JOIN bliss.TracksV2 b ON b.File=i.database_file AND b.Artist IS NOT NULL AND trim(b.Artist)<>'' WHERE t.urlmd5 IN ({placeholders})")
        } else {
            format!("SELECT t.urlmd5, t.album, t.primary_artist, ct.contributor FROM {schema}.tracks t LEFT JOIN {schema}.contributor_track ct ON ct.track=t.id AND ct.role IN (1,6) WHERE t.urlmd5 IN ({placeholders})")
        };
        let mut statement = db.prepare(&sql)?;
        let mut rows = statement.query(params_from_iter(batch.iter()))?;
        while let Some(row) = rows.next()? {
            let key: String = row.get(0)?;
            let group = groups.entry(key).or_insert(CandidateGroups {
                album: row.get(1)?,
                artists: Vec::new(),
            });
            if let Some(primary) = row.get::<_, Option<i64>>(2)? {
                group.artists.push(primary);
            }
            if identity == ArtistIdentity::SharedArtist {
                if let Some(artist) = row.get::<_, Option<i64>>(3)? {
                    group.artists.push(artist);
                }
            }
        }
    }
    for group in groups.values_mut() {
        group.artists.sort_unstable();
        group.artists.dedup();
    }
    Ok(groups)
}

/// Stream the complete catalogue once into album and artist totals.
///
/// The caller attaches a trusted persistent database as `history` and owns
/// the read-only snapshots. Only artist and track-artist contributor roles
/// participate; the primary artist is included even when no role row exists.
pub fn accumulate_catalogue(
    db: &Connection,
    source: ListeningSource,
    identity: ArtistIdentity,
) -> rusqlite::Result<HistoryAccumulator> {
    accumulate_catalogue_with_schemas(db, source, identity, "main", "history", None)
}

fn accumulate_exact_credit(
    db: &Connection,
    source: ListeningSource,
    catalogue_schema: &str,
    history_schema: &str,
    selected_urls: Option<&[String]>,
) -> rusqlite::Result<HistoryAccumulator> {
    let mut totals = HistoryAccumulator::default();
    let mut canonical = HashMap::<String, i64>::new();
    let mut raw_credit_keys = HashMap::<String, i64>::new();
    let mut statement = db.prepare("SELECT rowid, Artist FROM bliss.TracksV2 ORDER BY rowid")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let raw: Option<String> = row.get(1)?;
        let raw = raw.unwrap_or_default();
        let key = if let Some(key) = raw_credit_keys.get(&raw) {
            *key
        } else {
            let credit = raw
                .nfkc()
                .flat_map(char::to_lowercase)
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let key = if credit.is_empty() {
                0
            } else {
                *canonical.entry(credit).or_insert(id)
            };
            raw_credit_keys.insert(raw, key);
            key
        };
        if key != 0 {
            totals.canonical_artists.insert(id, key);
        }
    }
    drop(rows);
    drop(statement);
    let mut selected_albums = BTreeSet::new();
    if let Some(urls) = selected_urls {
        for (url, group) in
            candidate_groups_with_schema(db, urls, ArtistIdentity::PrimaryArtist, catalogue_schema)?
        {
            if let Some(album) = group.album {
                selected_albums.insert(album);
            }
            totals.exact_candidate_groups.insert(
                url,
                CandidateGroups {
                    album: group.album,
                    artists: Vec::new(),
                },
            );
        }
    }
    let (join, count, last) = match source {
        ListeningSource::Lyrion => (String::new(), "tp.playCount", "tp.lastPlayed"),
        ListeningSource::AlternativePlayCount => (
            format!("LEFT JOIN {history_schema}.alternativeplaycount apc ON apc.urlmd5=t.urlmd5"),
            "apc.playCount",
            "apc.lastPlayed",
        ),
    };
    let filter = if selected_urls.is_none() {
        String::new()
    } else if selected_albums.is_empty() {
        " WHERE 0".to_owned()
    } else if selected_albums.len() > 900 {
        // A large inventory can exceed SQLite's host-variable limit. Scan the
        // catalogue and apply the same selection while accumulating instead.
        String::new()
    } else {
        let marks = std::iter::repeat_n("?", selected_albums.len())
            .collect::<Vec<_>>()
            .join(",");
        format!(" WHERE t.album IN ({marks})")
    };
    let album_parameters: Vec<i64> = if selected_urls.is_some() && selected_albums.len() <= 900 {
        selected_albums.iter().copied().collect()
    } else {
        Vec::new()
    };
    let album_sql = format!("SELECT t.album,{count},{last} FROM {catalogue_schema}.tracks t LEFT JOIN {history_schema}.tracks_persistent tp ON tp.urlmd5=t.urlmd5 {join}{filter}");
    let mut statement = db.prepare(&album_sql)?;
    let mut rows = statement.query(params_from_iter(album_parameters.iter()))?;
    while let Some(row) = rows.next()? {
        let count: Option<i64> = row.get(1)?;
        let last: Option<i64> = row.get(2)?;
        let album: Option<i64> = row.get(0)?;
        if selected_urls.is_some() && !album.is_some_and(|id| selected_albums.contains(&id)) {
            continue;
        }
        totals.add(&TrackHistory {
            album,
            artists: &[],
            playcount: count.map(|v| v.max(0) as u64),
            last_played: last.map(|v| v.max(0) as u64),
        });
    }
    drop(rows);
    drop(statement);
    if let Some(urls) = selected_urls.filter(|urls| urls.len() <= 128) {
        if accumulate_exact_selected_fast(
            db,
            &mut totals,
            urls,
            &join,
            count,
            last,
            catalogue_schema,
            history_schema,
        )? {
            return Ok(totals);
        }
    }
    let sql = format!("SELECT t.id,t.album,b.rowid,{count},{last},t.urlmd5 FROM identities.track_identity i JOIN {catalogue_schema}.tracks t ON t.id=i.lms_track_id JOIN bliss.TracksV2 b ON b.File=i.database_file LEFT JOIN {history_schema}.tracks_persistent tp ON tp.urlmd5=t.urlmd5 {join}");
    let mut statement = db.prepare(&sql)?;
    let mut rows = statement.query([])?;
    let selected_keys =
        selected_urls.map(|urls| urls.iter().map(String::as_str).collect::<HashSet<_>>());
    // Multiple Bliss files can refer to one LMS track. Collect them before
    // attributing history so conflicting credits cannot be resolved by row
    // order. Matching normalized credits still count the track only once.
    type ExactTrack = (
        Option<i64>,
        Option<i64>,
        Option<u64>,
        Option<u64>,
        Option<String>,
        bool,
    );
    let mut tracks = HashMap::<i64, ExactTrack>::new();
    let mut selected_artists = BTreeSet::new();
    while let Some(row) = rows.next()? {
        let track_id: i64 = row.get(0)?;
        let album: Option<i64> = row.get(1)?;
        let artist: Option<i64> = row.get(2)?;
        let count: Option<i64> = row.get(3)?;
        let last: Option<i64> = row.get(4)?;
        let artist = artist.and_then(|id| totals.canonical_artists.get(&id).copied());
        let url: Option<String> = row.get(5)?;
        if let Some(existing) = tracks.get_mut(&track_id) {
            existing.5 |= existing.1 != artist;
        } else {
            tracks.insert(
                track_id,
                (
                    album,
                    artist,
                    count.map(|v| v.max(0) as u64),
                    last.map(|v| v.max(0) as u64),
                    url,
                    false,
                ),
            );
        }
    }
    drop(rows);
    drop(statement);
    for (album, artist, count, last, url, ambiguous) in tracks.into_values() {
        let artists: Vec<i64> = if ambiguous {
            Vec::new()
        } else {
            artist.into_iter().collect()
        };
        if let Some(url) = url.filter(|url| {
            selected_keys
                .as_ref()
                .is_none_or(|set| set.contains(url.as_str()))
        }) {
            selected_artists.extend(artists.iter().map(|id| totals.canonical_artist(*id)));
            totals.exact_candidate_groups.insert(
                url,
                CandidateGroups {
                    album,
                    artists: artists.clone(),
                },
            );
        }
        totals.add_selected(
            &TrackHistory {
                album: None,
                artists: &artists,
                playcount: count,
                last_played: last,
            },
            None,
        );
    }
    if selected_urls.is_some() {
        totals.artists.retain(|id, _| selected_artists.contains(id));
    }
    Ok(totals)
}

/// For small candidate sets, join listening data only for Bliss credits
/// represented by candidates. Ambiguous identity snapshots use the general
/// path, which reconciles conflicting credits before attributing history.
#[allow(clippy::too_many_arguments)]
fn accumulate_exact_selected_fast(
    db: &Connection,
    totals: &mut HistoryAccumulator,
    urls: &[String],
    join: &str,
    count: &str,
    last: &str,
    catalogue_schema: &str,
    history_schema: &str,
) -> rusqlite::Result<bool> {
    let mut seen = HashSet::new();
    let mut statement = db.prepare("SELECT lms_track_id FROM identities.track_identity")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        if !seen.insert(id) {
            return Ok(false);
        }
    }
    drop(rows);
    drop(statement);
    drop(seen);
    let groups =
        candidate_groups_with_schema(db, urls, ArtistIdentity::ExactBlissCredit, catalogue_schema)?;
    let mut selected = BTreeSet::new();
    for (url, mut group) in groups {
        group.artists = group
            .artists
            .into_iter()
            .filter_map(|id| totals.canonical_artists.get(&id).copied())
            .collect();
        group.artists.sort_unstable();
        group.artists.dedup();
        selected.extend(group.artists.iter().copied());
        totals.exact_candidate_groups.insert(url, group);
    }
    let selected_rows: Vec<i64> = totals
        .canonical_artists
        .iter()
        .filter_map(|(row, credit)| selected.contains(credit).then_some(*row))
        .collect();
    if selected_rows.len() > 20_000 {
        return Ok(false);
    }
    for batch in selected_rows.chunks(900) {
        let marks = std::iter::repeat_n("?", batch.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("SELECT b.rowid,{count},{last} FROM bliss.TracksV2 b JOIN identities.track_identity i ON i.database_file=b.File JOIN {catalogue_schema}.tracks t ON t.id=i.lms_track_id LEFT JOIN {history_schema}.tracks_persistent tp ON tp.urlmd5=t.urlmd5 {join} WHERE b.rowid IN ({marks})");
        let mut statement = db.prepare(&sql)?;
        let mut rows = statement.query(params_from_iter(batch.iter()))?;
        while let Some(row) = rows.next()? {
            let raw_id: i64 = row.get(0)?;
            let artist = totals.canonical_artist(raw_id);
            let count: Option<i64> = row.get(1)?;
            let last: Option<i64> = row.get(2)?;
            totals.add(&TrackHistory {
                album: None,
                artists: &[artist],
                playcount: count.map(|value| value.max(0) as u64),
                last_played: last.map(|value| value.max(0) as u64),
            });
        }
    }
    Ok(true)
}

/// Variant for a persistent-history main database with an attached catalogue.
pub fn accumulate_attached_catalogue(
    db: &Connection,
    source: ListeningSource,
    identity: ArtistIdentity,
) -> rusqlite::Result<HistoryAccumulator> {
    accumulate_catalogue_with_schemas(db, source, identity, "catalogue", "main", None)
}

/// Aggregate only groups represented by the supplied candidates, while reading
/// every catalogue track belonging to those groups.
pub fn accumulate_selected_catalogue(
    db: &Connection,
    source: ListeningSource,
    identity: ArtistIdentity,
    urlmd5s: &[String],
) -> rusqlite::Result<HistoryAccumulator> {
    accumulate_catalogue_with_schemas(db, source, identity, "main", "history", Some(urlmd5s))
}

pub fn accumulate_selected_attached_catalogue(
    db: &Connection,
    source: ListeningSource,
    identity: ArtistIdentity,
    urlmd5s: &[String],
) -> rusqlite::Result<HistoryAccumulator> {
    accumulate_catalogue_with_schemas(db, source, identity, "catalogue", "main", Some(urlmd5s))
}

fn accumulate_catalogue_with_schemas(
    db: &Connection,
    source: ListeningSource,
    identity: ArtistIdentity,
    catalogue_schema: &str,
    history_schema: &str,
    selected_urls: Option<&[String]>,
) -> rusqlite::Result<HistoryAccumulator> {
    if identity == ArtistIdentity::ExactBlissCredit {
        return accumulate_exact_credit(
            db,
            source,
            catalogue_schema,
            history_schema,
            selected_urls,
        );
    }
    let (join, count, last) = match source {
        ListeningSource::Lyrion => ("", "tp.playCount", "tp.lastPlayed"),
        ListeningSource::AlternativePlayCount => (
            "LEFT JOIN {history_schema}.alternativeplaycount apc ON apc.urlmd5 = t.urlmd5",
            "apc.playCount",
            "apc.lastPlayed",
        ),
    };
    let join = join.replace("{history_schema}", history_schema);
    let mut totals = HistoryAccumulator::default();
    let mut aliases = HashMap::<String, i64>::new();
    let mut contributor_statement = db.prepare(&format!(
        "SELECT id, musicbrainz_id FROM {catalogue_schema}.contributors ORDER BY id"
    ))?;
    let mut contributor_rows = contributor_statement.query([])?;
    while let Some(row) = contributor_rows.next()? {
        let id: i64 = row.get(0)?;
        let mbid: Option<String> = row.get(1)?;
        if let Some(mbid) = mbid
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty())
        {
            let canonical = *aliases.entry(mbid).or_insert(id);
            totals.canonical_artists.insert(id, canonical);
        }
    }
    drop(contributor_rows);
    drop(contributor_statement);
    let mut filter_albums = BTreeSet::new();
    let mut filter_artists = BTreeSet::new();
    let mut bind_values = Vec::<i64>::new();
    let mut filter = String::new();
    if let Some(urls) = selected_urls {
        for group in
            candidate_groups_with_schema(db, urls, identity, catalogue_schema)?.into_values()
        {
            if let Some(album) = group.album.filter(|id| *id > 0) {
                filter_albums.insert(album);
            }
            for artist in group.artists.into_iter().filter(|id| *id > 0) {
                filter_artists.insert(totals.canonical_artist(artist));
            }
        }
        let mut raw_artists = filter_artists.clone();
        raw_artists.extend(
            totals
                .canonical_artists
                .iter()
                .filter_map(|(id, canonical)| filter_artists.contains(canonical).then_some(*id)),
        );
        if filter_albums.len() + raw_artists.len() * 2 <= 900 {
            let mut selects = Vec::new();
            if !filter_albums.is_empty() {
                let marks = std::iter::repeat_n("?", filter_albums.len())
                    .collect::<Vec<_>>()
                    .join(",");
                selects.push(format!(
                    "SELECT id FROM {catalogue_schema}.tracks WHERE album IN ({marks})"
                ));
                bind_values.extend(filter_albums.iter().copied());
            }
            if !raw_artists.is_empty() {
                let marks = std::iter::repeat_n("?", raw_artists.len())
                    .collect::<Vec<_>>()
                    .join(",");
                selects.push(format!(
                    "SELECT id FROM {catalogue_schema}.tracks WHERE primary_artist IN ({marks})"
                ));
                bind_values.extend(raw_artists.iter().copied());
                if identity == ArtistIdentity::SharedArtist {
                    selects.push(format!("SELECT track FROM {catalogue_schema}.contributor_track WHERE role IN (1,6) AND contributor IN ({marks})"));
                    bind_values.extend(raw_artists.iter().copied());
                }
            }
            filter = if selects.is_empty() {
                " WHERE 0".to_owned()
            } else {
                format!(" WHERE t.id IN ({})", selects.join(" UNION "))
            };
        }
    }
    let sql = format!("SELECT t.id, t.album, t.primary_artist, ct.contributor, {count}, {last}
        FROM {catalogue_schema}.tracks t LEFT JOIN {catalogue_schema}.contributor_track ct ON ct.track = t.id AND ct.role IN (1, 6)
        LEFT JOIN {history_schema}.tracks_persistent tp ON tp.urlmd5 = t.urlmd5 {join}{filter} ORDER BY t.id");
    let mut statement = db.prepare(&sql)?;
    let mut rows = statement.query(params_from_iter(bind_values.iter()))?;
    type PendingTrack = (i64, Option<i64>, Vec<i64>, Option<u64>, Option<u64>);
    let mut current: Option<PendingTrack> = None;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        if current.as_ref().is_some_and(|track| track.0 != id) {
            let (_, album, artists, playcount, last_played) = current.take().unwrap();
            totals.add_selected(
                &TrackHistory {
                    album,
                    artists: &artists,
                    playcount,
                    last_played,
                },
                selected_urls.map(|_| (&filter_albums, &filter_artists)),
            );
        }
        if current.is_none() {
            let primary: Option<i64> = row.get(2)?;
            let count: Option<i64> = row.get(4)?;
            let last: Option<i64> = row.get(5)?;
            current = Some((
                id,
                row.get(1)?,
                primary.into_iter().collect(),
                count.map(|v| v.max(0) as u64),
                last.map(|v| v.max(0) as u64),
            ));
        }
        if identity == ArtistIdentity::SharedArtist {
            if let Some(artist) = row.get::<_, Option<i64>>(3)? {
                current.as_mut().unwrap().2.push(artist);
            }
        }
    }
    if let Some((_, album, artists, playcount, last_played)) = current {
        totals.add_selected(
            &TrackHistory {
                album,
                artists: &artists,
                playcount,
                last_played,
            },
            selected_urls.map(|_| (&filter_albums, &filter_artists)),
        );
    }
    Ok(totals)
}

/// A single catalogue track's identities and optional source observations.
pub struct TrackHistory<'a> {
    pub album: Option<i64>,
    pub artists: &'a [i64],
    pub playcount: Option<u64>,
    pub last_played: Option<u64>,
}

/// Aggregate values retain coverage so missing data is not confused with zero.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HistoryTotal {
    pub track_count: u64,
    pub known_playcounts: u64,
    pub playcount_sum: u64,
    pub known_last_played: u64,
    pub last_played: Option<u64>,
}

impl HistoryTotal {
    fn add(&mut self, track: &TrackHistory<'_>) {
        self.track_count = self.track_count.saturating_add(1);
        if let Some(count) = track.playcount {
            self.known_playcounts = self.known_playcounts.saturating_add(1);
            self.playcount_sum = self.playcount_sum.saturating_add(count);
        }
        if let Some(last_played) = track.last_played {
            self.known_last_played = self.known_last_played.saturating_add(1);
            self.last_played = Some(self.last_played.unwrap_or(0).max(last_played));
        }
    }
}

/// Streaming accumulation uses memory proportional to distinct groups.
#[derive(Default)]
pub struct HistoryAccumulator {
    albums: HashMap<i64, HistoryTotal>,
    artists: HashMap<i64, HistoryTotal>,
    canonical_artists: HashMap<i64, i64>,
    exact_candidate_groups: HashMap<String, CandidateGroups>,
}

impl HistoryAccumulator {
    pub fn exact_candidate_groups(&self, urls: &[String]) -> HashMap<String, CandidateGroups> {
        urls.iter()
            .filter_map(|url| {
                self.exact_candidate_groups
                    .get(url)
                    .map(|group| (url.clone(), group.clone()))
            })
            .collect()
    }
    pub fn canonical_artist(&self, id: i64) -> i64 {
        self.canonical_artists.get(&id).copied().unwrap_or(id)
    }
    pub fn albums(&self) -> impl Iterator<Item = &HistoryTotal> {
        self.albums.values()
    }
    pub fn artists(&self) -> impl Iterator<Item = &HistoryTotal> {
        self.artists.values()
    }
    pub fn add(&mut self, track: &TrackHistory<'_>) {
        self.add_selected(track, None);
    }
    fn add_selected(
        &mut self,
        track: &TrackHistory<'_>,
        filter: Option<(&BTreeSet<i64>, &BTreeSet<i64>)>,
    ) {
        if let Some(album) = track
            .album
            .filter(|id| *id > 0 && filter.is_none_or(|(albums, _)| albums.contains(id)))
        {
            self.albums.entry(album).or_default().add(track);
        }
        for artist in track
            .artists
            .iter()
            .filter(|id| **id > 0)
            .map(|id| self.canonical_artists.get(id).copied().unwrap_or(*id))
            .filter(|id| filter.is_none_or(|(_, artists)| artists.contains(id)))
            .collect::<BTreeSet<_>>()
        {
            self.artists.entry(artist).or_default().add(track);
        }
    }

    pub fn album(&self, id: i64) -> Option<&HistoryTotal> {
        self.albums.get(&id)
    }

    pub fn artist(&self, id: i64) -> Option<&HistoryTotal> {
        self.artists.get(&self.canonical_artist(id))
    }
}
