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
use regex::Regex;

/// Enum representing the different search modes
#[derive(Debug, Copy, Clone, PartialEq, Eq, ValueEnum)]
enum SearchMode {
    /// Search for the pattern in file names
    #[clap(name = "file-name")]
    FileName,
    /// Search for the pattern in directory names
    #[clap(name = "dir-name")]
    DirName,
    /// Search for the pattern in file contents
    Content,
    /// Search everywhere (file names, directory names, and file contents)
    All,
}

/// Configuration for the finder program
#[derive(Parser, Debug)]
#[command(author, version, about = "A fast file finder tool", long_about = None)]
struct Config {
    /// The pattern to search for
    #[arg(required = true)]
    pattern: String,

    /// The root directory to start searching from
    #[arg(default_value = ".")]
    path: PathBuf,

    /// Search mode (file names, directory names, file contents, or all)
    #[arg(short, long, value_enum, default_value_t = SearchMode::All)]
    mode: SearchMode,

    /// Use regex pattern matching
    #[arg(short, long)]
    regex: bool,

    /// Case sensitive search (default is case insensitive)
    #[arg(short, long)]
    case_sensitive: bool,

    /// Ignore binary files when searching content
    #[arg(short, long, default_value_t = true)]
    ignore_binary: bool,

    /// Follow symbolic links
    #[arg(short, long)]
    follow_links: bool,

    /// Maximum depth to search
    #[arg(short, long)]
    max_depth: Option<usize>,

    /// Show progress bar
    #[arg(short, long, default_value_t = true)]
    progress: bool,

    /// The pattern to search for (pre-lowercased if needed for optimization)
    #[clap(skip)] // This field is derived, not directly parsed
    pattern_lowercase: Option<String>,
}

/// Represents a search match
#[derive(Debug, Clone)]
struct Match {
    path: PathBuf,
    match_type: MatchType,
    line_number: Option<usize>,
    line_content: Option<String>,
}

/// Type of match found
#[derive(Debug, Clone)]
enum MatchType {
    FileName,
    DirName,
    FileContent,
}

fn main() -> Result<()> {
    // Parse command line arguments
    let mut config = Config::parse(); // Make config mutable to add derived fields
    
    // Start timer for performance measurement
    let start_time = Instant::now();
    
    // Precompute lowercase pattern if needed
    if !config.regex && !config.case_sensitive {
        config.pattern_lowercase = Some(config.pattern.to_lowercase());
    } else {
        config.pattern_lowercase = None; // Ensure it's None otherwise
    }
    
    // Create a matcher for file *content* search
    let content_matcher = create_content_matcher(&config)?; // Renamed for clarity
    
    // Renamed for clarity: counts processed directory entries
    let processed_entry_count = Arc::new(AtomicUsize::new(0));
    // New counter for optimized progress: counts found matches
    let found_items_count_for_progress = Arc::new(AtomicUsize::new(0));
    
    // Setup progress bar if requested
    let progress_bar = if config.progress {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::default_spinner()
                // Update template slightly for clarity
                .template("{spinner:.green} [{elapsed_precise}] Searched: {pos}, Found: {msg}")
                .unwrap(),
        );
        Some(pb)
    } else {
        None
    };
    
    // Determine search modes based on config
    let search_file_names = config.mode == SearchMode::FileName || config.mode == SearchMode::All;
    let search_dir_names = config.mode == SearchMode::DirName || config.mode == SearchMode::All;
    let search_contents = config.mode == SearchMode::Content || config.mode == SearchMode::All;
    
    // Build the walker for directory traversal
    let mut walker = WalkBuilder::new(&config.path);
    walker.standard_filters(true); // Respect .gitignore files
    walker.follow_links(config.follow_links);
    
    if let Some(max_depth) = config.max_depth {
        walker.max_depth(Some(max_depth));
    }
    
    // Compile regex for name matching if using regex mode
    let name_regex_matcher = if config.regex {
        let pattern = if config.case_sensitive {
            config.pattern.clone()
        } else {
            format!("(?i){}", config.pattern) // (?i) for case-insensitivity
        };
        Some(Regex::new(&pattern).context("Failed to compile name regex")?)
    } else {
        None
    };
    
    // Use Arc<Mutex<Vec>> to collect matches from threads
    let matches_arc = Arc::new(std::sync::Mutex::new(Vec::new()));
    let matches_clone = Arc::clone(&matches_arc);
    let found_count_clone = Arc::clone(&found_items_count_for_progress); // Clone for the closure
    
    // Process entries in parallel using WalkParallel's run method
    walker.build_parallel().run(|| {
        // Clone Arcs and references for the move closure
        let matches = Arc::clone(&matches_clone);
        let found_count_progress = Arc::clone(&found_count_clone); // Use the cloned counter
        let matcher = &content_matcher; // Use the renamed content matcher
        let config = &config;
        let progress_bar = &progress_bar;
        let processed_count = &processed_entry_count; // Use the renamed processed counter
        let name_matcher = &name_regex_matcher; // Use the renamed regex matcher
        
        Box::new(move |result| {
            if let Ok(entry) = result {
                let current_processed_count = processed_count.fetch_add(1, Ordering::Relaxed);
                
                // Update progress bar
                if let Some(pb) = progress_bar {
                    // Update less frequently for potentially better performance
                    if current_processed_count % 200 == 0 {
                         // Use dedicated counters
                        let found = found_count_progress.load(Ordering::Relaxed);
                        pb.set_position(current_processed_count as u64);
                        pb.set_message(format!("{}", found)); // Found count is the message
                    }
                }
                
                // Skip entries we don't care about based on search mode
                let file_type = entry.file_type();
                let is_dir = file_type.map_or(false, |ft| ft.is_dir());
                let is_file = file_type.map_or(false, |ft| ft.is_file());
                
                let mut local_matches = Vec::new();
                
                // Check directory names
                if search_dir_names && is_dir {
                    let path = entry.path();
                    let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    
                    if matches_name(config, dir_name, name_matcher) {
                        local_matches.push(Match {
                            path: path.to_path_buf(),
                            match_type: MatchType::DirName,
                            line_number: None,
                            line_content: None,
                        });
                    }
                }
                
                // Check file names
                if search_file_names && is_file {
                    let path = entry.path();
                    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    
                    if matches_name(config, file_name, name_matcher) {
                        local_matches.push(Match {
                            path: path.to_path_buf(),
                            match_type: MatchType::FileName,
                            line_number: None,
                            line_content: None,
                        });
                    }
                }
                
                // Search file contents
                if search_contents && is_file {
                    match search_file_content(config, matcher, entry.path()) {
                        Ok(content_matches) => local_matches.extend(content_matches),
                        Err(e) => eprintln!("Error searching content in {}: {}", entry.path().display(), e),
                    }
                }
                
                // Add local matches to the shared matches collection
                if !local_matches.is_empty() {
                    // Increment the atomic counter for progress *before* locking
                    let num_found = local_matches.len();
                    found_count_progress.fetch_add(num_found, Ordering::Relaxed);
                    // Now lock and extend
                    let mut matches_guard = matches.lock().unwrap();
                    matches_guard.extend(local_matches);
                }
            } else if let Err(e) = result {
                 // Optionally log errors encountered during walk
                 eprintln!("Warning: Error processing entry: {}", e);
            }
            
            ignore::WalkState::Continue
        })
    });
    
    // Corrected final results collection: Lock and take the inner Vec
    let final_matches_vec = {
        let mut guard = matches_arc.lock().unwrap();
        // Take the Vec out of the MutexGuard, leaving an empty Vec behind.
        // This avoids cloning the whole Vec.
        std::mem::take(&mut *guard)
    };
    
    // Final count for summary message
    let final_found_count = final_matches_vec.len();
    
    // Finish progress bar
    if let Some(pb) = progress_bar {
        // Update final counts
        let final_processed = processed_entry_count.load(Ordering::Relaxed);
        pb.set_position(final_processed as u64);
        pb.finish_with_message(format!("{}", final_found_count)); // Just show final count
    }
    
    // Print results
    for m in &final_matches_vec { // Use the final vector
        match m.match_type {
            MatchType::FileName => println!("File: {}", m.path.display()),
            MatchType::DirName => println!("Directory: {}", m.path.display()),
            MatchType::FileContent => {
                println!(
                    "Content: {}:{}:{}",
                    m.path.display(),
                    m.line_number.unwrap_or(0), // Keep existing format
                    m.line_content.as_deref().unwrap_or(""),
                );
            }
        }
    }
    
    let elapsed = start_time.elapsed();
    println!(
        "Search completed in {:.2} seconds. Processed {} entries, found {} matches.", // Updated summary
        elapsed.as_secs_f64(),
        processed_entry_count.load(Ordering::Relaxed), // Use final processed count
        final_found_count // Use final found count
    );
    
    Ok(())
}

