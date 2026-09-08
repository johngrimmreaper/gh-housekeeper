# gh-housekeeper

Safe GitHub Actions artifact housekeeping with a Rust CLI and native GUI.

The project provides a shared policy engine for inventorying, visualizing,
classifying, protecting, and safely deleting GitHub Actions artifacts.

## Planned interfaces

- `gh-housekeeper` CLI
- Native Rust desktop GUI

## Core principles

- dry-run first
- explainable cleanup decisions
- protected repositories and artifacts
- configurable retention policies
- safe bulk deletion
- one shared policy engine for CLI and GUI
