# Finder

A lightning-fast tool for searching terabytes of code. Finder can search for keywords in file names, directory names, and file contents.

## Features

- Blazing fast searches through large codebases
- Search in file names, directory names, file contents, or all of them
- Regular expression support
- Case-sensitive or case-insensitive search
- Progress indicators
- Support for ignoring binary files
- Follow symbolic links option
- Depth limiting
- Respects `.gitignore` files

## Installation

Ensure you have Rust and Cargo installed, then:

```bash
# Clone the repository
git clone https://github.com/yourusername/finder.git
cd finder

# Build in release mode for maximum performance
cargo build --release

# Optional: install to your path
cargo install --path .
```

## Usage

```bash
# Basic usage - search for "example" in the current directory
finder example

# Search only in file names
finder --mode FileName "example"

# Search only in directory names
finder --mode DirName "example"

# Search only in file contents
finder --mode Content "example"

# Search using a regular expression
finder --regex "foo.*bar"

# Case sensitive search
finder --case-sensitive "Example"

# Search a specific directory
finder "example" /path/to/search

# Limit search depth
finder --max-depth 3 "example"

# Follow symbolic links
finder --follow-links "example"

# Turn off binary file filtering
finder --ignore-binary false "example"

# Turn off progress indication
finder --progress false "example"
```

## Command Line Options

```
Usage: finder [OPTIONS] <PATTERN> [PATH]

Arguments:
  <PATTERN>  The pattern to search for
  [PATH]     The root directory to start searching from [default: .]

Options:
  -m, --mode <MODE>              Search mode (file names, directory names, file contents, or all) [default: all]
  -r, --regex                    Use regex pattern matching
  -c, --case-sensitive           Case sensitive search (default is case insensitive)
  -i, --ignore-binary <BOOL>     Ignore binary files when searching content [default: true]
  -f, --follow-links             Follow symbolic links
  -d, --max-depth <MAX_DEPTH>    Maximum depth to search
  -p, --progress <BOOL>          Show progress bar [default: true]
  -h, --help                     Print help
  -V, --version                  Print version
```

## Performance Tips

1. For large codebases, use the `--mode` flag to limit the search scope
2. Consider using regex for more complex pattern matching
3. The `--max-depth` option can significantly improve performance when you only need to search to a certain depth
4. When searching file contents in large codebases, results will appear as they're found

## License

MIT 