/// Create a matcher for file *content* search
fn create_content_matcher(config: &Config) -> Result<RegexMatcher> { // Renamed function
    let pattern = if config.regex {
        config.pattern.clone()
    } else {
        // Escape the user pattern for literal matching
        regex::escape(&config.pattern)
    };
    
    let matcher = if config.case_sensitive {
        RegexMatcher::new(&pattern)
    } else {
        // Use new_line_matcher for potentially better performance with (?i)
        RegexMatcher::new_line_matcher(&format!("(?i){}", pattern))
    };
    
    matcher.with_context(|| format!("Failed to create content matcher with pattern: '{}'", config.pattern))
}

/// Check if a name (file or directory) matches the search pattern
fn matches_name(config: &Config, name: &str, name_regex_matcher: &Option<Regex>) -> bool {
    if let Some(re) = name_regex_matcher {
        // Use the pre-compiled regex if available (handles case sensitivity internally)
        re.is_match(name)
    } else if config.case_sensitive {
        // Simple substring check (case-sensitive)
        name.contains(&config.pattern)
    } else {
        // Use pre-lowercased pattern for case-insensitive comparison
        // Note: name.to_lowercase() still allocates, but avoids repeated pattern lowercasing.
        // For maximum performance, consider libraries like `caseless` if this becomes a bottleneck.
        name.to_lowercase().contains(config.pattern_lowercase.as_ref().unwrap_or(&config.pattern))
    }
}

/// Search file contents for matches
fn search_file_content(config: &Config, matcher: &RegexMatcher, path: &Path) -> Result<Vec<Match>> {
    let mut matches = Vec::new();
    
    // Binary detection strategy
    let binary_detection = if config.ignore_binary {
        BinaryDetection::quit(b' ')
    } else {
        BinaryDetection::none()
    };
    
    // Create a searcher with configuration
    let mut searcher = SearcherBuilder::new()
        .binary_detection(binary_detection)
        // Consider adding .line_number(true) explicitly if needed, though default
        .build();
    
    // Search the file
    searcher.search_path(
        matcher,
        path,
        UTF8(|line_number, line| {
            // Ensure line_number is converted correctly
            let line_num = line_number.try_into().unwrap_or(usize::MAX); // Safely convert u64 to usize
            matches.push(Match {
                path: path.to_path_buf(),
                match_type: MatchType::FileContent,
                line_number: Some(line_num),
                line_content: Some(line.trim_end().to_string()), // trim_end is usually sufficient
            });
            Ok(true) // Continue searching in the file
        }),
    )?; // Propagate errors from searcher
    
    Ok(matches)
}
