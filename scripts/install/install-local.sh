#!/bin/sh

set -eu

BIN_DIR="${CODEX_INSTALL_DIR:-$HOME/.local/bin}"
BIN_PATH="$BIN_DIR/codex"
CODEX_HOME_DIR="${CODEX_HOME:-$HOME/.codex}"
STANDALONE_ROOT="$CODEX_HOME_DIR/packages/standalone"
RELEASES_DIR="$STANDALONE_ROOT/releases"
CURRENT_LINK="$STANDALONE_ROOT/current"
LOCK_FILE="$STANDALONE_ROOT/install.lock"
LOCK_DIR="$STANDALONE_ROOT/install.lock.d"
LOCK_STALE_AFTER_SECS=600
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/../.." && pwd)
CODEX_RS_DIR="$REPO_ROOT/codex-rs"

path_action="already"
path_profile=""
lock_kind=""
tmp_dir=""
python_bin=""
cargo_toml_backup=""
cargo_lock_backup=""
use_upstream_version=false

step() {
  printf '==> %s\n' "$1"
}

warn() {
  printf 'WARNING: %s\n' "$1" >&2
}

usage() {
  cat <<'EOF'
Usage: install-local.sh [--use-upstream-version]

  --use-upstream-version  Build with the upstream release or pre-release
                          version persisted by upstream sync instead of 0.0.0.

On Windows Git Bash/MSYS/Cygwin, this delegates to install-local.ps1.
EOF
}

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --help | -h)
        usage
        exit 0
        ;;
      --use-upstream-version)
        use_upstream_version=true
        ;;
      *)
        echo "Unknown argument: $1" >&2
        exit 1
        ;;
    esac
    shift
  done
}

require_command() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "$1 is required to install a local Codex debug build." >&2
    exit 1
  fi
}

resolve_python_bin() {
  if command -v python3 >/dev/null 2>&1; then
    printf 'python3\n'
    return
  fi

  if command -v python >/dev/null 2>&1; then
    printf 'python\n'
    return
  fi

  echo "python3 or python is required to install a local Codex debug build." >&2
  exit 1
}

python_with_scripts_path() {
  PYTHONPATH="$REPO_ROOT/scripts${PYTHONPATH:+:$PYTHONPATH}" "$python_bin" "$@"
}

read_workspace_version() {
  python_with_scripts_path -c 'from codex_package.version import read_workspace_version; print(read_workspace_version())'
}

resolve_upstream_build_version() {
  python_with_scripts_path -c 'from codex_package.version import resolve_upstream_build_version; print(resolve_upstream_build_version())'
}

set_workspace_version() {
  version="$1"
  python_with_scripts_path - "$CODEX_RS_DIR/Cargo.toml" "$version" <<'PY'
import sys
from pathlib import Path

from codex_package.version import replace_workspace_version

replace_workspace_version(Path(sys.argv[1]), sys.argv[2])
PY
}

backup_cargo_manifest_files() {
  cargo_toml="$CODEX_RS_DIR/Cargo.toml"
  cargo_lock="$CODEX_RS_DIR/Cargo.lock"

  cargo_toml_backup="$tmp_dir/Cargo.toml.original"
  cp "$cargo_toml" "$cargo_toml_backup"

  if [ -f "$cargo_lock" ]; then
    cargo_lock_backup="$tmp_dir/Cargo.lock.original"
    cp "$cargo_lock" "$cargo_lock_backup"
  else
    cargo_lock_backup="__missing__"
  fi
}

restore_cargo_manifest_files() {
  restore_failed=0

  if [ -n "$cargo_toml_backup" ]; then
    if ! cp "$cargo_toml_backup" "$CODEX_RS_DIR/Cargo.toml"; then
      restore_failed=1
    fi
  fi

  if [ -n "$cargo_lock_backup" ]; then
    if [ "$cargo_lock_backup" = "__missing__" ]; then
      if ! rm -f "$CODEX_RS_DIR/Cargo.lock"; then
        restore_failed=1
      fi
    elif ! cp "$cargo_lock_backup" "$CODEX_RS_DIR/Cargo.lock"; then
      restore_failed=1
    fi
  fi

  if [ "$restore_failed" -eq 0 ]; then
    cargo_toml_backup=""
    cargo_lock_backup=""
  fi

  return "$restore_failed"
}

prepare_upstream_build_version() {
  current_workspace_version="$(read_workspace_version)"
  upstream_build_version="$(resolve_upstream_build_version)"

  if [ "$upstream_build_version" = "$current_workspace_version" ]; then
    return
  fi

  step "Using upstream release version $upstream_build_version for local build"
  backup_cargo_manifest_files
  set_workspace_version "$upstream_build_version"
}

