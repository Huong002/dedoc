// Phase 1: English -> Vietnamese translation for `open` (and `search --open`).
//
// Providers are free and keyless:
//   1. Google GTX (`translate.googleapis.com`, `client=gtx`) — primary.
//   2. Lingva (`lingva.ml`) — fallback when Google fails.
//
// Anything that looks like code (URLs, emails, inline code, identifiers,
// paths, generics, CLI flags) is hidden behind `__PH_{i}__` placeholders
// before translation and restored afterwards, so translators cannot mangle
// it. Translation never panics and never fails the caller: on any error the
// source text is kept and a one-time warning is printed to stderr.

use std::collections::{
  HashMap,
  HashSet,
};
use std::fs::{
  File,
  create_dir_all,
};
use std::io::{
  BufReader,
  BufWriter,
};
use std::sync::OnceLock;
use std::sync::atomic::{
  AtomicBool,
  Ordering,
};
use std::time::Duration;

use regex::Regex;
use serde::{
  Deserialize,
  Serialize,
};

use crate::common::{
  get_default_user_agent,
  get_program_directory,
};

// ---------------------------------------------------------------------------
// Config + errors
// ---------------------------------------------------------------------------

pub(crate) struct TranslationConfig
{
  pub target_lang: String,
  pub provider: String,
  pub no_cache: bool,
}

impl TranslationConfig
{
  pub(crate) fn new(target_lang: &str, provider: &str, no_cache: bool) -> Self
  {
    Self { target_lang: target_lang.to_owned(),
           provider: provider.to_owned(),
           no_cache }
  }
}

#[derive(Debug)]
pub(crate) enum TranslationError
{
  Network(String),
  Parse(String),
  RateLimited,
  PlaceholderMismatch,
}

impl std::fmt::Display for TranslationError
{
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result
  {
    match self {
      Self::Network(msg) => write!(f, "network error: {msg}"),
      Self::Parse(msg) => write!(f, "parse error: {msg}"),
      Self::RateLimited => write!(f, "rate limited (HTTP 429)"),
      Self::PlaceholderMismatch => write!(f, "placeholder mismatch after translation"),
    }
  }
}

// ---------------------------------------------------------------------------
// Providers
// ---------------------------------------------------------------------------

pub(crate) trait TranslationProvider
{
  fn name(&self) -> &str;
  fn translate(&self,
               texts: &[String],
               sl: &str,
               tl: &str)
               -> Result<Vec<String>, TranslationError>;
}

fn is_rate_limited_error(err: &ureq::Error) -> bool
{
  match err {
    ureq::Error::Status(429, _) => true,
    ureq::Error::Status(_, response) => response.status() == 429,
    _ => {
      let msg = err.to_string();
      msg.contains("429") || msg.contains("Too Many Requests")
    }
  }
}

fn ureq_agent() -> ureq::Agent
{
  ureq::AgentBuilder::new().timeout(Duration::from_secs(10)).build()
}

