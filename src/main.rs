#[cfg(all(feature = "use-jemalloc", not(target_env = "msvc")))]
use jemallocator::Jemalloc;

#[cfg(all(feature = "use-jemalloc", not(target_env = "msvc")))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use grep_regex::RegexMatcher;
use grep_searcher::sinks::UTF8;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use ignore::WalkBuilder;
use indicatif::{ProgressBar, ProgressStyle};
use log::{info, warn, error, debug, trace, LevelFilter};
use regex::Regex;
use caseless::Caseless;

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

/// Configuration for the finder program
#[derive(Parser, Debug)]
#[command(author, version, about = "A fast file finder tool", long_about = None)]
struct Config {
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

    /// Set the logging level.
    #[arg(long, value_enum, help = "Set the logging level (error, warn, info, debug, trace)")]
    log_level: Option<LogLevelCli>,

    /// Specify a file to write logs to. Defaults to stderr.
    #[arg(long, help = "Path to a file to write logs to (e.g., finder.log)")]
    log_file: Option<PathBuf>,
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
    let mut config = Config::parse();

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
        // Attempt to create/open the log file for appending
        match File::options().create(true).append(true).open(log_file_path) {
            Ok(file) => {
                log_builder.target(env_logger::Target::Pipe(Box::new(file)));
                // We can't use info! here yet as logger isn't fully initialized,
                // but this will be logged once init() is called if level allows.
                // Consider a simple println! for immediate feedback about log file if critical.
                // println!("Logging to file: {}", log_file_path.display()); 
            }
            Err(e) => {
                // If file can't be opened, log to stderr and continue with stderr logging for finder.
                // Need to use eprintln! as logger is not yet initialized for file output.
                eprintln!(
                    "Warning: Could not open log file '{}': {}. Logging to stderr instead.",
                    log_file_path.display(),
                    e
                );
                log_builder.target(env_logger::Target::Stderr); // Explicitly set to stderr
            }
        }
    } else {
        log_builder.target(env_logger::Target::Stderr); // Default to stderr if no file specified
    }

    log_builder.init(); // Initialize the logger with all configurations

    info!("Finder application started");
    if let Some(log_file_path) = &config.log_file {
        // This info message will go to the file if successfully opened.
        info!("Logging to file: {}", log_file_path.display());
    }
    debug!("Parsed configuration: {:?}", config);

    let start_time = Instant::now();

    if !config.regex && !config.case_sensitive {
        config.pattern_lowercase = Some(config.pattern.to_lowercase());
        debug!("Pre-computed lowercase pattern: {:?}", config.pattern_lowercase.as_ref().unwrap());
    }

    info!(
        "Starting search for pattern '{}' in path '{}' (mode: {:?}, regex: {}, case_sensitive: {})",
        config.pattern,
        config.path.display(),
        config.mode,
        config.regex,
        config.case_sensitive
    );

    let content_matcher = create_content_matcher(&config)?;
    let processed_entry_count = Arc::new(AtomicUsize::new(0));
    let found_items_count_for_progress = Arc::new(AtomicUsize::new(0));

    let progress_bar = if config.progress {
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

    let search_file_names = config.mode == SearchMode::FileName || config.mode == SearchMode::All;
    let search_dir_names = config.mode == SearchMode::DirName || config.mode == SearchMode::All;
    let search_contents = config.mode == SearchMode::Content || config.mode == SearchMode::All;

    let mut walker = WalkBuilder::new(&config.path);
    walker.standard_filters(true);
    walker.follow_links(config.follow_links);
    if let Some(max_depth) = config.max_depth {
        debug!("Max search depth set to: {}", max_depth);
        walker.max_depth(Some(max_depth));
    }

    let name_regex_matcher = if config.regex {
        let pattern = if config.case_sensitive {
            config.pattern.clone()
        } else {
            format!("(?i){}", config.pattern)
        };
        debug!("Compiled name regex pattern: {}", pattern);
        Some(Regex::new(&pattern).context("Failed to compile name regex")?)
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
        
        let config_ref = &config;
        let content_matcher_ref = &content_matcher;
        let name_regex_matcher_ref = &name_regex_matcher;
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
                            if matches_name(config_ref, dir_name, name_regex_matcher_ref) {
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
                            if matches_name(config_ref, file_name, name_regex_matcher_ref) {
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
                        match search_file_content(config_ref, content_matcher_ref, path) {
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

fn create_content_matcher(config: &Config) -> Result<RegexMatcher> {
    let pattern_str = if config.regex {
        config.pattern.clone()
    } else {
        regex::escape(&config.pattern)
    };
    debug!("Content matcher regex pattern: {}", pattern_str);
    let matcher = if config.case_sensitive {
        RegexMatcher::new(&pattern_str)
    } else {
        RegexMatcher::new_line_matcher(&format!("(?i){}", pattern_str))
    };
    matcher.with_context(|| format!("Failed to create content matcher with pattern: '{}'", config.pattern))
}

fn matches_name(config: &Config, name_to_check: &str, name_regex_matcher: &Option<Regex>) -> bool {
    trace!("Matching name: '{}' against pattern: '{}' (regex: {}, case_sensitive: {})", 
           name_to_check, config.pattern, config.regex, config.case_sensitive);
    if let Some(re) = name_regex_matcher {
        re.is_match(name_to_check)
    } else if config.case_sensitive {
        name_to_check.contains(&config.pattern)
    } else {
        let pattern_for_caseless = config.pattern_lowercase.as_deref().unwrap_or(&config.pattern);
        if pattern_for_caseless.is_empty() {
            return name_to_check.is_empty();
        }
        name_to_check.chars().default_caseless_match(pattern_for_caseless.chars())
    }
}

fn search_file_content(config: &Config, matcher: &RegexMatcher, path: &Path) -> Result<Vec<Match>> {
    trace!("Searching content in: {}", path.display());
    let mut matches = Vec::new();
    let binary_detection = if config.ignore_binary {
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