is_windows_uname() {
  case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

windows_path() {
  if command -v cygpath >/dev/null 2>&1; then
    cygpath -w "$1"
    return
  fi

  printf '%s\n' "$1"
}

run_windows_local_installer() {
  ps_script="$(windows_path "$SCRIPT_DIR/install-local.ps1")"

  if command -v pwsh >/dev/null 2>&1; then
    powershell_cmd="pwsh"
  elif command -v powershell.exe >/dev/null 2>&1; then
    powershell_cmd="powershell.exe"
  elif command -v powershell >/dev/null 2>&1; then
    powershell_cmd="powershell"
  else
    echo "PowerShell is required to install a local Codex debug build on Windows." >&2
    echo "Run scripts/install/install-local.ps1 from PowerShell, or install PowerShell and retry." >&2
    exit 1
  fi

  step "Detected Windows Git Bash/MSYS/Cygwin; using install-local.ps1"
  "$powershell_cmd" -NoProfile -ExecutionPolicy Bypass -File "$ps_script" "$@"
}

pick_profile() {
  case "$(uname -s):${SHELL:-}" in
    Darwin:*/zsh)
      printf '%s\n' "$HOME/.zprofile"
      ;;
    Darwin:*/bash)
      printf '%s\n' "$HOME/.bash_profile"
      ;;
    Linux:*/zsh)
      printf '%s\n' "$HOME/.zshrc"
      ;;
    Linux:*/bash)
      printf '%s\n' "$HOME/.bashrc"
      ;;
    *)
      printf '%s\n' "$HOME/.profile"
      ;;
  esac
}

append_path_block() {
  profile="$1"
  begin_marker="$2"
  end_marker="$3"
  path_line="$4"

  {
    printf '\n%s\n' "$begin_marker"
    printf '%s\n' "$path_line"
    printf '%s\n' "$end_marker"
  } >>"$profile"
}

rewrite_path_block() {
  profile="$1"
  begin_marker="$2"
  end_marker="$3"
  path_line="$4"
  tmp_profile="$tmp_dir/profile.$$.tmp"

  awk -v begin="$begin_marker" -v end="$end_marker" -v line="$path_line" '
    BEGIN {
      in_block = 0
      replaced = 0
    }
    $0 == begin {
      if (!replaced) {
        print begin
        print line
        print end
        replaced = 1
      }
      in_block = 1
      next
    }
    in_block {
      if ($0 == end) {
        in_block = 0
      }
      next
    }
    {
      print
    }
    END {
      if (in_block != 0) {
        exit 1
      }
    }
  ' "$profile" >"$tmp_profile"
  mv "$tmp_profile" "$profile"
}

add_to_path() {
  path_action="already"
  path_profile=""

  case ":$PATH:" in
    *":$BIN_DIR:"*)
      return
      ;;
  esac

  profile="$(pick_profile)"
  path_profile="$profile"
  begin_marker="# >>> Codex installer >>>"
  end_marker="# <<< Codex installer <<<"
  path_line="export PATH=\"$BIN_DIR:\$PATH\""

  if [ -f "$profile" ] && grep -F "$begin_marker" "$profile" >/dev/null 2>&1; then
    if grep -F "$path_line" "$profile" >/dev/null 2>&1; then
      path_action="configured"
      return
    fi

    if grep -F "$end_marker" "$profile" >/dev/null 2>&1; then
      rewrite_path_block "$profile" "$begin_marker" "$end_marker" "$path_line"
      path_action="updated"
      return
    fi
  fi

  append_path_block "$profile" "$begin_marker" "$end_marker" "$path_line"
  path_action="added"
}

print_launch_instructions() {
  case "$path_action" in
    added | updated | configured)
      step "Current terminal: export PATH=\"$BIN_DIR:\$PATH\" && codex"
      step "Future terminals: open a new terminal and run: codex"
      step "PATH was configured in $path_profile"
      ;;
    *)
      step "Current terminal: codex"
      step "Future terminals: open a new terminal and run: codex"
      ;;
  esac
}

mkdir_lock_is_stale() {
  [ -d "$LOCK_DIR" ] || return 1

  pid="$(cat "$LOCK_DIR/pid" 2>/dev/null || true)"
  started_at="$(cat "$LOCK_DIR/started_at" 2>/dev/null || true)"
  now="$(date +%s 2>/dev/null || printf '0')"

  case "$started_at" in
    '' | *[!0-9]*)
      started_at=0
      ;;
  esac

  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    return 1
  fi

  if [ "$started_at" -eq 0 ] || [ "$now" -eq 0 ]; then
    return 0
  fi

  [ $((now - started_at)) -ge "$LOCK_STALE_AFTER_SECS" ]
}

