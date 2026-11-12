#!/bin/bash
# memory-init.sh
# Initializes the memory repository

set -e

# ============================================================================
# Helper Functions
# ============================================================================

log() { echo "[memory-init] $1"; }
error() { echo "[memory-init] Error: $1" >&2; }
warn() { echo "[memory-init] Warning: $1" >&2; }

find_settings() {
  local project_settings="$(pwd)/.phi/settings.json"
  local config_dir="${PHI_CONFIG_DIR:-$HOME/.phi}"
  local global_settings="$config_dir/settings.json"

  if [ -f "$project_settings" ] && grep -q '"memory"' "$project_settings" 2>/dev/null; then
    echo "$project_settings"
  elif [ -f "$global_settings" ] && grep -q '"memory"' "$global_settings" 2>/dev/null; then
    echo "$global_settings"
  fi
}

get_project_name() {
  if git rev-parse --show-toplevel &>/dev/null; then
    git rev-parse --show-toplevel | xargs basename
  else
    basename "$(pwd)"
  fi
}

expand_home_path() {
  local path="$1"

  if [[ "$path" == "~" ]]; then
    printf '%s\n' "$HOME"
  elif [[ "$path" == "~/"* ]]; then
    printf '%s/%s\n' "$HOME" "${path:2}"
  else
    printf '%s\n' "$path"
  fi
}

get_safe_global_dirname() {
  local directory_name="$1"
  local trimmed_name
  local safe_directory_name

  trimmed_name=$(printf '%s' "$directory_name" | xargs)
  safe_directory_name=$(basename "$trimmed_name")
  safe_directory_name=$(printf '%s' "$safe_directory_name" | sed 's/^\.\+$/global/')

  if [ -z "$safe_directory_name" ]; then
    echo "global"
  else
    echo "$safe_directory_name"
  fi
}

# ============================================================================
# Main Logic
# ============================================================================

main() {
  log "Starting memory initialization..."
  
  # 1. Read settings
  SETTINGS_FILE=$(find_settings)
  if [ -z "$SETTINGS_FILE" ]; then
    error "Phi memory settings not found. Configure memory.memoryDir first."
    exit 1
  fi
  
  log "Using settings: $SETTINGS_FILE"
  
  # Extract values (support memoryDir structure with fallback to top-level)
  if command -v jq &> /dev/null; then
    # Check memoryDir first, then fallback to top-level
    REPO_URL=$(jq -r '.memory.memoryDir.repoUrl // .memory.repoUrl // empty' "$SETTINGS_FILE")
    LOCAL_PATH=$(jq -r '.memory.memoryDir.localPath // .memory.localPath // empty' "$SETTINGS_FILE")
    GLOBAL_MEMORY=$(jq -r '.memory.memoryDir.globalMemory // .memory.globalMemory // empty' "$SETTINGS_FILE")
  else
    warn "jq is not installed. Falling back to grep/sed parsing for settings. Install jq for more reliable JSON parsing."
    REPO_URL=$(grep -o '"repoUrl"[[:space:]]*:[[:space:]]*"[^"]*"' "$SETTINGS_FILE" | sed 's/.*:[[:space:]]*"\([^"]*\)"/\1/')
    LOCAL_PATH=$(grep -o '"localPath"[[:space:]]*:[[:space:]]*"[^"]*"' "$SETTINGS_FILE" | sed 's/.*:[[:space:]]*"\([^"]*\)"/\1/')
    GLOBAL_MEMORY=$(grep -o '"globalMemory"[[:space:]]*:[[:space:]]*"[^"]*"' "$SETTINGS_FILE" | sed 's/.*:[[:space:]]*"\([^"]*\)"/\1/')
  fi
  
  LOCAL_PATH="${LOCAL_PATH:-$HOME/.phi/memory}"
  LOCAL_PATH=$(expand_home_path "$LOCAL_PATH")
  
  if [ -z "$REPO_URL" ]; then
    error "repoUrl not configured in settings"
    exit 1
  fi
  
  # 2. Calculate directories
  PROJECT_NAME=$(get_project_name)
  PROJECT_DIR="$LOCAL_PATH/$PROJECT_NAME"
  GLOBAL_ENABLED=$([ -n "$GLOBAL_MEMORY" ] && echo "true" || echo "false")
  SAFE_GLOBAL_MEMORY=$(get_safe_global_dirname "$GLOBAL_MEMORY")
  GLOBAL_DIR="$LOCAL_PATH/$SAFE_GLOBAL_MEMORY"
  
  log "Project: $PROJECT_DIR"
  [ "$GLOBAL_ENABLED" = "true" ] && log "Global: $GLOBAL_DIR"
  
  # 3. Sync git repository
  if [ ! -d "$LOCAL_PATH" ]; then
    log "Cloning repository..."
    git clone "$REPO_URL" "$LOCAL_PATH"
  elif [ ! -d "$LOCAL_PATH/.git" ]; then
    error "Directory exists but is not a git repository: $LOCAL_PATH"
    exit 1
  else
    log "Syncing repository..."
    cd "$LOCAL_PATH"
    git fetch origin
    git pull origin main 2>/dev/null || git pull origin master 2>/dev/null || log "No remote changes"
  fi
  
  # 4. Create the Markdown memory directory structure.
  log "Creating directories..."
  mkdir -p "$PROJECT_DIR/core/project"

  if [ "$GLOBAL_ENABLED" = "true" ]; then
    mkdir -p "$GLOBAL_DIR"
  fi

  log "Memory initialized successfully!"
  log "  Project: $PROJECT_DIR"
  if [ "$GLOBAL_ENABLED" = "true" ]; then
    log "  Global: $GLOBAL_DIR"
  fi
}

main "$@"
