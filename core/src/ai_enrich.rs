//! AI-assisted genre, subgenre and mood enrichment.
//!
//! Exports unenriched albums with a canonical taxonomy (Broad Genre -> Subgenres
//! + Moods) as a self-contained prompt for any AI of the user's choice, parses
//! the AI's JSON reply into a persistent review queue, supports flagging
//! questionable albums for a follow-up AI discussion (with full tracklists and
//! user notes), and writes approved tags directly into the audio files:
//!   - GENRE (multi-value): [BroadGenre, Subgenre1, Subgenre2?]
//!   - MOOD (multi-value): [Mood1, Mood2, Mood3?]
//!   - LOCALBEAT_AI_ENRICHED: taxonomy version (e.g. "v1")

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use lofty::TextEncoding;
use lofty::config::{ParseOptions, ParsingMode, WriteOptions};
use lofty::file::FileType;
use lofty::id3::v2::{ExtendedTextFrame, Frame, FrameId, Id3v2Tag, TextInformationFrame};
use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst, Mp4File};
use lofty::mpeg::MpegFile;
use lofty::ogg::tag::VorbisComments;
use lofty::ogg::{OpusFile, VorbisFile};
use lofty::prelude::*;
use lofty::tag::{ItemKey, Tag};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::borrow::Cow;

use crate::library::{Album, Library, norm};

pub const AI_ENRICHED_KEY: &str = "LOCALBEAT_AI_ENRICHED";
pub const MP4_AI_ENRICHED_KEY: &str = "----:com.apple.iTunes:LOCALBEAT_AI_ENRICHED";
pub const MP4_MOOD_KEY: &str = "----:com.apple.iTunes:MOOD";
pub const DEFAULT_BATCH_SIZE: usize = 60;

// ---------------------------------------------------------------- taxonomy

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BroadGenre {
    pub name: String,
    pub subgenres: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Taxonomy {
    pub version: String,
    pub broad_genres: Vec<BroadGenre>,
    pub moods: Vec<String>,
}

impl Taxonomy {
    pub fn default_v1() -> Self {
        let bg = |name: &str, subs: &[&str]| BroadGenre {
            name: name.to_string(),
            subgenres: subs.iter().map(|s| (*s).to_string()).collect(),
        };
        Taxonomy {
            version: "v1".to_string(),
            broad_genres: vec![
                bg(
                    "Rock",
                    &[
                        "Classic Rock",
                        "Hard Rock",
                        "Blues Rock",
                        "Southern Rock",
                        "Progressive Rock",
                        "Psychedelic Rock",
                        "Glam Rock",
                        "Heartland Rock",
                        "Folk Rock",
                        "Soft Rock",
                        "Garage Rock",
                        "Punk Rock",
                        "Surf & Rockabilly",
                    ],
                ),
                bg(
                    "Alternative & Indie",
                    &[
                        "Alternative Rock",
                        "Indie Rock",
                        "Indie Pop",
                        "Jangle Pop",
                        "Post-Punk",
                        "New Wave",
                        "Grunge",
                        "Britpop",
                        "Shoegaze & Dream Pop",
                        "College Rock",
                        "Indie Folk",
                    ],
                ),
                bg(
                    "Metal",
                    &[
                        "Heavy Metal",
                        "Glam Metal",
                        "Thrash Metal",
                        "Progressive Metal",
                        "Alt-Metal & Nu Metal",
                    ],
                ),
                bg(
                    "Pop",
                    &[
                        "Pop Rock",
                        "Synth-Pop",
                        "Dance Pop",
                        "Art Pop",
                        "Baroque & Chamber Pop",
                        "Power Pop",
                        "Adult Contemporary",
                        "Vocal Pop",
                    ],
                ),
                bg(
                    "Folk & Country",
                    &[
                        "Contemporary Folk",
                        "Traditional Folk",
                        "Singer-Songwriter",
                        "Americana",
                        "Alt-Country",
                        "Classic Country",
                        "Outlaw Country",
                        "Bluegrass",
                        "Celtic",
                    ],
                ),
                bg(
                    "Blues & Soul",
                    &[
                        "Electric Blues",
                        "Acoustic & Delta Blues",
                        "Classic Soul",
                        "Motown",
                        "Funk",
                        "Gospel",
                    ],
                ),
                bg(
                    "Jazz",
                    &[
                        "Bebop & Hard Bop",
                        "Cool & Modal Jazz",
                        "Vocal Jazz",
                        "Jazz Fusion",
                        "Smooth Jazz",
                        "Swing & Big Band",
                    ],
                ),
                bg(
                    "Hip-Hop & R&B",
                    &[
                        "Golden Age Hip-Hop",
                        "Modern Hip-Hop",
                        "Contemporary R&B",
                        "Neo-Soul",
                    ],
                ),
                bg(
                    "Electronic",
                    &[
                        "Ambient",
                        "Downtempo & Trip-Hop",
                        "House & Techno",
                        "Synthwave & Electropop",
                        "IDM & Experimental",
                    ],
                ),
                bg(
                    "Classical",
                    &[
                        "Orchestral & Symphonic",
                        "Chamber Music",
                        "Solo Piano",
                        "Opera & Choral",
                        "Baroque",
                        "Modern Classical",
                    ],
                ),
                bg(
                    "Soundtracks & Stage",
                    &[
                        "Film Score",
                        "Movie Soundtrack",
                        "Musical Theatre",
                        "Video Game",
                    ],
                ),
                bg(
                    "World & Reggae",
                    &[
                        "Reggae & Dub",
                        "Ska",
                        "Latin",
                        "Afrobeat & African",
                        "World Fusion",
                    ],
                ),
            ],
            moods: [
                "Upbeat",
                "Energetic",
                "Driving",
                "Chill",
                "Acoustic",
                "Melancholy",
                "Introspective",
                "Gritty",
                "Soulful",
                "Dreamy",
                "Late Night",
                "Epic",
                "Playful",
                "Nostalgic",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
        }
    }

    pub fn file(config_dir: &Path) -> PathBuf {
        config_dir.join("ai-taxonomy.json")
    }

    /// Loads `<config>/ai-taxonomy.json`, writing the default v1 taxonomy if absent.
    pub fn load(config_dir: &Path) -> Self {
        let f = Self::file(config_dir);
        if let Ok(bytes) = std::fs::read(&f) {
            if let Ok(t) = serde_json::from_slice::<Taxonomy>(&bytes) {
                if !t.broad_genres.is_empty() && !t.moods.is_empty() {
                    return t;
                }
            }
        }
        let t = Self::default_v1();
        let _ = t.save(config_dir);
        t
    }

    pub fn save(&self, config_dir: &Path) -> Result<()> {
        let f = Self::file(config_dir);
        std::fs::create_dir_all(config_dir)?;
        let tmp = f.with_extension("json.part");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &f)?;
        Ok(())
    }

    pub fn broad_names(&self) -> Vec<&str> {
        self.broad_genres.iter().map(|b| b.name.as_str()).collect()
    }

    pub fn all_subgenres(&self) -> Vec<&str> {
        self.broad_genres.iter().flat_map(|b| b.subgenres.iter().map(String::as_str)).collect()
    }

    pub fn subgenres_for(&self, broad: &str) -> Vec<&str> {
        self.broad_genres
            .iter()
            .find(|b| b.name.eq_ignore_ascii_case(broad))
            .map(|b| b.subgenres.iter().map(String::as_str).collect())
            .unwrap_or_default()
    }

    pub fn canonical_broad<'a>(&'a self, s: &str) -> Option<&'a str> {
        let q = norm(s);
        self.broad_genres.iter().find(|b| norm(&b.name) == q).map(|b| b.name.as_str())
    }

    pub fn canonical_subgenre<'a>(&'a self, s: &str) -> Option<&'a str> {
        let q = norm(s);
        self.broad_genres
            .iter()
            .flat_map(|b| b.subgenres.iter())
            .find(|sub| norm(sub) == q)
            .map(String::as_str)
    }

    pub fn canonical_mood<'a>(&'a self, s: &str) -> Option<&'a str> {
        let q = norm(s);
        self.moods.iter().find(|m| norm(m) == q).map(String::as_str)
    }

    /// Formats the allowed taxonomy cleanly for an AI prompt.
    pub fn prompt_block(&self) -> String {
        let mut out = String::new();
        out.push_str("### Allowed Broad Genres & Subgenres\n");
        for bg in &self.broad_genres {
            out.push_str(&format!("- **{}**: {}\n", bg.name, bg.subgenres.join(", ")));
        }
        out.push_str("\n### Allowed Moods\n");
        out.push_str(&self.moods.join(", "));
        out.push('\n');
        out
    }
}