acquire_install_lock() {
  mkdir -p "$STANDALONE_ROOT"

  if [ "$(uname -s)" = "Darwin" ] && command -v lockf >/dev/null 2>&1; then
    : >>"$LOCK_FILE"
    exec 9<>"$LOCK_FILE"
    lockf 9
    lock_kind="lockf"
    return
  fi

  if command -v flock >/dev/null 2>&1; then
    exec 9>"$LOCK_FILE"
    flock 9
    lock_kind="flock"
    return
  fi

  while ! mkdir "$LOCK_DIR" 2>/dev/null; do
    if mkdir_lock_is_stale; then
      warn "Removing stale installer lock at $LOCK_DIR"
      rm -rf "$LOCK_DIR"
      continue
    fi
    sleep 1
  done

  printf '%s\n' "$$" >"$LOCK_DIR/pid"
  date +%s >"$LOCK_DIR/started_at" 2>/dev/null || true
  lock_kind="mkdir"
}

release_install_lock() {
  if [ "$lock_kind" = "mkdir" ]; then
    rm -rf "$LOCK_DIR" 2>/dev/null || true
  elif [ "$lock_kind" = "flock" ] || [ "$lock_kind" = "lockf" ]; then
    exec 9>&- 2>/dev/null || true
  fi
  lock_kind=""
}

cleanup_stale_install_artifacts() {
  mkdir -p "$RELEASES_DIR" "$STANDALONE_ROOT"

  find "$RELEASES_DIR" -mindepth 1 -maxdepth 1 -name '.staging.*' -exec rm -rf {} +
  find "$STANDALONE_ROOT" -mindepth 1 -maxdepth 1 -name '.current.*' -exec rm -f {} +
  find "$STANDALONE_ROOT" -mindepth 1 -maxdepth 1 -name '.swap-backup.*' -exec rm -rf {} +

  if [ -d "$BIN_DIR" ]; then
    find "$BIN_DIR" -mindepth 1 -maxdepth 1 -name '.codex.*' -exec rm -f {} +
    find "$BIN_DIR" -mindepth 1 -maxdepth 1 -name '.swap-backup.*' -exec rm -rf {} +
  fi
}

replace_path_with_symlink() {
  link_path="$1"
  link_target="$2"
  tmp_link="$3"
  backup_path="$(dirname "$link_path")/.swap-backup.$(basename "$link_path").$$"

  rm -rf "$tmp_link" "$backup_path"
  ln -s "$link_target" "$tmp_link"

  if mv -Tf "$tmp_link" "$link_path" 2>/dev/null; then
    return 0
  fi

  if mv -hf "$tmp_link" "$link_path" 2>/dev/null; then
    return 0
  fi

  if [ -L "$link_path" ] || [ -e "$link_path" ]; then
    if ! mv -f "$link_path" "$backup_path"; then
      rm -f "$tmp_link"
      echo "Failed to prepare replacement for $link_path." >&2
      return 1
    fi
  fi

  if mv -f "$tmp_link" "$link_path"; then
    rm -rf "$backup_path"
    return 0
  fi

  if [ -L "$backup_path" ] || [ -e "$backup_path" ]; then
    mv -f "$backup_path" "$link_path" 2>/dev/null || true
  fi
  rm -f "$tmp_link"
  echo "Failed to replace $link_path." >&2
  return 1
}

resolve_platform_target() {
  case "$(uname -s):$(uname -m)" in
    Linux:x86_64 | Linux:amd64)
      printf 'x86_64-unknown-linux-gnu\n'
      ;;
    Linux:arm64 | Linux:aarch64)
      printf 'aarch64-unknown-linux-gnu\n'
      ;;
    Darwin:x86_64 | Darwin:amd64)
      printf 'x86_64-apple-darwin\n'
      ;;
    Darwin:arm64 | Darwin:aarch64)
      printf 'aarch64-apple-darwin\n'
      ;;
    *)
      echo "Unsupported platform: $(uname -s) $(uname -m)" >&2
      exit 1
      ;;
  esac
}

generate_release_name() {
  release_prefix="$1"
  timestamp="$(date +%Y%m%d%H%M%S 2>/dev/null || date +%s)"

  printf "%s-%s-%s\n" "$release_prefix" "$timestamp" "$$"
}

