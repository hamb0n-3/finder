#[cfg(all(feature = "use-jemalloc", not(target_env = "msvc")))]
use jemallocator::Jemalloc;

#[cfg(all(feature = "use-jemalloc", not(target_env = "msvc")))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::fs::{self}; // For reading file content and metadata, removed Metadata
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime};    // For last modified time

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use grep_regex::RegexMatcher;
use grep_searcher::sinks::UTF8;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use ignore::WalkBuilder;
use indicatif::{ProgressBar, ProgressStyle};
use log::{info, warn, error, debug, trace, LevelFilter};
use regex::Regex;
// Ensure relevant imports are present for the refined run_indexer
use rusqlite::{Connection, OptionalExtension}; // Removed Result as RusqliteResult, params

/// CLI Enum for specifying log levels
#[derive(Debug, Copy, Clone, PartialEq, Eq, ValueEnum)]
enum LogLevelCli {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

/// Enum representing the different search modes
#[derive(Debug, Copy, Clone, PartialEq, Eq, ValueEnum)]
enum SearchMode {
    #[clap(name = "file-name")]
    FileName,
    #[clap(name = "dir-name")]
    DirName,
    Content,
    All,
}

/// Enum representing the different subcommands
#[derive(Parser, Debug)]
enum SubCommand {
    Search(SearchConfig),
    Index(IndexConfig),
}

/// Configuration for the search subcommand
#[derive(Parser, Debug, Clone)]
struct SearchConfig {
    #[arg(required = true)]
    pattern: String,
    #[arg(default_value = ".")]
    path: PathBuf,
    #[arg(short, long, value_enum, default_value_t = SearchMode::All)]
    mode: SearchMode,
    #[arg(short, long)]
    regex: bool,
    #[arg(short, long)]
    case_sensitive: bool,
    #[arg(short, long, default_value_t = true)]
    ignore_binary: bool,
    #[arg(short, long)]
    follow_links: bool,
    #[arg(short, long)]
    max_depth: Option<usize>,
    #[arg(short, long, default_value_t = true)]
    progress: bool,
    #[clap(skip)]
    pattern_lowercase: Option<String>,
    
    #[arg(long, help = "Use the pre-built index for searching (experimental)")]
    use_index: bool,
    #[arg(long, default_value = "finder.db", help = "Path to the SQLite database file to use for indexed search")]
    db_path: PathBuf, // New field for specifying DB path for search
}

/// Configuration for the index subcommand
#[derive(Parser, Debug)]
struct IndexConfig {
    #[arg(default_value = ".")]
    path: PathBuf, // Path to the directory to index
    #[arg(long, default_value = "finder.db")]
    db_path: PathBuf, // Path to the SQLite database file
}

/// Configuration for the finder program
#[derive(Parser, Debug)]
#[command(author, version, about = "A fast file finder tool", long_about = None)]
struct Config {
    #[clap(subcommand)]
    subcommand: SubCommand,


    /// Set the logging level.
    #[arg(long, value_enum, help = "Set the logging level (error, warn, info, debug, trace)")]
    log_level: Option<LogLevelCli>,