// Long inputs are split on sentence boundaries so no single request exceeds
// ~3500 characters (Google GTX rejects oversized `q` params).
// Manual split (no regex lookbehind — unsupported by the `regex` crate):
// cut after `.`/`!`/`?` followed by whitespace-or-end, or at newlines.
fn split_long_text(text: &str, max_chars: usize) -> Vec<String>
{
  if text.chars().count() <= max_chars {
    return vec![text.to_owned()];
  }

  // 1. Cut into sentence-ish pieces.
  let chars: Vec<char> = text.chars().collect();
  let mut pieces: Vec<String> = vec![];
  let mut buf = String::new();
  let mut i = 0usize;
  while i < chars.len() {
    let c = chars[i];
    buf.push(c);
    if c == '\n' {
      // Coalesce consecutive newlines into one boundary.
      while i + 1 < chars.len() && chars[i + 1] == '\n' {
        i += 1;
        buf.push(chars[i]);
      }
      if !buf.trim().is_empty() {
        pieces.push(buf.trim().to_owned());
      }
      buf.clear();
    } else if (c == '.' || c == '!' || c == '?')
           && (i + 1 >= chars.len() || chars[i + 1].is_whitespace())
    {
      // Consume trailing spaces/tabs (newlines handled above on next iter).
      while i + 1 < chars.len() && chars[i + 1].is_whitespace() && chars[i + 1] != '\n' {
        i += 1;
        buf.push(chars[i]);
      }
      if !buf.trim().is_empty() {
        pieces.push(buf.trim().to_owned());
      }
      buf.clear();
    }
    i += 1;
  }
  if !buf.trim().is_empty() {
    pieces.push(buf.trim().to_owned());
  }

  // No boundary found (one giant run): hard-split by char count.
  if pieces.is_empty() {
    pieces.push(text.to_owned());
  }

  // 2. Pack pieces into chunks <= max_chars; hard-split oversized pieces.
  let mut chunks: Vec<String> = vec![];
  let mut current = String::new();
  let mut current_len = 0usize;
  for piece in pieces {
    // Hard-split a single oversized piece by chars.
    if piece.chars().count() > max_chars {
      if !current.is_empty() {
        chunks.push(std::mem::take(&mut current));
        current_len = 0;
      }
      let pchars: Vec<char> = piece.chars().collect();
      for window in pchars.chunks(max_chars) {
        chunks.push(window.iter().collect());
      }
      continue;
    }
    let piece_len = piece.chars().count();
    let sep = if current.is_empty() { 0 } else { 1 };
    if current_len + sep + piece_len > max_chars && !current.is_empty() {
      chunks.push(std::mem::take(&mut current));
      current_len = 0;
    }
    if !current.is_empty() {
      current.push(' ');
      current_len += 1;
    }
    current.push_str(&piece);
    current_len += piece_len;
  }
  if !current.is_empty() {
    chunks.push(current);
  }
  if chunks.is_empty() {
    chunks.push(text.to_owned());
  }
  chunks
}

/// Batching: GTX ignores repeated `q` params (only the first is translated),
/// so multiple texts are joined with `\n` into one request (Google preserves
/// newlines) and split back afterwards. At most 8 texts / 3000 chars per
/// batch; a batch whose newline count mismatches falls back to per-text
/// requests (bounded: <= 8 requests).
const BATCH_MAX_TEXTS: usize = 8;
const BATCH_MAX_CHARS: usize = 3000;

fn split_into_batches(texts: &[String]) -> Vec<Vec<usize>>
{
  let mut batches: Vec<Vec<usize>> = vec![];
  let mut current: Vec<usize> = vec![];
  let mut current_len = 0usize;
  for (i, text) in texts.iter().enumerate() {
    let len = text.chars().count();
    if !current.is_empty() &&
       (current.len() >= BATCH_MAX_TEXTS || current_len + 1 + len > BATCH_MAX_CHARS)
    {
      batches.push(std::mem::take(&mut current));
      current_len = 0;
    }
    if !current.is_empty() {
      current_len += 1; // the joining '\n'
    }
    current.push(i);
    current_len += len;
  }
  if !current.is_empty() {
    batches.push(current);
  }
  batches
}

fn gtx_request(agent: &ureq::Agent,
               chunk: &str,
               sl: &str,
               tl: &str)
               -> Result<String, TranslationError>
{
  let url = format!("https://translate.googleapis.com/translate_a/single?client=gtx&sl={sl}&tl={tl}&dt=t&q={}",
                    urlencoding::encode(chunk));
  let response = agent.get(&url)
                      .set("User-Agent", &get_default_user_agent())
                      .call()
                      .map_err(|err| {
                        if is_rate_limited_error(&err) {
                          TranslationError::RateLimited
                        } else {
                          TranslationError::Network(err.to_string())
                        }
                      })?;
  let body = response.into_string()
                     .map_err(|err| TranslationError::Network(err.to_string()))?;
  parse_gtx_body(&body)
}

pub(crate) struct GoogleGtxTranslator;

impl TranslationProvider for GoogleGtxTranslator
{
  fn name(&self) -> &str
  {
    "google-gtx"
  }

