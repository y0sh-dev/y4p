// Copyright (C) 2026 yosana
// SPDX-License-Identifier: GPL-3.0-or-later

// src/cli/help.rs

/// Display the application version and primary system description.
pub fn print_version() {
    println!("y4p v1.0.0");
    println!("Unified Wayland Clipboard Infrastructure.");
}

/// Render structured usage instructions, command definitions, and technical examples.
pub fn print_help() {
    print_version();

    // BUGFIX: every example below previously invoked "y4-clip", a binary
    // name that does not exist anywhere else in the project — Cargo.toml
    // names the package (and therefore the built binary) "y4p",
    // matching the README's own install instructions
    // (`cp target/release/y4p /usr/local/bin/`). Other files used
    // yet a *third*, different placeholder ("y1-clip"; fixed separately in
    // cli/mod.rs, cli/daemon.rs, cli/search.rs). Every copy-pasted example
    // command in this help text would have failed with "command not
    // found". All examples below now consistently say "y4p".
    println!("\nUSAGE:");
    println!("    y4p <COMMAND> [ARGS] [OPTIONS]");

    println!("\nCORE COMMANDS:");
    println!("    daemon             - Initialize background monitor and IPC socket listener.");
    println!("                         Flags: --verbose (-v).");

    println!("    status             - Query the running daemon and print its status.");
    println!("                         No flags or arguments.");

    println!("    pause              - Suspend clipboard monitoring (private mode).");
    println!("                         No flags or arguments.");

    println!("    resume             - Resume clipboard monitoring.");
    println!("                         No flags or arguments.");

    println!("    list [range]       - Display history metadata. Supports range (e.g., 0-50).");
    println!("                         Flags: --raw (-R), --full (-A), --id (-i).");
    
    println!("    search <keywords...> - Keyword scan metadata using SQLite indexing.");
    println!("                         Multiple keywords AND-match (space-separated or repeated args).");
    println!("                         Flags: --raw (-R), --id (-i).");
    
    println!("    copy-to <target>   - Restore record to clipboard via IPC synchronization.");
    println!("                         Accepts index or stable ID (via --id flag).");
    println!("                         Flags: --id (-i), --verbose (-v).");

    println!("\nDATA OPERATIONS:");
    println!("    show <target>      - Inspect record content and metadata.");
    println!("                         Flags: --raw (-R), --id (-i).");

    println!("    store [mime]       - Ingest stdin to storage and sync with active daemon.");
    println!("                         Flags: --verbose (-v).");
    
    println!("    paste-from [mime]  - Access system clipboard directly. Bypasses database.");

    println!("\nMANAGEMENT:");
    println!("    delete <target>    - Physically remove a specific record from persistent storage.");
    println!("                         Flags: --id (-i).");
    
    println!("    wipe               - Purge all history and execute SQLite VACUUM.");
    println!("                         Flags: --force (-f) [REQUIRED].");

    println!("\nGLOBAL OPTIONS:");
    println!("    -h, --help         - Show this help information.");
    println!("    -V, --version      - Show version information.");
    println!("    -v, --verbose      - Enable detailed system and transfer logging.");


    println!("\nPRACTICAL EXAMPLES:");
    println!("    # 1. High-speed selection with fzf using Stable IDs:");
    println!("    $ y4p list 0-100 --raw --id | fzf | awk '{{print $1}}' | xargs -r y4p copy-to --id");
    
    println!("\n    # 2. Extracting binary content from history:");
    println!("    $ y4p show 12 --id --raw > recovered_asset.webp");
    
    println!("\n    # 3. Manual ingestion with custom MIME:");
    println!("    $ cat data.json | y4p store application/json");

    println!("\nTECHNICAL NOTES:");
    println!("    - Storage: Secured at ~/.local/share/y4p/ (mode 600).");
    // BUGFIX: was documented as a fixed "/tmp/y4p.<uid>.sock" path;
    // the socket now prefers $XDG_RUNTIME_DIR (see core::get_socket_path)
    // and only falls back to /tmp when no session runtime dir is set.
    println!("    - IPC: Communication via $XDG_RUNTIME_DIR/y4p/y4p.sock");
    println!("           (falls back to /tmp/y4p.<uid>.sock if $XDG_RUNTIME_DIR is unset).");
    // BUGFIX: was documented as "MD5-based deduplication" — the actual
    // implementation (storage/mod.rs, matching the README/ARCHITECTURE.md)
    // uses SHA3-256, not MD5.
    println!("    - Engine: SQLite WAL mode with SHA3-256-based deduplication.");
    println!();
}