build_local_package() {
  package_dir="$1"
  target="$2"

  step "Building local Codex debug package"
  rm -rf "$package_dir"
  # Keep fast dev builds while matching release behavior for recoverable
  # session-history invariants.
  CARGO_PROFILE_DEV_DEBUG_ASSERTIONS=false
  export CARGO_PROFILE_DEV_DEBUG_ASSERTIONS
  if [ -n "${CODEX_LOCAL_RG:-}" ]; then
    if [ ! -x "$CODEX_LOCAL_RG" ]; then
      echo "CODEX_LOCAL_RG must point to an executable rg." >&2
      return 1
    fi

    "$python_bin" "$REPO_ROOT/scripts/build_codex_package.py" \
      --target "$target" \
      --variant codex \
      --cargo-profile dev \
      --package-dir "$package_dir" \
      --rg-bin "$CODEX_LOCAL_RG" \
      --force
  else
    "$python_bin" "$REPO_ROOT/scripts/build_codex_package.py" \
      --target "$target" \
      --variant codex \
      --cargo-profile dev \
      --package-dir "$package_dir" \
      --force
  fi

  ln -sf "bin/codex" "$package_dir/codex"
}

release_dir_is_complete() {
  candidate_dir="$1"
  expected_target="$2"

  [ -f "$candidate_dir/codex-package.json" ] &&
    [ -x "$candidate_dir/bin/codex" ] &&
    [ -x "$candidate_dir/codex" ] &&
    [ -x "$candidate_dir/codex-path/rg" ] &&
    grep -E "\"target\"[[:space:]]*:[[:space:]]*\"$expected_target\"" "$candidate_dir/codex-package.json" >/dev/null 2>&1 ||
    return 1

  case "$expected_target" in
    *linux*) [ -x "$candidate_dir/codex-resources/bwrap" ] ;;
    *) true ;;
  esac
}

update_current_link() {
  release_dir="$1"
  tmp_link="$STANDALONE_ROOT/.current.$$"

  replace_path_with_symlink "$CURRENT_LINK" "$release_dir" "$tmp_link"
}

update_visible_command() {
  mkdir -p "$BIN_DIR"
  tmp_link="$BIN_DIR/.codex.$$"

  replace_path_with_symlink "$BIN_PATH" "$CURRENT_LINK/bin/codex" "$tmp_link"
}

verify_visible_command() {
  "$BIN_PATH" --version >/dev/null
}

prune_old_releases() {
  active_release="$1"

  "$python_bin" - "$RELEASES_DIR" "$active_release" <<'PY'
from pathlib import Path
import shutil
import sys


releases_dir = Path(sys.argv[1]).resolve()
active_release = Path(sys.argv[2]).resolve()
if active_release.parent != releases_dir or not active_release.is_dir():
    raise SystemExit(f"refusing to prune around invalid active release: {active_release}")

previous_releases = sorted(
    (
        path
        for path in releases_dir.iterdir()
        if path != active_release
        and not path.name.startswith(".")
        and path.is_dir()
        and not path.is_symlink()
    ),
    key=lambda path: (
        (
            (1, parts[-2], path.name)
            if len(parts := path.name.rsplit("-", 2)) == 3
            and len(parts[-2]) == 14
            and parts[-2].isdigit()
            else (0, f"{path.stat().st_mtime_ns:020d}", path.name)
        )
    ),
    reverse=True,
)
for old_release in previous_releases[2:]:
    shutil.rmtree(old_release)
    print(f"Removed old standalone release: {old_release.name}")
PY
}

parse_args "$@"

if is_windows_uname; then
  run_windows_local_installer "$@"
  exit $?
fi

require_command cargo
require_command mktemp
python_bin="$(resolve_python_bin)"

platform_target="$(resolve_platform_target)"
release_prefix="local-debug-$platform_target"
release_name="$(generate_release_name "$release_prefix")"
release_dir="$RELEASES_DIR/$release_name"

tmp_dir="$(mktemp -d)"
cleanup() {
  restore_cargo_manifest_files || warn "Failed to restore Cargo workspace version files"
  release_install_lock
  if [ -n "$tmp_dir" ]; then
    rm -rf "$tmp_dir"
  fi
}
trap cleanup EXIT INT TERM

acquire_install_lock
cleanup_stale_install_artifacts
step "Installing local debug build to $release_dir"
stage_release="$RELEASES_DIR/.staging.$release_name.$$"
if [ "$use_upstream_version" = true ]; then
  prepare_upstream_build_version
fi
build_local_package "$stage_release" "$platform_target"
restore_cargo_manifest_files
if ! release_dir_is_complete "$stage_release" "$platform_target"; then
  rm -rf "$stage_release"
  echo "Local release validation failed." >&2
  exit 1
fi
if [ -e "$release_dir" ] || [ -L "$release_dir" ]; then
  rm -rf "$release_dir"
fi
mv "$stage_release" "$release_dir"
update_current_link "$release_dir"
update_visible_command
add_to_path
verify_visible_command
prune_old_releases "$release_dir"
release_install_lock
printf 'Activated local release: %s\n' "$release_name"
print_launch_instructions