  fn translate(&self,
               texts: &[String],
               sl: &str,
               tl: &str)
               -> Result<Vec<String>, TranslationError>
  {
    let agent = ureq_agent();
    let mut out: Vec<String> = vec![String::new(); texts.len()];

    for batch in split_into_batches(texts) {
      // Single-text batches (or long texts): keep old chunked path.
      if batch.len() == 1 {
        let text = &texts[batch[0]];
        let mut translated_chunks = vec![];
        for chunk in split_long_text(text, 3500) {
          translated_chunks.push(gtx_request(&agent, &chunk, sl, tl)?);
        }
        out[batch[0]] = translated_chunks.concat();
        continue;
      }
      // Multi-text batch: join with '\n', one request, split back.
      let joined = batch.iter().map(|&i| texts[i].as_str()).collect::<Vec<_>>().join("\n");
      // The joined blob may exceed per-request limits: chunk it only when huge.
      if joined.chars().count() > 3500 {
        // Fall back to per-text for oversized batches (rare, bounded).
        for &i in &batch {
          let mut translated_chunks = vec![];
          for chunk in split_long_text(&texts[i], 3500) {
            translated_chunks.push(gtx_request(&agent, &chunk, sl, tl)?);
          }
          out[i] = translated_chunks.concat();
        }
        continue;
      }
      let translated = gtx_request(&agent, &joined, sl, tl)?;
      let parts: Vec<&str> = translated.split('\n').collect();
      if parts.len() == batch.len() {
        for (&i, part) in batch.iter().zip(parts.into_iter()) {
          out[i] = part.to_owned();
        }
      } else {
        // Newlines got mangled: bounded per-text retry for this batch.
        for &i in &batch {
          let mut translated_chunks = vec![];
          for chunk in split_long_text(&texts[i], 3500) {
            translated_chunks.push(gtx_request(&agent, &chunk, sl, tl)?);
          }
          out[i] = translated_chunks.concat();
        }
      }
    }

    Ok(out)
  }
}

// GTX answers `[[["translated","source",...],...],null,"en",...]`.
// Concatenate element [0] of every segment in [0].
fn parse_gtx_body(body: &str) -> Result<String, TranslationError>
{
  let json: serde_json::Value =
    serde_json::from_str(body).map_err(|err| TranslationError::Parse(err.to_string()))?;
  let segments =
    json.get(0)
        .and_then(|v| v.as_array())
        .ok_or_else(|| TranslationError::Parse("missing [0] segments".to_string()))?;

  let mut out = String::new();
  for segment in segments {
    if let Some(translated) = segment.get(0).and_then(|v| v.as_str()) {
      out.push_str(translated);
    }
  }
  if out.is_empty() {
    return Err(TranslationError::Parse("empty translation".to_string()));
  }
  Ok(out)
}

pub(crate) struct LingvaTranslator;

impl TranslationProvider for LingvaTranslator
{
  fn name(&self) -> &str
  {
    "lingva"
  }

  fn translate(&self,
               texts: &[String],
               sl: &str,
               tl: &str)
               -> Result<Vec<String>, TranslationError>
  {
    let agent = ureq_agent();
    let mut out = Vec::with_capacity(texts.len());

    for text in texts {
      let mut translated_chunks = vec![];
      for chunk in split_long_text(text, 3500) {
        let url = format!("https://lingva.ml/api/v1/{sl}/{tl}/{}", urlencoding::encode(&chunk));
        let response =
          agent.get(&url)
               .set("User-Agent", &get_default_user_agent())
               .call()
               .map_err(|err| {
                 if is_rate_limited_error(&err) {
                   TranslationError::RateLimited
                 } else {
                   TranslationError::Network(err.to_string())
                 }
               })?;
        let body =
          response.into_string()
                  .map_err(|err| TranslationError::Network(err.to_string()))?;
        let json: serde_json::Value =
          serde_json::from_str(&body).map_err(|err| TranslationError::Parse(err.to_string()))?;
        let translated =
          json.get("translation")
              .and_then(|v| v.as_str())
              .ok_or_else(|| TranslationError::Parse("missing `translation` field".to_string()))?;
        translated_chunks.push(translated.to_owned());
      }
      out.push(translated_chunks.concat());
    }

    Ok(out)
  }
}