    /// Specify a file to write logs to. Defaults to stderr.
    #[arg(long, help = "Path to a file to write logs to (e.g., finder.log)")]
    log_file: Option<PathBuf>,
    // name_regex_matcher is no longer part of Config
    // name_regex_matcher: Option<Regex>, 
}

#[derive(Debug, Clone)]
struct Match {
    path: PathBuf,
    match_type: MatchType,
    line_number: Option<usize>,
    line_content: Option<String>,
}

#[derive(Debug, Clone)]
enum MatchType {
    FileName,
    DirName,
    FileContent,
}

fn main() -> Result<()> {
    let config = Config::parse();

    // Initialize logger
    let mut log_builder = env_logger::Builder::new();

    // Set log level: CLI takes precedence over RUST_LOG, then default
    if let Some(cli_level) = config.log_level {
        let level_filter = match cli_level {
            LogLevelCli::Error => LevelFilter::Error,
            LogLevelCli::Warn => LevelFilter::Warn,
            LogLevelCli::Info => LevelFilter::Info,
            LogLevelCli::Debug => LevelFilter::Debug,
            LogLevelCli::Trace => LevelFilter::Trace,
        };
        log_builder.filter_level(level_filter);
    } else {
        log_builder.parse_env(env_logger::Env::default().default_filter_or("info"));
    }

    // Set log target: file if specified, otherwise default (stderr)
    if let Some(log_file_path) = &config.log_file {
        match File::options().create(true).append(true).open(log_file_path) {
            Ok(file) => {
                log_builder.target(env_logger::Target::Pipe(Box::new(file)));
            }
            Err(e) => {
                eprintln!(
                    "Warning: Could not open log file '{}': {}. Logging to stderr instead.",
                    log_file_path.display(),
                    e
                );
                log_builder.target(env_logger::Target::Stderr);
            }
        }
    } else {
        log_builder.target(env_logger::Target::Stderr);
    }

    log_builder.init();

    match config.subcommand {
        SubCommand::Search(ref search_config) => {
            run_search(search_config.clone(), &config)?;
        }
        SubCommand::Index(index_config) => {
            // Call run_indexer
            if let Err(e) = run_indexer(index_config) {
                error!("Indexer failed: {}", e);
                // Consider exiting with an error code if the indexer is critical
                // std::process::exit(1); 
            }
        }
    }

    Ok(())
}

fn run_indexer(index_config: IndexConfig) -> Result<()> {
    info!("Indexer started. Indexing path: '{}', Database: '{}'", 
          index_config.path.display(), index_config.db_path.display());
    let start_time = Instant::now();

    let mut conn = Connection::open(&index_config.db_path) // Make conn mutable for transactions
        .with_context(|| format!("Failed to open database at '{}'", index_config.db_path.display()))?;
    info!("Successfully opened database: {}", index_config.db_path.display());

    // Create tables (same as before)
    conn.execute(
        "CREATE TABLE IF NOT EXISTS files (
            id INTEGER PRIMARY KEY,
            path TEXT NOT NULL UNIQUE,
            last_modified INTEGER NOT NULL
        )",
        [],
    ).with_context(|| "Failed to create 'files' table")?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS tokens (
            id INTEGER PRIMARY KEY,
            token TEXT NOT NULL,
            file_id INTEGER NOT NULL,
            FOREIGN KEY (file_id) REFERENCES files (id) ON DELETE CASCADE -- Added ON DELETE CASCADE
        )",
        [],
    ).with_context(|| "Failed to create 'tokens' table")?;
    info!("Database tables created or already exist.");

    let mut files_processed_count = 0;
    let mut files_newly_indexed_count = 0;
    let mut files_updated_count = 0;
    let mut tokens_inserted_count = 0;

    let tx = conn.transaction().with_context(|| "Failed to start database transaction")?;

    let walker = WalkBuilder::new(&index_config.path);
    // walker.standard_filters(true); // Consider user preference for this
    // walker.follow_links(false); // Usually false for indexing to avoid cycles/redundancy

    for result in walker.build() {
        match result {
            Ok(entry) => {
                if !entry.file_type().map_or(false, |ft| ft.is_file()) {
                    if entry.file_type().map_or(false, |ft| ft.is_dir()) && entry.path().is_dir() {
                        // Log directory being processed, useful for tracking progress / debugging permission issues
                        trace!("Processing directory for indexing: {}", entry.path().display());
                    }
                    continue; // Skip non-files (directories, symlinks if not followed, etc.)
                }
                
                files_processed_count += 1;
                let path = entry.path();

                match fs::metadata(path) {
                    Ok(metadata) => {
                        let last_modified_secs = metadata.modified()
                            .ok()
                            .and_then(|st| st.duration_since(SystemTime::UNIX_EPOCH).ok())
                            .map(|d| d.as_secs())
                            .unwrap_or(0);

                        let path_str = path.to_string_lossy().into_owned();

                        // Check if file exists in DB and if it's modified
                        let existing_file_info: Option<(i64, i64)> = tx.query_row(
                            "SELECT id, last_modified FROM files WHERE path = ?1",
                            rusqlite::params![path_str],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        ).optional().with_context(|| format!("Failed to query existing file data for {}", path_str))?;

                        let mut needs_reindexing = true;
                        let mut existing_file_id: Option<i64> = None;

                        if let Some((id, db_last_modified)) = existing_file_info {
                            existing_file_id = Some(id);
                            if db_last_modified >= last_modified_secs as i64 {
                                needs_reindexing = false;
                                trace!("File {} is already indexed and up-to-date. Skipping.", path_str);
                            } else {
                                debug!("File {} has been modified since last index. Re-indexing.", path_str);
                                // Delete old tokens for this file before inserting new ones
                                tx.execute("DELETE FROM tokens WHERE file_id = ?1", rusqlite::params![id])
                                    .with_context(|| format!("Failed to delete old tokens for file ID {}", id))?;
                                files_updated_count += 1;
                            }
                        } else {
                            files_newly_indexed_count += 1;
                        }

                        if needs_reindexing {
                            let file_id_to_use = match existing_file_id {
                                Some(id) => {
                                    // Update last_modified for the existing file entry
                                    tx.execute("UPDATE files SET last_modified = ?1 WHERE id = ?2", rusqlite::params![last_modified_secs, id])
                                        .with_context(|| format!("Failed to update last_modified for file ID {}", id))?;
                                    id
                                }
                                None => {
                                    // Insert new file entry
                                    tx.execute(
                                        "INSERT INTO files (path, last_modified) VALUES (?1, ?2)",
                                        rusqlite::params![path_str, last_modified_secs],
                                    ).with_context(|| format!("Failed to insert new file {}", path_str))?;
                                    tx.last_insert_rowid()
                                }
                            };
                            
                            if existing_file_id.is_none() { // Only log "Indexed file" for truly new files
                                debug!("Indexed file: {} (ID: {})", path_str, file_id_to_use);
                            }


                            match fs::read_to_string(path) {
                                Ok(content) => {
                                    let file_tokens: Vec<String> = content
                                        .split_whitespace()
                                        .map(|s| s.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect::<String>())
                                        .filter(|s| !s.is_empty() && s.len() > 1)
                                        .collect();
                                    
                                    let mut current_file_tokens_inserted = 0;
                                    for token_str in file_tokens {
                                        // Consider INSERT OR IGNORE if token+file_id duplicates are possible and benign
                                        match tx.execute(
                                            "INSERT INTO tokens (token, file_id) VALUES (?1, ?2)",
                                            rusqlite::params![token_str, file_id_to_use],
                                        ) {
                                            Ok(_) => current_file_tokens_inserted += 1,
                                            Err(e) => warn!("Failed to insert token '{}' for file_id {}: {}", token_str, file_id_to_use, e),
                                        }
                                    }
                                    tokens_inserted_count += current_file_tokens_inserted;
                                    if current_file_tokens_inserted > 0 {
                                        trace!("Inserted {} tokens for file {}", current_file_tokens_inserted, path_str);
                                    }
                                }
                                Err(e) => {
                                    // Log error but continue indexing other files.
                                    // The file entry in 'files' table might exist without tokens if content is unreadable.
                                    debug!("Failed to read content of {} for tokenization: {}. No tokens will be indexed for this file.", path.display(), e);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to get metadata for {}: {}. Skipping file.", path.display(), e);
                    }
                }
            }
            Err(e) => {
                // This error often indicates a problem with accessing a directory (e.g., permissions)
                // Extract path information from ignore::Error if available.
                // This helps in identifying which file or directory caused the issue during the walk.
                let path_display_str = match &e {
                    // Correctly destructure WithPath to get the path.
                    ignore::Error::WithPath { path, .. } => path.display().to_string(),
                    // For loop errors, the 'child' path is often the problematic one.
                    ignore::Error::Loop { child, .. } => child.display().to_string(),
                    // For other error types from the 'ignore' crate,
                    // a specific entry path might not be directly available in the error variant,
                    // or it might be nested. "unknown path" is used as a fallback,
                    // and the full error string from e.to_string() will provide more context.
                    // The Io variant (ignore::Error::Io(std_io_error)) doesn't directly give us a path here.
                    // If an Io error is path-specific, 'ignore' usually wraps it in WithPath.
                    _ => "unknown path".to_string(),
                };

                // Log the full error details. e.to_string() provides a comprehensive message from the 'ignore' crate.
                error!(
                    "Error walking directory for indexing (entry path: {}): {}. Subsequent entries in this directory might be missed.",
                    path_display_str,
                    e.to_string() // This provides the full context of the ignore::Error
                );
            }
        }
    }

    tx.commit().with_context(|| "Failed to commit database transaction")?;
    
    let elapsed = start_time.elapsed();
    info!(
        "Indexer finished in {:.2}s. Processed {} files. Newly indexed: {}. Updated: {}. Total tokens inserted: {}.", 
        elapsed.as_secs_f64(),
        files_processed_count,
        files_newly_indexed_count,
        files_updated_count,
        tokens_inserted_count
    );
    Ok(())
}

// New function to encapsulate search logic
fn run_search(mut search_config: SearchConfig, main_config: &Config) -> Result<()> {
    info!("Finder application started (Search mode)");
    if let Some(log_file_path) = &main_config.log_file {
        info!("Logging to file: {}", log_file_path.display());
    }
    debug!("Parsed search configuration: {:?}", search_config);

    let start_time = Instant::now();

    if !search_config.regex && !search_config.case_sensitive {
        search_config.pattern_lowercase = Some(search_config.pattern.to_lowercase());
        debug!("Pre-computed lowercase pattern: {:?}", search_config.pattern_lowercase.as_ref().unwrap());
    }


    info!(
        "Starting search for pattern '{}' in path '{}' (mode: {:?}, regex: {}, case_sensitive: {})",
        search_config.pattern,
        search_config.path.display(),
        search_config.mode,
        search_config.regex,
        search_config.case_sensitive
    );

    let content_matcher = create_content_matcher(&search_config)?;
    let processed_entry_count = Arc::new(AtomicUsize::new(0));
    let found_items_count_for_progress = Arc::new(AtomicUsize::new(0));

    let progress_bar = if search_config.progress {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.green} [{elapsed_precise}] Searched: {pos}, Found: {msg}")
                .unwrap(),
        );
        Some(pb)
    } else {
        None
    };

    let search_file_names = search_config.mode == SearchMode::FileName || search_config.mode == SearchMode::All;
    let search_dir_names = search_config.mode == SearchMode::DirName || search_config.mode == SearchMode::All;
    let search_contents = search_config.mode == SearchMode::Content || search_config.mode == SearchMode::All;

    let mut walker = WalkBuilder::new(&search_config.path);
    walker.standard_filters(true);
    walker.follow_links(search_config.follow_links);
    if let Some(max_depth) = search_config.max_depth {
        debug!("Max search depth set to: {}", max_depth);
        walker.max_depth(Some(max_depth));
    }

    let name_matcher: Option<Regex> = if search_config.regex {
        let pattern = if search_config.case_sensitive {
            search_config.pattern.clone()
        } else {
            format!("(?i){}", search_config.pattern)
        };
        debug!("Compiled name regex pattern for names: {}", pattern);
        Some(Regex::new(&pattern).context("Failed to compile name regex for names")?)
    } else {
        None
    };

    let matches_arc = Arc::new(std::sync::Mutex::new(Vec::new()));
    let matches_clone_for_walker = Arc::clone(&matches_arc);
    let found_count_clone_for_walker = Arc::clone(&found_items_count_for_progress);
    let processed_count_clone_for_walker = Arc::clone(&processed_entry_count);

    walker.build_parallel().run(|| {
        let matches_in_thread = Arc::clone(&matches_clone_for_walker);
        let found_count_progress_in_thread = Arc::clone(&found_count_clone_for_walker);
        let processed_count_in_thread = Arc::clone(&processed_count_clone_for_walker);
        
        let search_config_ref = &search_config; 
        let content_matcher_ref = &content_matcher;
        let name_matcher_ref = &name_matcher;
        let progress_bar_ref = &progress_bar;

        Box::new(move |result| {
            match result {
                Ok(entry) => {
                    trace!("Processing entry: {}", entry.path().display());
                    let current_processed_count = processed_count_in_thread.fetch_add(1, Ordering::Relaxed) + 1;

                    if let Some(pb) = progress_bar_ref {
                        if current_processed_count % 200 == 0 || current_processed_count == 1 {
                            let found = found_count_progress_in_thread.load(Ordering::Relaxed);
                            pb.set_position(current_processed_count as u64);
                            pb.set_message(format!("{}", found));
                        }
                    }

                    let file_type = entry.file_type();
                    let is_dir = file_type.map_or(false, |ft| ft.is_dir());
                    let is_file = file_type.map_or(false, |ft| ft.is_file());
                    let mut local_matches = Vec::new();

                    if search_dir_names && is_dir {
                        let path = entry.path();
                        if let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) {
                            if matches_name(search_config_ref, dir_name, name_matcher_ref) {

                                debug!("Found directory match: {}", path.display());
                                local_matches.push(Match {
                                    path: path.to_path_buf(),
                                    match_type: MatchType::DirName,
                                    line_number: None,
                                    line_content: None,
                                });
                            }
                        }
                    }

                    if search_file_names && is_file {
                        let path = entry.path();
                        if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
                            if matches_name(search_config_ref, file_name, name_matcher_ref) {
                                debug!("Found file name match: {}", path.display());
                                local_matches.push(Match {
                                    path: path.to_path_buf(),
                                    match_type: MatchType::FileName,
                                    line_number: None,
                                    line_content: None,
                                });
                            }
                        }
                    }

                    if search_contents && is_file {
                        let path = entry.path();
                        debug!("Searching content in file: {}", path.display());
                        match search_file_content(search_config_ref, content_matcher_ref, path) {
                            Ok(content_matches) => {
                                if !content_matches.is_empty() {
                                    info!("Content match(es) found in file: {}", path.display());
                                    debug!("Found {} content matches in {}", content_matches.len(), path.display());
                                }
                                local_matches.extend(content_matches);
                            },
                            Err(e) => warn!("Error searching content in {}: {}", path.display(), e),
                        }
                    }

                    if !local_matches.is_empty() {
                        let num_found = local_matches.len();
                        found_count_progress_in_thread.fetch_add(num_found, Ordering::Relaxed);
                        if let Ok(mut matches_guard) = matches_in_thread.lock() {
                            matches_guard.extend(local_matches);
                        } else {
                            error!("Mutex for matches was poisoned during extend.");
                        }
                    }
                },
                Err(e) => {
                    warn!("Error processing directory entry: {}", e);
                }
            }
            ignore::WalkState::Continue
        })
    });

    let final_matches_vec = {
        let mut guard = matches_arc.lock().unwrap_or_else(|poisoned| {
            error!("Matches mutex was poisoned before final collection. Recovering data.");
            poisoned.into_inner()
        });
        std::mem::take(&mut *guard)
    };

    let final_found_count = final_matches_vec.len();
    let final_processed_count = processed_entry_count.load(Ordering::Relaxed);

    if let Some(pb) = progress_bar {
        pb.set_position(final_processed_count as u64);
        pb.finish_with_message(format!("{}", final_found_count));
    }

    for m in &final_matches_vec {
        match m.match_type {
            MatchType::FileName => println!("File: {}", m.path.display()),
            MatchType::DirName => println!("Directory: {}", m.path.display()),
            MatchType::FileContent => {
                println!(
                    "Content: {}:{}:{}",
                    m.path.display(),
                    m.line_number.unwrap_or(0),
                    m.line_content.as_deref().unwrap_or(""),
                );
            }
        }
    }

    let elapsed = start_time.elapsed();
    info!(
        "Search completed in {:.2}s. Processed {} entries, found {} matches.",
        elapsed.as_secs_f64(),
        final_processed_count,
        final_found_count
    );
    Ok(())
}