// ---------------------------------------------------------------- album IDs & pending store

/// Compact 8-hex-char ID for an album (avoids control chars in `Album::key` when
/// sending to an AI).
pub fn album_id(album_key: &str) -> String {
    format!("{:x}", md5::compute(album_key.as_bytes()))[..8].to_string()
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

impl Confidence {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "high" => Confidence::High,
            "low" => Confidence::Low,
            _ => Confidence::Medium,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Confidence::High => "High",
            Confidence::Medium => "Medium",
            Confidence::Low => "Low",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Proposal {
    pub album_id: String,
    pub album_key: String,
    pub artist: String,
    pub album: String,
    pub year: Option<u16>,
    #[serde(default)]
    pub old_genres: Vec<String>,
    #[serde(default)]
    pub old_moods: Vec<String>,
    pub broad_genre: String,
    #[serde(default)]
    pub subgenres: Vec<String>,
    #[serde(default)]
    pub moods: Vec<String>,
    pub confidence: Confidence,
    #[serde(default)]
    pub ai_note: String,
    /// Out-of-taxonomy values returned by the AI (surfaced in amber in the Review UI).
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Marked by the user for a follow-up AI discussion.
    #[serde(default)]
    pub flagged: bool,
    /// Optional user guidance sent into the follow-up AI discussion.
    #[serde(default)]
    pub user_note: String,
}

impl Proposal {
    /// Final `GENRE` list written to files: broad genre first, then distinct subgenres.
    pub fn final_genres(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.broad_genre.trim().is_empty() {
            out.push(self.broad_genre.trim().to_string());
        }
        for s in &self.subgenres {
            let t = s.trim();
            if !t.is_empty() && !out.iter().any(|x| x.eq_ignore_ascii_case(t)) {
                out.push(t.to_string());
            }
        }
        out
    }

    /// Recomputes out-of-taxonomy warnings after an inline edit.
    pub fn revalidate(&mut self, tax: &Taxonomy) {
        self.warnings.clear();
        if self.broad_genre.trim().is_empty() {
            self.warnings.push("missing broad genre".into());
        } else if let Some(c) = tax.canonical_broad(&self.broad_genre) {
            self.broad_genre = c.to_string();
        } else {
            self.warnings.push(format!("broad genre \"{}\" is not in taxonomy", self.broad_genre));
        }
        for sub in &mut self.subgenres {
            if let Some(c) = tax.canonical_subgenre(sub) {
                *sub = c.to_string();
            } else if !sub.trim().is_empty() {
                self.warnings.push(format!("subgenre \"{}\" is not in taxonomy", sub.trim()));
            }
        }
        for m in &mut self.moods {
            if let Some(c) = tax.canonical_mood(m) {
                *m = c.to_string();
            } else if !m.trim().is_empty() {
                self.warnings.push(format!("mood \"{}\" is not in taxonomy", m.trim()));
            }
        }
    }

    pub fn needs_attention(&self) -> bool {
        self.flagged || self.confidence != Confidence::High || !self.warnings.is_empty()
    }
}

/// Persistent review queue saved in `<config>/ai-enrich/pending.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PendingStore {
    pub items: BTreeMap<String, Proposal>,
}

impl PendingStore {
    pub fn dir(config_dir: &Path) -> PathBuf {
        config_dir.join("ai-enrich")
    }

    pub fn file(config_dir: &Path) -> PathBuf {
        Self::dir(config_dir).join("pending.json")
    }

    pub fn exports_dir(config_dir: &Path) -> PathBuf {
        Self::dir(config_dir).join("exports")
    }

    pub fn backups_dir(config_dir: &Path) -> PathBuf {
        Self::dir(config_dir).join("backups")
    }

    pub fn load(config_dir: &Path) -> Self {
        std::fs::read(Self::file(config_dir))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, config_dir: &Path) -> Result<()> {
        let f = Self::file(config_dir);
        std::fs::create_dir_all(f.parent().unwrap())?;
        let tmp = f.with_extension("json.part");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &f)?;
        Ok(())
    }
}