// ---------------------------------------------------------------------------
// Placeholder protection
// ---------------------------------------------------------------------------

fn placeholder_regex() -> &'static Regex
{
  static RE: OnceLock<Regex> = OnceLock::new();
  RE.get_or_init(|| {
    Regex::new(
      r"(?x)
        __PH_\d+__                            # existing placeholder token: exempt, never re-protect
      | https?://\S+                          # URLs
      | [\w.+-]+@[\w-]+\.[\w.]+               # emails
      | `[^`]*`                               # inline code
      | --[\w-]+                              # CLI flags
      | \b\w+(?:::\w+)+                       # a::b paths
      | \b[\w$.]+\(\)                         # foo(), obj.method()
      | \b\w+<[\w\s,]+>                       # generics X<Y>
      | [\w.~/-]*/[\w.~/-]+                   # paths with /
      | \b[\w./-]+\.(?:rs|toml|json|js|ts|html|css|md)\b  # filenames
      | \b\w*[a-z][A-Z]\w*\b                  # camelCase / inner-capital words
      | \b[A-Z][a-z]+[A-Z]\w*\b               # PascalCase w/ inner cap
      | \b[A-Z]{2,}[A-Za-z0-9]*\b             # ALLCAPS acronyms
      | \b\w*_\w+\b                           # snake_case
      ",
    )
    .expect("static regex is valid")
  })
}

fn existing_token_regex() -> &'static Regex
{
  static RE: OnceLock<Regex> = OnceLock::new();
  RE.get_or_init(|| Regex::new(r"^__PH_\d+__$").expect("static regex is valid"))
}

fn normalize_token_regex() -> &'static Regex
{
  static RE: OnceLock<Regex> = OnceLock::new();
  // Matches translator-mangled variants: different case, stray spaces.
  RE.get_or_init(|| Regex::new(r"(?i)__\s*ph_\s*(\d+)\s*__").expect("static regex is valid"))
}

/// Replace code-like spans with `__PH_{i}__` tokens.
/// Returns the protected text plus the original spans in order.
pub(crate) fn protect(text: &str) -> (String, Vec<String>)
{
  let re = placeholder_regex();
  let exempt = existing_token_regex();
  let mut placeholders = vec![];
  let protected = re.replace_all(text, |caps: &regex::Captures| {
                  // Exempt: text already containing a placeholder token must
                  // not be wrapped again (no nested protection).
                  if exempt.is_match(&caps[0]) {
                    return caps[0].to_owned();
                  }
                  placeholders.push(caps[0].to_owned());
                  format!("__PH_{}__", placeholders.len() - 1)
                })
                .into_owned();
  (protected, placeholders)
}

/// Restore placeholders. Errors when the translation dropped a token.
pub(crate) fn restore(translated: &str,
                      placeholders: &[String])
                      -> Result<String, TranslationError>
{
  // Normalize translator-mangled token variants (`__ ph_0 __`, different
  // case, stray spaces) back to canonical `__PH_{n}__` before restoring.
  let norm = normalize_token_regex();
  let mut out =
    norm.replace_all(translated, |caps: &regex::Captures| {
            format!("__PH_{}__", &caps[1])
          })
        .into_owned();
  for (i, original) in placeholders.iter().enumerate() {
    let token = format!("__PH_{i}__");
    if !out.contains(token.as_str()) {
      return Err(TranslationError::PlaceholderMismatch);
    }
    out = out.replace(token.as_str(), original);
  }
  Ok(out)
}

// Text without a single letter is not worth an HTTP request.
fn looks_translatable(text: &str) -> bool
{
  let trimmed = text.trim();
  trimmed.len() >= 2 && trimmed.chars().any(|c| c.is_alphabetic())
}

// ---------------------------------------------------------------------------
// Disk cache: ~/.dedoc/translated/<docset>/<page_hash>.json
// ---------------------------------------------------------------------------

const CACHE_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct CacheUnit
{
  source_hash: String,
  source: String,
  translated: String,
}

#[derive(Serialize, Deserialize)]
struct TranslationCache
{
  version: u32,
  source_lang: String,
  target_lang: String,
  provider: String,
  glossary_hash: String,
  units: Vec<CacheUnit>,
}

/// Stable FNV-1a 64-bit hash (deterministic across processes).
/// `DefaultHasher` uses a random seed per process, so it must NOT be used
/// for on-disk cache keys.
pub(crate) fn hash_text(text: &str) -> String
{
  const OFFSET_BASIS: u64 = 14695981039346656037;
  const PRIME: u64 = 1099511628211;
  let mut hash = OFFSET_BASIS;
  for byte in text.as_bytes() {
    hash ^= u64::from(*byte);
    hash = hash.wrapping_mul(PRIME);
  }
  format!("{hash:016x}")
}

/// Strip anything outside `[a-zA-Z0-9_-]` so a hostile docset name
/// (`../evil`) cannot escape the cache directory via path traversal.
fn sanitize_docset(docset: &str) -> String
{
  let cleaned: String =
    docset.chars()
          .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
          .collect();
  if cleaned.is_empty() {
    "unknown".to_owned()
  } else {
    cleaned
  }
}

fn cache_path(docset: &str, page_id: &str) -> Result<std::path::PathBuf, String>
{
  Ok(get_program_directory()?.join("translated")
                              .join(sanitize_docset(docset))
                              .join(format!("{}.json", hash_text(page_id))))
}

fn read_cache(docset: &str,
              page_id: &str,
              config: &TranslationConfig)
              -> Option<TranslationCache>
{
  let path = cache_path(docset, page_id).ok()?;
  let file = File::open(&path).ok()?;
  let cache: TranslationCache = serde_json::from_reader(BufReader::new(file)).ok()?;
  if cache.version != CACHE_VERSION
     || cache.target_lang != config.target_lang
     || cache.provider != config.provider
     || cache.source_lang != "en"
  {
    return None;
  }
  Some(cache)
}

fn write_cache(docset: &str, page_id: &str, cache: &TranslationCache)
{
  let Ok(path) = cache_path(docset, page_id) else {
    return;
  };
  if let Some(parent) = path.parent() {
    if create_dir_all(parent).is_err() {
      return;
    }
  }
  if let Ok(file) = File::create(&path) {
    let _ = serde_json::to_writer(BufWriter::new(file), cache);
  }
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

static WARNED_ONCE: AtomicBool = AtomicBool::new(false);

fn warn_once(message: &str)
{
  if !WARNED_ONCE.swap(true, Ordering::Relaxed) {
    eprintln!("WARNING: {message}");
  }
}

fn pick_provider(config: &TranslationConfig) -> Box<dyn TranslationProvider>
{
  match config.provider.as_str() {
    "lingva" => Box::new(LingvaTranslator),
    _ => Box::new(GoogleGtxTranslator),
  }
}

/// Translate texts, never failing: on any error the source text is kept and
/// a single stderr warning is emitted. Never panics.
pub(crate) fn translate_texts(texts: Vec<String>, config: &TranslationConfig) -> Vec<String>
{
  if texts.is_empty() {
    return vec![];
  }

  // Protect code spans; remember which inputs are worth sending.
  // Only sendable texts go to the provider (empty/non-letter inputs stay local).
  let mut filtered: Vec<String> = Vec::new();
  let mut filtered_placeholders: Vec<Vec<String>> = Vec::new();
  let mut filtered_idx: Vec<usize> = Vec::new();
  let mut sendable: Vec<bool> = vec![false; texts.len()];
  for (i, text) in texts.iter().enumerate() {
    if looks_translatable(text) {
      let (protected, placeholders) = protect(text);
      filtered.push(protected);
      filtered_placeholders.push(placeholders);
      filtered_idx.push(i);
      sendable[i] = true;
    }
  }

  if filtered.is_empty() {
    return texts;
  }

  let primary = pick_provider(config);
  let translated_filtered: Vec<String> =
    match primary.translate(&filtered, "en", &config.target_lang) {
    Ok(v) => v,
    Err(err) => {
      let fallback: Box<dyn TranslationProvider> = if primary.name() == "lingva" {
        Box::new(GoogleGtxTranslator)
      } else {
        Box::new(LingvaTranslator)
      };
      match fallback.translate(&filtered, "en", &config.target_lang) {
        Ok(v) => v,
        Err(fallback_err) => {
          warn_once(&format!("translation via {} failed ({err}); fallback {} failed \
                              ({fallback_err}); showing original English text.",
                             primary.name(),
                             fallback.name()));
          return texts;
        }
      }
    }
  };

  // Restore placeholders; keep source for anything that fails.
  let mut restored_map: std::collections::HashMap<usize, String> =
    std::collections::HashMap::new();
  for (pos, raw) in translated_filtered.into_iter().enumerate() {
    let orig_i = filtered_idx[pos];
    let placeholders = &filtered_placeholders[pos];
    if placeholders.is_empty() {
      restored_map.insert(orig_i, raw);
    } else {
      match restore(&raw, placeholders) {
        Ok(restored) => {
          restored_map.insert(orig_i, restored);
        }
        Err(_) => {
          warn_once("translation dropped code placeholders; showing original text for a line.");
          restored_map.insert(orig_i, texts[orig_i].clone());
        }
      }
    }
  }
  texts.into_iter()
       .enumerate()
       .map(|(i, original)| restored_map.remove(&i).unwrap_or(original))
       .collect()
}

/// Translate a page with disk cache. `docset` scopes the cache directory,
/// `page_id` is hashed into the cache filename.
pub(crate) fn translate_page(texts: Vec<String>,
                             config: &TranslationConfig,
                             docset: &str,
                             page_id: &str)
                             -> Vec<String>
{
  if texts.is_empty() {
    return vec![];
  }
  if config.no_cache {
    return translate_texts(texts, config);
  }

  // Index cache hits by source hash.
  let mut cached: HashMap<String, String> = HashMap::new();
  if let Some(cache) = read_cache(docset, page_id, config) {
    for unit in cache.units {
      cached.insert(unit.source_hash, unit.translated);
    }
  }

  let mut missing_idx = vec![];
  let mut missing = vec![];
  let mut out: Vec<Option<String>> = Vec::with_capacity(texts.len());
  for (i, text) in texts.iter().enumerate() {
    match cached.get(&hash_text(text)) {
      Some(hit) => out.push(Some(hit.clone())),
      None => {
        missing_idx.push(i);
        missing.push(text.clone());
        out.push(None);
      }
    }
  }

  if !missing.is_empty() {
    for (slot, translated) in missing_idx.into_iter().zip(translate_texts(missing, config)) {
      out[slot] = Some(translated);
    }
  }

  let result: Vec<String> =
    texts.iter().zip(out.into_iter()).map(|(src, t)| t.unwrap_or_else(|| src.clone())).collect();

  // Merge with existing units and persist (best effort, failures ignored).
  let mut units: Vec<CacheUnit> = vec![];
  let mut seen = HashSet::new();
  if let Some(existing) = read_cache(docset, page_id, config) {
    for unit in existing.units {
      if seen.insert(unit.source_hash.clone()) {
        units.push(unit);
      }
    }
  }
  for (source, translated) in texts.iter().zip(result.iter()) {
    let h = hash_text(source);
    if seen.insert(h.clone()) {
      units.push(CacheUnit { source_hash: h,
                             source: source.clone(),
                             translated: translated.clone() });
    }
  }
  write_cache(docset, page_id, &TranslationCache { version: CACHE_VERSION,
                                                   source_lang: "en".to_string(),
                                                   target_lang: config.target_lang.clone(),
                                                   provider: config.provider.clone(),
                                                   glossary_hash: "none".to_string(),
                                                   units });

  result
}