fn create_content_matcher(search_config: &SearchConfig) -> Result<RegexMatcher> {
    let pattern_str = if search_config.regex {
        search_config.pattern.clone()
    } else {
        regex::escape(&search_config.pattern)
    };
    debug!("Content matcher regex pattern: {}", pattern_str);
    let created_matcher = if search_config.case_sensitive {
        RegexMatcher::new(&pattern_str)
    } else {
        RegexMatcher::new_line_matcher(&format!("(?i){}", pattern_str))
    };
    created_matcher.with_context(|| format!("Failed to create content matcher with pattern: '{}'", search_config.pattern))
}

fn matches_name(search_config: &SearchConfig, name_to_check: &str, name_regex_matcher: &Option<Regex>) -> bool {
    trace!("Matching name: '{}' against pattern: '{}' (regex: {}, case_sensitive: {})", 
           name_to_check, search_config.pattern, search_config.regex, search_config.case_sensitive);

    if search_config.regex {
        match name_regex_matcher {
            Some(re) => re.is_match(name_to_check),
            None => {
                warn!("Regex mode is on, but no compiled regex matcher was provided for name matching. Pattern: '{}'", search_config.pattern);
                false
            }
        }
    } else {
        if search_config.case_sensitive {
            name_to_check.contains(&search_config.pattern)
        } else {
            match &search_config.pattern_lowercase {
                Some(lower_pattern) => {
                    if lower_pattern.is_empty() {
                        name_to_check.is_empty()
                    } else {
                        name_to_check.to_lowercase().contains(lower_pattern)
                    }
                }
                None => {
                    warn!("Case-insensitive non-regex search attempted, but lowercase pattern was not pre-computed. Original pattern: '{}'", search_config.pattern);
                    name_to_check.to_lowercase().contains(&search_config.pattern.to_lowercase())
                }
            }
        }
    }
}

fn search_file_content(search_config: &SearchConfig, matcher: &RegexMatcher, path: &Path) -> Result<Vec<Match>> {
    trace!("Searching content in: {}", path.display());
    let mut matches = Vec::new();
    let binary_detection = if search_config.ignore_binary {
        BinaryDetection::quit(b'\0')
    } else {
        BinaryDetection::none()
    };

    let mut searcher = SearcherBuilder::new()
        .binary_detection(binary_detection)
        .line_number(true)
        .build();

    searcher.search_path(
        matcher,
        path,
        UTF8(|line_number, line| {
            let line_num = line_number.try_into().unwrap_or(usize::MAX);
            trace!("Content match in {}:{} - {}", path.display(), line_num, line.trim_end());
            matches.push(Match {
                path: path.to_path_buf(),
                match_type: MatchType::FileContent,
                line_number: Some(line_num),
                line_content: Some(line.trim_end().to_string()),
            });
            Ok(true)
        }),
    )?;
    Ok(matches)
} 