// ---------------------------------------------------------------- stats & export

#[derive(Debug, Clone, Copy, Default)]
pub struct EnrichStats {
    pub total_albums: usize,
    pub enriched_current: usize,
    pub pending_review: usize,
    pub remaining_to_export: usize,
    pub flagged: usize,
}

pub fn stats(lib: &Library, tax: &Taxonomy, pending: &PendingStore) -> EnrichStats {
    let mut s = EnrichStats {
        total_albums: lib.albums.len(),
        pending_review: pending.items.len(),
        flagged: pending.items.values().filter(|p| p.flagged).count(),
        ..Default::default()
    };
    for a in &lib.albums {
        let id = album_id(&a.key);
        if a.ai_enriched.as_deref() == Some(tax.version.as_str()) {
            s.enriched_current += 1;
        } else if !pending.items.contains_key(&id) {
            s.remaining_to_export += 1;
        }
    }
    s
}

#[derive(Debug, Clone)]
pub struct ExportOptions {
    pub batch_size: usize,
    /// When true, include albums already tagged with the current taxonomy version.
    pub include_enriched: bool,
    /// When true, include albums already waiting in the pending review queue.
    pub include_pending: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        ExportOptions {
            batch_size: DEFAULT_BATCH_SIZE,
            include_enriched: false,
            include_pending: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExportBatch {
    pub prompt: String,
    pub album_ids: Vec<String>,
    pub total_candidates: usize,
    pub remaining_after: usize,
}

fn is_compilation_artist(artist: &str) -> bool {
    let a = norm(artist);
    a.is_empty() || a == "various artists" || a == "various" || a == "soundtrack" || a == "compilations"
}

fn sample_tracks(lib: &Library, a: &Album, limit: usize) -> Vec<String> {
    a.songs
        .iter()
        .take(limit)
        .map(|&i| {
            let s = &lib.songs[i];
            if !s.artist.is_empty() && norm(&s.artist) != norm(&a.artist) {
                format!("{} — {}", s.artist, s.title)
            } else {
                s.title.clone()
            }
        })
        .collect()
}

/// Builds a self-contained prompt for the next chunk of unenriched albums.
pub fn export_batch_prompt(
    lib: &Library,
    tax: &Taxonomy,
    pending: &PendingStore,
    opts: &ExportOptions,
) -> Option<ExportBatch> {
    let candidates: Vec<&Album> = lib
        .albums
        .iter()
        .filter(|a| {
            let id = album_id(&a.key);
            let enriched = a.ai_enriched.as_deref() == Some(tax.version.as_str());
            (opts.include_enriched || !enriched) && (opts.include_pending || !pending.items.contains_key(&id))
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }
    let total_candidates = candidates.len();
    let take = opts.batch_size.max(1).min(total_candidates);
    let chosen = &candidates[..take];
    let album_ids: Vec<String> = chosen.iter().map(|a| album_id(&a.key)).collect();

    let mut prompt = String::new();
    prompt.push_str("# LocalBeat Genre & Mood Enrichment\n\n");
    prompt.push_str(
        "Classify each music album below so I can browse my personal library by specific subgenre and mood \
         (for example, 10,000 Maniacs should be `Alternative & Indie` / `Jangle Pop` / `Introspective`, \
         not lumped under generic `Rock` with Guns N' Roses `Rock` / `Hard Rock` / `Energetic`).\n\n",
    );
    prompt.push_str(&tax.prompt_block());
    prompt.push_str(
        "\n### Rules\n\
         1. Pick **exactly 1 `broad_genre`** from the Allowed Broad Genres list.\n\
         2. Pick **1–2 `subgenres`** from the Allowed Subgenres list (matching the album's actual era/sound, not just the artist's average).\n\
         3. Pick **1–3 `moods`** from the Allowed Moods list.\n\
         4. Set `confidence` to `\"high\"`, `\"medium\"`, or `\"low\"`. Use `\"low\"` or `\"medium\"` (with a short `note`) whenever an album is obscure, mixed-genre, or ambiguous.\n\
         5. Preserve every album's exact `id`.\n\
         6. Return **only** a JSON object in a ```json code block matching this exact schema:\n\n\
         ```json\n\
         {\n\
           \"albums\": [\n\
             {\n\
               \"id\": \"a1b2c3d4\",\n\
               \"broad_genre\": \"Alternative & Indie\",\n\
               \"subgenres\": [\"Jangle Pop\", \"College Rock\"],\n\
               \"moods\": [\"Introspective\", \"Melancholy\", \"Acoustic\"],\n\
               \"confidence\": \"high\",\n\
               \"note\": \"\"\n\
             }\n\
           ]\n\
         }\n\
         ```\n\n\
         ### Albums to classify\n\n\
         ```json\n",
    );
    let rows: Vec<Value> = chosen
        .iter()
        .map(|a| {
            let mut o = json!({
                "id": album_id(&a.key),
                "artist": a.artist,
                "album": a.title,
                "year": a.year,
                "existing_genres": a.genres,
            });
            if is_compilation_artist(&a.artist) || a.genres.is_empty() {
                let tracks = sample_tracks(lib, a, 10);
                if !tracks.is_empty() {
                    o["sample_tracks"] = json!(tracks);
                }
            }
            o
        })
        .collect();
    prompt.push_str(&serde_json::to_string_pretty(&rows).unwrap_or_default());
    prompt.push_str("\n```\n");

    Some(ExportBatch {
        prompt,
        album_ids,
        total_candidates,
        remaining_after: total_candidates - take,
    })
}

/// Builds a focused follow-up discussion prompt for albums the user flagged as
/// questionable, including full tracklists, previous AI guesses, and user notes.
pub fn export_followup_prompt(lib: &Library, tax: &Taxonomy, pending: &PendingStore) -> Option<(String, usize)> {
    let flagged: Vec<&Proposal> = pending.items.values().filter(|p| p.flagged).collect();
    if flagged.is_empty() {
        return None;
    }
    let by_key: HashMap<&str, &Album> = lib.albums.iter().map(|a| (a.key.as_str(), a)).collect();

    let mut prompt = String::new();
    prompt.push_str("# LocalBeat Genre & Mood Follow-Up Discussion\n\n");
    prompt.push_str(
        "These albums from my library had questionable or ambiguous genre/mood labels. \
         Let's nail down the best classification for each one. \
         Please briefly explain your reasoning for each album, and include an updated ```json code block \
         (same schema as before, keeping each `id`) that I can import straight back into LocalBeat Studio.\n\n",
    );
    prompt.push_str(&tax.prompt_block());
    prompt.push_str("\n### Questionable albums\n\n");

    for p in &flagged {
        let yr = p.year.map(|y| format!(" ({y})")).unwrap_or_default();
        prompt.push_str(&format!("#### {} — {}{} [`id`: `{}`]\n", p.artist, p.album, yr, p.album_id));
        prompt.push_str(&format!(
            "- **Previous proposal**: Broad: `{}` · Subgenres: `{}` · Moods: `{}` (confidence: {})\n",
            p.broad_genre,
            p.subgenres.join(", "),
            p.moods.join(", "),
            p.confidence.label()
        ));
        if !p.ai_note.trim().is_empty() {
            prompt.push_str(&format!("- **Previous AI note**: {}\n", p.ai_note.trim()));
        }
        if !p.user_note.trim().is_empty() {
            prompt.push_str(&format!("- **My note**: {}\n", p.user_note.trim()));
        }
        if !p.old_genres.is_empty() {
            prompt.push_str(&format!("- **Existing file genres**: {}\n", p.old_genres.join(", ")));
        }
        if let Some(a) = by_key.get(p.album_key.as_str()) {
            let tracks = sample_tracks(lib, a, 30);
            if !tracks.is_empty() {
                prompt.push_str("- **Tracks**: ");
                prompt.push_str(&tracks.join(" / "));
                prompt.push('\n');
            }
        }
        prompt.push('\n');
    }
    prompt.push_str(
        "### Output JSON schema (after your brief notes)\n\
         ```json\n\
         {\"albums\": [{\"id\": \"...\", \"broad_genre\": \"...\", \"subgenres\": [...], \"moods\": [...], \"confidence\": \"high\", \"note\": \"...\"}]}\n\
         ```\n",
    );
    Some((prompt, flagged.len()))
}

// ---------------------------------------------------------------- tolerant import

#[derive(Debug, Deserialize)]
struct RawResponse {
    albums: Vec<RawAlbum>,
}

#[derive(Debug, Deserialize)]
struct RawAlbum {
    #[serde(default)]
    id: String,
    #[serde(default)]
    artist: String,
    #[serde(default)]
    album: String,
    #[serde(default)]
    broad_genre: String,
    #[serde(default)]
    subgenres: Vec<String>,
    #[serde(default)]
    moods: Vec<String>,
    #[serde(default)]
    confidence: String,
    #[serde(default)]
    note: String,
}

#[derive(Debug, Default, Clone)]
pub struct ImportSummary {
    pub added: usize,
    pub updated: usize,
    pub unmatched: Vec<String>,
    pub warning_count: usize,
}

/// Extracts JSON from raw text, markdown fenced code blocks, or prose wrapping a
/// JSON object/array.
fn extract_raw_albums(input: &str) -> Result<Vec<RawAlbum>> {
    let candidates = json_candidates(input);
    let mut last_err = None;
    for c in candidates {
        if let Ok(r) = serde_json::from_str::<RawResponse>(c) {
            return Ok(r.albums);
        }
        if let Ok(arr) = serde_json::from_str::<Vec<RawAlbum>>(c) {
            return Ok(arr);
        }
        // Keep the most useful parse error.
        if let Err(e) = serde_json::from_str::<RawResponse>(c) {
            last_err = Some(e);
        }
    }
    match last_err {
        Some(e) => bail!("couldn't parse AI JSON response: {e}"),
        None => bail!("no JSON block found in the pasted response"),
    }
}

fn json_candidates(input: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let trimmed = input.trim();
    if !trimmed.is_empty() {
        out.push(trimmed);
    }
    // Fenced blocks (prefer the last one, since follow-up replies often put prose then JSON).
    let mut fenced = Vec::new();
    let mut rest = input;
    while let Some(start) = rest.find("```") {
        let after = &rest[start + 3..];
        let Some(nl) = after.find('\n') else { break };
        let body = &after[nl + 1..];
        let Some(end) = body.find("```") else { break };
        let block = body[..end].trim();
        if block.starts_with('{') || block.starts_with('[') {
            fenced.push(block);
        }
        rest = &body[end + 3..];
    }
    fenced.reverse();
    out.extend(fenced);
    // Fallback: outermost {...} or [...] slice.
    if let (Some(a), Some(b)) = (input.find('{'), input.rfind('}')) {
        if a < b {
            out.push(&input[a..=b]);
        }
    }
    if let (Some(a), Some(b)) = (input.find('['), input.rfind(']')) {
        if a < b {
            out.push(&input[a..=b]);
        }
    }
    out
}

/// Parses an AI response and merges proposals into `pending`.
pub fn import_response(
    text: &str,
    lib: &Library,
    tax: &Taxonomy,
    pending: &mut PendingStore,
) -> Result<ImportSummary> {
    let raw = extract_raw_albums(text)?;
    if raw.is_empty() {
        bail!("the JSON contained 0 albums");
    }
    let by_id: HashMap<String, &Album> = lib.albums.iter().map(|a| (album_id(&a.key), a)).collect();
    let by_name: HashMap<(String, String), &Album> =
        lib.albums.iter().map(|a| ((norm(&a.artist), norm(&a.title)), a)).collect();

    let mut summary = ImportSummary::default();
    for r in raw {
        let matched = by_id
            .get(&r.id.trim().to_ascii_lowercase())
            .copied()
            .or_else(|| {
                (!r.artist.is_empty() && !r.album.is_empty())
                    .then(|| by_name.get(&(norm(&r.artist), norm(&r.album))).copied())
                    .flatten()
            });
        let Some(album) = matched else {
            let label = if !r.artist.is_empty() || !r.album.is_empty() {
                format!("{} ({} — {})", r.id, r.artist, r.album)
            } else {
                r.id.clone()
            };
            summary.unmatched.push(label);
            continue;
        };
        let id = album_id(&album.key);
        let prev = pending.items.get(&id);
        let mut p = Proposal {
            album_id: id.clone(),
            album_key: album.key.clone(),
            artist: album.artist.clone(),
            album: album.title.clone(),
            year: album.year,
            old_genres: album.genres.clone(),
            old_moods: album.moods.clone(),
            broad_genre: r.broad_genre.trim().to_string(),
            subgenres: r.subgenres.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
            moods: r.moods.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
            confidence: Confidence::parse(&r.confidence),
            ai_note: r.note.trim().to_string(),
            warnings: Vec::new(),
            // Re-importing clears `flagged` so the refined result is ready to review/apply.
            flagged: false,
            user_note: prev.map(|x| x.user_note.clone()).unwrap_or_default(),
        };
        p.revalidate(tax);
        if !p.warnings.is_empty() {
            summary.warning_count += 1;
        }
        if prev.is_some() {
            summary.updated += 1;
        } else {
            summary.added += 1;
        }
        pending.items.insert(id, p);
    }
    Ok(summary)
}

/// Saves a prompt to `<config>/ai-enrich/exports/<prefix>-<timestamp>.md`.
pub fn save_prompt_file(config_dir: &Path, prefix: &str, prompt: &str) -> Result<PathBuf> {
    let dir = PendingStore::exports_dir(config_dir);
    std::fs::create_dir_all(&dir)?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let path = dir.join(format!("{prefix}-{stamp}.md"));
    std::fs::write(&path, prompt)?;
    Ok(path)
}

// ---------------------------------------------------------------- reading & writing file tags

fn parse_relaxed() -> ParseOptions {
    ParseOptions::new().parsing_mode(ParsingMode::Relaxed)
}

fn mp4_freeform(name: &'static str) -> AtomIdent<'static> {
    AtomIdent::Freeform {
        mean: Cow::Borrowed("com.apple.iTunes"),
        name: Cow::Borrowed(name),
    }
}

fn split_multi(raw: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out = Vec::new();
    for s in raw {
        for part in s.split([';', '\0']) {
            let p = part.trim();
            if !p.is_empty() && !out.iter().any(|x: &String| x.eq_ignore_ascii_case(p)) {
                out.push(p.to_string());
            }
        }
    }
    out
}

/// Reads multi-value `MOOD` from any tag type lofty supports.
pub fn read_moods(tag: &Tag) -> Vec<String> {
    split_multi(tag.get_strings(ItemKey::Mood).map(str::to_string))
}

/// Reads `LOCALBEAT_AI_ENRICHED` directly from the format's native tag (lofty's
/// generic `Tag` discards custom keys that aren't in `ItemKey`).
pub fn read_ai_enriched_path(path: &Path, file_type: FileType) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let opts = parse_relaxed();
    match file_type {
        FileType::Flac => {
            let flac = lofty::flac::FlacFile::read_from(&mut f, opts).ok()?;
            flac.vorbis_comments()?.get(AI_ENRICHED_KEY).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
        }
        FileType::Vorbis => {
            let v = VorbisFile::read_from(&mut f, opts).ok()?;
            v.vorbis_comments().get(AI_ENRICHED_KEY).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
        }
        FileType::Opus => {
            let o = OpusFile::read_from(&mut f, opts).ok()?;
            o.vorbis_comments().get(AI_ENRICHED_KEY).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
        }
        FileType::Mpeg => {
            let m = MpegFile::read_from(&mut f, opts).ok()?;
            let id3 = m.id3v2()?;
            id3.iter().find_map(|frame| match frame {
                Frame::UserText(u) if u.description.eq_ignore_ascii_case(AI_ENRICHED_KEY) => {
                    let t = u.content.trim();
                    (!t.is_empty()).then(|| t.to_string())
                }
                _ => None,
            })
        }
        FileType::Mp4 => {
            let m = Mp4File::read_from(&mut f, opts).ok()?;
            let ilst = m.ilst()?;
            let atom = ilst.get(&mp4_freeform(AI_ENRICHED_KEY))?;
            atom.data().find_map(|d| match d {
                AtomData::UTF8(s) | AtomData::UTF16(s) => {
                    let t = s.trim();
                    (!t.is_empty()).then(|| t.to_string())
                }
                _ => None,
            })
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OldFileTags {
    pub genres: Vec<String>,
    pub moods: Vec<String>,
    pub ai_enriched: Option<String>,
}

/// Writes GENRE, MOOD and LOCALBEAT_AI_ENRICHED into `path` using the format's
/// native tag (so custom tags like ACCURATERIP/CDTOC are never lost), preserving
/// `mtime`. Returns the file's previous values for undo.
pub fn write_tags_to_file(
    path: &Path,
    genres: &[String],
    moods: &[String],
    ai_enriched: Option<&str>,
) -> Result<OldFileTags> {
    let orig_mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    let probe = lofty::probe::Probe::open(path)
        .with_context(|| format!("opening {}", path.display()))?
        .guess_file_type()
        .with_context(|| format!("detecting format of {}", path.display()))?;
    let ft = probe.file_type().ok_or_else(|| anyhow::anyhow!("unsupported audio format: {}", path.display()))?;

    let clean_genres: Vec<String> = genres.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    let clean_moods: Vec<String> = moods.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    let clean_marker = ai_enriched.map(str::trim).filter(|s| !s.is_empty());

    let old = match ft {
        FileType::Flac => {
            let mut flac = {
                let mut f = std::fs::File::open(path)?;
                lofty::flac::FlacFile::read_from(&mut f, parse_relaxed())
                    .with_context(|| format!("reading FLAC {}", path.display()))?
            };
            if flac.vorbis_comments().is_none() {
                flac.set_vorbis_comments(VorbisComments::new());
            }
            let vc = flac.vorbis_comments_mut().unwrap();
            let old = edit_vorbis(vc, &clean_genres, &clean_moods, clean_marker);
            flac.save_to_path(path, WriteOptions::default())
                .with_context(|| format!("saving FLAC {}", path.display()))?;
            old
        }
        FileType::Vorbis => {
            let mut v = {
                let mut f = std::fs::File::open(path)?;
                VorbisFile::read_from(&mut f, parse_relaxed())
                    .with_context(|| format!("reading Ogg Vorbis {}", path.display()))?
            };
            let old = edit_vorbis(v.vorbis_comments_mut(), &clean_genres, &clean_moods, clean_marker);
            v.save_to_path(path, WriteOptions::default())
                .with_context(|| format!("saving Ogg Vorbis {}", path.display()))?;
            old
        }
        FileType::Opus => {
            let mut o = {
                let mut f = std::fs::File::open(path)?;
                OpusFile::read_from(&mut f, parse_relaxed())
                    .with_context(|| format!("reading Opus {}", path.display()))?
            };
            let old = edit_vorbis(o.vorbis_comments_mut(), &clean_genres, &clean_moods, clean_marker);
            o.save_to_path(path, WriteOptions::default())
                .with_context(|| format!("saving Opus {}", path.display()))?;
            old
        }
        FileType::Mpeg => {
            let mut mpeg = {
                let mut f = std::fs::File::open(path)?;
                MpegFile::read_from(&mut f, parse_relaxed())
                    .with_context(|| format!("reading MP3 {}", path.display()))?
            };
            if mpeg.id3v2().is_none() {
                mpeg.set_id3v2(Id3v2Tag::new());
            }
            let id3 = mpeg.id3v2_mut().unwrap();
            fix_id3_languages_in_place(id3);
            let old = edit_id3(id3, &clean_genres, &clean_moods, clean_marker);
            save_id3_mp3(id3, path)
                .with_context(|| format!("saving MP3 {}", path.display()))?;
            old
        }
        FileType::Mp4 => {
            let mut mp4 = {
                let mut f = std::fs::File::open(path)?;
                Mp4File::read_from(&mut f, parse_relaxed())
                    .with_context(|| format!("reading MP4 {}", path.display()))?
            };
            if mp4.ilst().is_none() {
                mp4.set_ilst(Ilst::new());
            }
            let ilst = mp4.ilst_mut().unwrap();
            let old = edit_mp4(ilst, &clean_genres, &clean_moods, clean_marker);
            ilst.save_to_path(path, WriteOptions::default())
                .with_context(|| format!("saving MP4 {}", path.display()))?;
            old
        }
        other => bail!("retagging {other:?} is not supported ({})", path.display()),
    };

    if let Some(mtime) = orig_mtime {
        let _ = std::fs::File::options().write(true).open(path).and_then(|f| f.set_modified(mtime));
    }
    Ok(old)
}

fn edit_vorbis(vc: &mut VorbisComments, genres: &[String], moods: &[String], marker: Option<&str>) -> OldFileTags {
    let old = OldFileTags {
        genres: split_multi(vc.get_all("GENRE").map(str::to_string)),
        moods: split_multi(vc.get_all("MOOD").map(str::to_string)),
        ai_enriched: vc.get(AI_ENRICHED_KEY).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string),
    };
    let _ = vc.remove("GENRE");
    for g in genres {
        vc.push("GENRE".to_string(), g.clone());
    }
    let _ = vc.remove("MOOD");
    for m in moods {
        vc.push("MOOD".to_string(), m.clone());
    }
    let _ = vc.remove(AI_ENRICHED_KEY);
    if let Some(val) = marker {
        vc.insert(AI_ENRICHED_KEY.to_string(), val.to_string());
    }
    old
}

fn edit_id3(id3: &mut Id3v2Tag, genres: &[String], moods: &[String], marker: Option<&str>) -> OldFileTags {
    let old_genres = id3
        .genres()
        .map(|it| split_multi(it.map(str::to_string)))
        .unwrap_or_default();
    let mood_id = FrameId::Valid(Cow::Borrowed("TMOO"));
    let old_moods = id3
        .get_text(&mood_id)
        .map(|s| split_multi([s.to_string()]))
        .unwrap_or_default();
    let old_ai = id3.iter().find_map(|frame| match frame {
        Frame::UserText(u) if u.description.eq_ignore_ascii_case(AI_ENRICHED_KEY) => {
            let t = u.content.trim();
            (!t.is_empty()).then(|| t.to_string())
        }
        _ => None,
    });

    let genre_id = FrameId::Valid(Cow::Borrowed("TCON"));
    let _ = id3.remove(&genre_id);
    if !genres.is_empty() {
        id3.insert(Frame::Text(TextInformationFrame::new(genre_id, TextEncoding::UTF8, genres.join("\0"))));
    }

    let _ = id3.remove(&mood_id);
    if !moods.is_empty() {
        id3.insert(Frame::Text(TextInformationFrame::new(mood_id, TextEncoding::UTF8, moods.join("\0"))));
    }

    id3.retain(|f| !matches!(f, Frame::UserText(u) if u.description.eq_ignore_ascii_case(AI_ENRICHED_KEY)));
    if let Some(val) = marker {
        id3.insert(Frame::UserText(ExtendedTextFrame::new(
            TextEncoding::UTF8,
            AI_ENRICHED_KEY.to_string(),
            val.to_string(),
        )));
    }
    OldFileTags { genres: old_genres, moods: old_moods, ai_enriched: old_ai }
}

fn edit_mp4(ilst: &mut Ilst, genres: &[String], moods: &[String], marker: Option<&str>) -> OldFileTags {
    let mood_ident = mp4_freeform("MOOD");
    let ai_ident = mp4_freeform(AI_ENRICHED_KEY);
    let read_atom = |ilst: &Ilst, ident: &AtomIdent<'_>| -> Vec<String> {
        ilst.get(ident)
            .map(|a| {
                a.data()
                    .filter_map(|d| match d {
                        AtomData::UTF8(s) | AtomData::UTF16(s) => Some(s.clone()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let old = OldFileTags {
        genres: ilst.genre().map(|g| split_multi([g.into_owned()])).unwrap_or_default(),
        moods: split_multi(read_atom(ilst, &mood_ident)),
        ai_enriched: read_atom(ilst, &ai_ident).into_iter().map(|s| s.trim().to_string()).find(|s| !s.is_empty()),
    };

    if genres.is_empty() {
        ilst.remove_genre();
    } else {
        ilst.set_genre(genres.join("; "));
    }
    let _ = ilst.remove(&mood_ident);
    if !moods.is_empty() {
        ilst.replace_atom(Atom::new(mood_ident, AtomData::UTF8(moods.join("; "))));
    }
    let _ = ilst.remove(&ai_ident);
    if let Some(val) = marker {
        ilst.replace_atom(Atom::new(ai_ident, AtomData::UTF8(val.to_string())));
    }
    old
}

fn fix_id3_languages_in_place(id3: &mut Id3v2Tag) {
    let valid = |l: &[u8; 3]| l.iter().all(|b| b.is_ascii_alphabetic());
    id3.retain_mut(|frame| {
        match frame {
            Frame::Comment(c) if !valid(&c.language) => c.language = lofty::tag::items::UNKNOWN_LANGUAGE,
            Frame::UnsynchronizedText(u) if !valid(&u.language) => u.language = lofty::tag::items::UNKNOWN_LANGUAGE,
            _ => {}
        }
        true
    });
}

pub(crate) fn save_id3_mp3(id3: &Id3v2Tag, path: &Path) -> Result<()> {
    if id3.save_to_path(path, WriteOptions::default()).is_ok() {
        return Ok(());
    }
    let orig = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut off = 0usize;
    while off + 10 <= orig.len() && &orig[off..off + 3] == b"ID3" {
        let ver = orig[off + 3];
        let flags = orig[off + 5];
        let b = &orig[off + 6..off + 10];
        if b.iter().any(|&x| x & 0x80 != 0) {
            break;
        }
        let body = ((b[0] as usize) << 21)
            | ((b[1] as usize) << 14)
            | ((b[2] as usize) << 7)
            | (b[3] as usize);
        let footer = if ver >= 4 && (flags & 0x10 != 0) { 10 } else { 0 };
        let next = off.saturating_add(10).saturating_add(body).saturating_add(footer);
        if next > orig.len() {
            break;
        }
        off = next;
    }
    let mut out = Vec::with_capacity(orig.len() + 4096);
    id3.dump_to(&mut out, WriteOptions::default())
        .with_context(|| format!("encoding ID3v2 tag for {}", path.display()))?;
    out.extend_from_slice(&orig[off..]);
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("track.mp3");
    let tmp = parent.join(format!(".{name}.retag.tmp"));
    let orig_perms = std::fs::metadata(path).map(|m| m.permissions()).ok();
    std::fs::write(&tmp, &out).with_context(|| format!("writing {}", tmp.display()))?;
    if let Some(perms) = orig_perms {
        let _ = std::fs::set_permissions(&tmp, perms);
    }
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{Origin, ScanStats, Song};

    fn test_lib() -> Library {
        let make = |artist: &str, album: &str, title: &str, year: u16, genres: &[&str], ai: Option<&str>| Song {
            path: PathBuf::from(format!("/m/{artist}/{album}/{title}.flac")),
            size: 100,
            mtime: 1000,
            ctime: 0,
            ctime_nsec: 0,
            tag_schema: 1,
            added: 1000,
            title: title.into(),
            artist: artist.into(),
            album: album.into(),
            album_artist: artist.into(),
            track: Some(1),
            disc: Some(1),
            year: Some(year),
            genre: genres.first().map(|s| s.to_string()),
            genres: genres.iter().map(|s| s.to_string()).collect(),
            moods: Vec::new(),
            ai_enriched: ai.map(str::to_string),
            seconds: 200.0,
            codec: "FLAC".into(),
            sample_rate: Some(44100),
            bits: Some(16),
            bitrate: None,
            drm: false,
            origin: Origin::Flac,
            picture: false,
            server_path: Some(format!("{artist}/{album}/{title}.flac")),
        };
        let songs = vec![
            make("10,000 Maniacs", "In My Tribe", "What's the Matter Here?", 1987, &["Rock"], None),
            make("Guns N' Roses", "Appetite for Destruction", "Welcome to the Jungle", 1987, &["Rock"], None),
            make("Miles Davis", "Kind of Blue", "So What", 1959, &["Jazz", "Cool & Modal Jazz"], Some("v1")),
        ];
        Library::build(songs, &mut ScanStats::default())
    }

    #[test]
    fn export_skips_enriched_and_chunks() {
        let lib = test_lib();
        let tax = Taxonomy::default_v1();
        let pending = PendingStore::default();
        let st = stats(&lib, &tax, &pending);
        assert_eq!((st.total_albums, st.enriched_current, st.remaining_to_export), (3, 1, 2));

        let batch = export_batch_prompt(&lib, &tax, &pending, &ExportOptions { batch_size: 1, ..Default::default() }).unwrap();
        assert_eq!(batch.album_ids.len(), 1);
        assert_eq!(batch.total_candidates, 2);
        assert_eq!(batch.remaining_after, 1);
        assert!(batch.prompt.contains("Allowed Broad Genres & Subgenres"));
        assert!(batch.prompt.contains("Allowed Moods"));
        assert!(!batch.prompt.contains("Kind of Blue"));
    }

    #[test]
    fn tolerant_import_validates_and_followup_merges() {
        let lib = test_lib();
        let tax = Taxonomy::default_v1();
        let mut pending = PendingStore::default();
        let maniacs_id = album_id(&lib.albums.iter().find(|a| a.title == "In My Tribe").unwrap().key);
        let gnr_id = album_id(&lib.albums.iter().find(|a| a.title == "Appetite for Destruction").unwrap().key);

        let ai_reply = format!(
            "Sure! Here are your albums:\n\n```json\n{{\n  \"albums\": [\n    {{\"id\": \"{maniacs_id}\", \"broad_genre\": \"alternative & indie\", \"subgenres\": [\"jangle pop\", \"College Rock\"], \"moods\": [\"introspective\", \"Acoustic\"], \"confidence\": \"high\", \"note\": \"\"}},\n    {{\"id\": \"{gnr_id}\", \"broad_genre\": \"Rock\", \"subgenres\": [\"Hard Rock\", \"Made Up Genre\"], \"moods\": [\"Energetic\", \"Gritty\"], \"confidence\": \"medium\", \"note\": \"Borderline metal\"}}\n  ]\n}}\n```\nHope that helps!"
        );
        let sum = import_response(&ai_reply, &lib, &tax, &mut pending).unwrap();
        assert_eq!((sum.added, sum.updated, sum.unmatched.len(), sum.warning_count), (2, 0, 0, 1));

        let m = &pending.items[&maniacs_id];
        assert_eq!(m.broad_genre, "Alternative & Indie");
        assert_eq!(m.subgenres, ["Jangle Pop", "College Rock"]);
        assert_eq!(m.moods, ["Introspective", "Acoustic"]);
        assert!(m.warnings.is_empty());
        assert_eq!(m.final_genres(), ["Alternative & Indie", "Jangle Pop", "College Rock"]);

        // Flag GNR for follow-up discussion.
        let g = pending.items.get_mut(&gnr_id).unwrap();
        assert!(!g.warnings.is_empty());
        g.flagged = true;
        g.user_note = "Keep it Hard Rock + Glam Metal".into();

        let (followup, n) = export_followup_prompt(&lib, &tax, &pending).unwrap();
        assert_eq!(n, 1);
        assert!(followup.contains("Welcome to the Jungle"));
        assert!(followup.contains("Keep it Hard Rock + Glam Metal"));

        // Re-import refined JSON from the follow-up discussion.
        let refined = format!(
            "Agreed! Updated JSON:\n```json\n{{\"albums\":[{{\"id\":\"{gnr_id}\",\"broad_genre\":\"Rock\",\"subgenres\":[\"Hard Rock\",\"Glam Metal\"],\"moods\":[\"Energetic\",\"Gritty\",\"Driving\"],\"confidence\":\"high\",\"note\":\"Refined\"}}]}}\n```"
        );
        let sum2 = import_response(&refined, &lib, &tax, &mut pending).unwrap();
        assert_eq!((sum2.added, sum2.updated, sum2.warning_count), (0, 1, 0));
        let g2 = &pending.items[&gnr_id];
        assert!(!g2.flagged);
        assert!(g2.warnings.is_empty());
        assert_eq!(g2.subgenres, ["Hard Rock", "Glam Metal"]);
    }

    #[test]
    fn write_tags_on_flac_preserves_custom_tags_and_mtime() {
        let dir = std::env::temp_dir().join(format!("localbeat-ai-flac-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("01. Track.flac");
        let frames = vec![0u32; 4410];
        let tags = vec![
            ("TITLE".into(), "What's the Matter Here?".into()),
            ("ARTIST".into(), "10,000 Maniacs".into()),
            ("ALBUM".into(), "In My Tribe".into()),
            ("GENRE".into(), "Rock".into()),
            ("ACCURATERIP".into(), "v2 200".into()),
        ];
        crate::flac_encode::write_flac(&file, &frames, &tags, None, 0).unwrap();
        let orig_mtime = std::fs::metadata(&file).unwrap().modified().unwrap();

        let genres = vec!["Alternative & Indie".into(), "Jangle Pop".into(), "College Rock".into()];
        let moods = vec!["Introspective".into(), "Acoustic".into()];
        let old = write_tags_to_file(&file, &genres, &moods, Some("v1")).unwrap();
        assert_eq!(old.genres, ["Rock"]);
        assert!(old.moods.is_empty());
        assert_eq!(old.ai_enriched, None);

        // mtime preserved.
        assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), orig_mtime);

        // Tags read back cleanly and ACCURATERIP was NOT lost.
        let flac = crate::flacfile::read_tags(&file).unwrap();
        assert_eq!(flac.tag("ACCURATERIP"), Some("v2 200"));
        assert_eq!(flac.tag(AI_ENRICHED_KEY), Some("v1"));
        assert_eq!(read_ai_enriched_path(&file, FileType::Flac).as_deref(), Some("v1"));

        let tf = lofty::read_from_path(&file).unwrap();
        let t = tf.primary_tag().unwrap();
        assert_eq!(t.get_strings(ItemKey::Genre).collect::<Vec<_>>(), ["Alternative & Indie", "Jangle Pop", "College Rock"]);
        assert_eq!(read_moods(t), ["Introspective", "Acoustic"]);

        // Undo restores old tags and clears LOCALBEAT_AI_ENRICHED.
        write_tags_to_file(&file, &old.genres, &old.moods, old.ai_enriched.as_deref()).unwrap();
        assert_eq!(read_ai_enriched_path(&file, FileType::Flac), None);
        let tf2 = lofty::read_from_path(&file).unwrap();
        assert_eq!(tf2.primary_tag().unwrap().get_strings(ItemKey::Genre).collect::<Vec<_>>(), ["Rock"]);
        assert!(read_moods(tf2.primary_tag().unwrap()).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
