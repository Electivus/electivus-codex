#!/bin/sh

# Immutable Electivus Installer protocol v1 bootstrap.
set -eu

SELECTOR="${CODEX_RELEASE:-stable}"
PROTOCOL="installer-v1"
REPOSITORY="Electivus/electivus-codex"
TAG_PREFIX="electivus-v"
GITHUB_API_BASE="https://api.github.com/repos/$REPOSITORY"
GITHUB_RELEASE_BASE="https://github.com/$REPOSITORY/releases/download"
METADATA_MAX_BYTES=1048576
MANIFEST_MAX_BYTES=1048576
INSTALLER_MAX_BYTES=4194304
MAX_RELEASE_PAGES=4

tmp_dir=""
requested_kind=""
requested_version=""
requested_tag=""
resolved_version=""
resolved_tag=""
resolved_channel=""
installer_digest=""
manifest_digest=""
download_pid=""
download_reader_pid=""
active_download_pipe=""
delegate_pid=""
delegate_starting=false
pending_signal_status=""
pending_signal_name=""
cleanup_done=false

usage() {
  cat <<'EOF'
Electivus Installer protocol v1

Usage: installer-v1.sh [--release stable|pre-release|VERSION]

Stable is the default. Exact versions accept bare SemVer or electivus-v... tags.
The verified product installer is always delegated noninteractively.
EOF
}

validate_version() {
  version="$1"
  version_without_line_breaks="$(printf '%s' "$version" | tr -d '\r\n')"
  if [ "$version_without_line_breaks" != "$version" ]; then
    echo "Invalid Electivus release version: values must not contain CR or LF characters." >&2
    return 1
  fi
  version_bytes="$(printf '%s' "$version" | LC_ALL=C wc -c | tr -d ' ')"
  if [ "$version_bytes" -gt 128 ]; then
    echo "Invalid Electivus release version: values must not exceed the 128-byte safety limit." >&2
    return 1
  fi
  semver_without_build="${version%%+*}"
  case "$semver_without_build" in
    *-*) core="${semver_without_build%%-*}"; prerelease="${semver_without_build#*-}" ;;
    *) core="$semver_without_build"; prerelease="" ;;
  esac

  if ! printf '%s\n' "$version" | grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?(\+[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$'; then
    echo "Invalid Electivus release version: $version. Expected a valid SemVer value." >&2
    return 1
  fi
  if [ -n "$prerelease" ] && printf '%s\n' "$prerelease" | tr '.' '\n' |
    grep -Eq '^0[0-9]+$'; then
    echo "Invalid Electivus release version: $version. Numeric pre-release identifiers cannot have leading zeroes." >&2
    return 1
  fi
  [ "$core" != "0.0.0" ] || {
    echo "Invalid Electivus release version: 0.0.0 is reserved for Fork development builds." >&2
    return 1
  }
}

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --release)
        [ "$#" -ge 2 ] || {
          echo "--release requires a value." >&2
          exit 1
        }
        SELECTOR="$2"
        shift
        ;;
      --help | -h)
        usage
        exit 0
        ;;
      *)
        echo "Unknown argument: $1" >&2
        exit 1
        ;;
    esac
    shift
  done
}

normalize_selector() {
  selector_without_line_breaks="$(printf '%s' "$SELECTOR" | tr -d '\r\n')"
  if [ "$selector_without_line_breaks" != "$SELECTOR" ]; then
    echo "Invalid Electivus release selector: values must not contain CR or LF characters." >&2
    return 1
  fi
  case "$SELECTOR" in
    stable | pre-release)
      requested_kind="$SELECTOR"
      requested_version=""
      requested_tag=""
      ;;
    latest | rust-v* | v* | "")
      echo "Invalid Electivus release selector: ${SELECTOR:-<empty>}. Use stable, pre-release, bare SemVer, or an electivus-v... tag." >&2
      return 1
      ;;
    "$TAG_PREFIX"*)
      requested_kind="exact"
      requested_version="${SELECTOR#"$TAG_PREFIX"}"
      validate_version "$requested_version"
      requested_tag="$TAG_PREFIX$requested_version"
      ;;
    *)
      requested_kind="exact"
      requested_version="$SELECTOR"
      validate_version "$requested_version"
      requested_tag="$TAG_PREFIX$requested_version"
      ;;
  esac
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "$1 is required by Electivus Installer protocol v1." >&2
    exit 1
  }
}

stop_active_download() {
  if [ -n "$download_reader_pid" ]; then
    kill "$download_reader_pid" 2>/dev/null || true
  fi
  if [ -n "$download_pid" ]; then
    kill "$download_pid" 2>/dev/null || true
  fi
  if [ -n "$download_reader_pid" ]; then
    kill -KILL "$download_reader_pid" 2>/dev/null || true
    wait "$download_reader_pid" 2>/dev/null || true
    download_reader_pid=""
  fi
  if [ -n "$download_pid" ]; then
    kill -KILL "$download_pid" 2>/dev/null || true
    wait "$download_pid" 2>/dev/null || true
    download_pid=""
  fi
  if [ -n "$active_download_pipe" ]; then
    rm -f "$active_download_pipe"
    active_download_pipe=""
  fi
}

download_file() {
  url="$1"
  output="$2"
  max_bytes="$3"

  download_pipe="$tmp_dir/download.$$.fifo"
  rm -f "$download_pipe"
  mkfifo "$download_pipe"
  active_download_pipe="$download_pipe"
  curl -fsSL --proto '=https' --proto-redir '=https' --tlsv1.2 \
    --connect-timeout 10 --max-time 300 "$url" >"$download_pipe" &
  download_pid=$!
  head_status=0
  head -c $((max_bytes + 1)) "$download_pipe" >"$output" &
  download_reader_pid=$!
  wait "$download_reader_pid" || head_status=$?
  download_reader_pid=""
  if [ "$head_status" -ne 0 ]; then
    stop_active_download
    rm -f "$output"
    return 1
  fi
  downloaded_bytes="$(wc -c <"$output" | tr -d ' ')"
  if [ "$downloaded_bytes" -gt "$max_bytes" ]; then
    stop_active_download
    rm -f "$output"
    echo "Download from $url exceeded the $max_bytes-byte safety limit." >&2
    return 1
  fi

  curl_status=0
  wait "$download_pid" || curl_status=$?
  download_pid=""
  rm -f "$download_pipe"
  active_download_pipe=""
  if [ "$curl_status" -ne 0 ]; then
    rm -f "$output"
    return 1
  fi
}

inventory_page_status() {
  python3 - "$1" <<'PY'
import json
from pathlib import Path
import sys


def reject_duplicate_keys(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON object key: {key}")
        result[key] = value
    return result


try:
    page = json.loads(
        Path(sys.argv[1]).read_text(encoding="utf-8"),
        object_pairs_hook=reject_duplicate_keys,
    )
except (OSError, UnicodeError, json.JSONDecodeError, ValueError):
    print("invalid")
    raise SystemExit
if not isinstance(page, list) or len(page) > 100:
    print("invalid")
elif len(page) < 100:
    print("last")
else:
    print("more")
PY
}

fetch_metadata() {
  metadata_dir="$tmp_dir/metadata"
  mkdir "$metadata_dir"
  if [ "$requested_kind" = "exact" ]; then
    metadata_url="$GITHUB_API_BASE/releases/tags/$requested_tag"
    download_file "$metadata_url" "$metadata_dir/exact.json" "$METADATA_MAX_BYTES" || {
      echo "Could not fetch published Electivus release metadata for $requested_tag." >&2
      return 1
    }
    return
  fi

  page=1
  while [ "$page" -le "$MAX_RELEASE_PAGES" ]; do
    metadata_path="$metadata_dir/page-$page.json"
    metadata_url="$GITHUB_API_BASE/releases?per_page=100&page=$page"
    download_file "$metadata_url" "$metadata_path" "$METADATA_MAX_BYTES" || {
      echo "Could not fetch bounded Electivus release inventory page $page." >&2
      return 1
    }
    page_status="$(inventory_page_status "$metadata_path")"
    case "$page_status" in
      last) return ;;
      more) ;;
      *)
        echo "Electivus release inventory page $page is malformed or exceeds 100 releases." >&2
        return 1
        ;;
    esac
    page=$((page + 1))
  done
  echo "Electivus release inventory exceeds the $MAX_RELEASE_PAGES-page safety limit." >&2
  return 1
}

resolve_release() {
  selection_path="$tmp_dir/selection"
  if ! python3 - "$requested_kind" "$requested_version" "$tmp_dir/metadata" >"$selection_path" <<'PY'
import functools
from datetime import datetime
import json
from pathlib import Path
import re
import sys

kind, requested_version, metadata_dir = sys.argv[1:]
required_assets = {
    "codex-package-aarch64-pc-windows-msvc.tar.gz",
    "codex-package-aarch64-unknown-linux-musl.tar.gz",
    "codex-package-x86_64-pc-windows-msvc.tar.gz",
    "codex-package-x86_64-unknown-linux-musl.tar.gz",
    "codex-package_SHA256SUMS",
    "install.sh",
    "install.ps1",
    "installer_SHA256SUMS",
}
semver_pattern = re.compile(
    r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)"
    r"(?:-((?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)"
    r"(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*))?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$"
)
published_at_pattern = re.compile(
    r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}"
    r"(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})"
)


def reject_duplicate_keys(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"duplicate JSON object key: {key}")
        result[key] = value
    return result


def parse_version(value):
    if not isinstance(value, str) or len(value.encode("utf-8")) > 128:
        return None
    match = semver_pattern.fullmatch(value)
    if match is None or value == "0.0.0":
        return None
    prerelease = tuple(match.group(4).split(".")) if match.group(4) else ()
    return (int(match.group(1)), int(match.group(2)), int(match.group(3))), prerelease


def compare_versions(left, right):
    left_core, left_pre = left[1]
    right_core, right_pre = right[1]
    if left_core != right_core:
        return (left_core > right_core) - (left_core < right_core)
    if not left_pre or not right_pre:
        return (not left_pre) - (not right_pre)
    for left_id, right_id in zip(left_pre, right_pre):
        if left_id == right_id:
            continue
        left_numeric, right_numeric = left_id.isdigit(), right_id.isdigit()
        if left_numeric and right_numeric:
            return (int(left_id) > int(right_id)) - (int(left_id) < int(right_id))
        if left_numeric != right_numeric:
            return -1 if left_numeric else 1
        return (left_id > right_id) - (left_id < right_id)
    return (len(left_pre) > len(right_pre)) - (len(left_pre) < len(right_pre))


def valid_published_at(value):
    if not isinstance(value, str) or published_at_pattern.fullmatch(value) is None:
        return False
    if int(value[11:13]) > 23 or int(value[14:16]) > 59 or int(value[17:19]) > 59:
        return False
    normalized = f"{value[:-1]}+00:00" if value.endswith("Z") else value
    try:
        datetime.fromisoformat(normalized)
    except ValueError:
        return False
    return True


def candidate(release):
    if not isinstance(release, dict) or release.get("draft") is not False:
        return None
    if not valid_published_at(release.get("published_at")):
        return None
    tag = release.get("tag_name")
    if not isinstance(tag, str) or not tag.startswith("electivus-v"):
        return None
    version = tag[len("electivus-v") :]
    parsed = parse_version(version)
    if parsed is None:
        return None
    prerelease = bool(parsed[1])
    if release.get("prerelease") is not prerelease:
        return None
    assets = release.get("assets")
    if not isinstance(assets, list) or len(assets) > 64:
        return None
    digests = {}
    for asset in assets:
        if not isinstance(asset, dict):
            return None
        name, digest = asset.get("name"), asset.get("digest")
        state, size = asset.get("state"), asset.get("size")
        if (
            not isinstance(name, str)
            or len(name.encode("utf-8")) > 256
            or name in digests
        ):
            return None
        if not isinstance(digest, str) or re.fullmatch(r"sha256:[0-9a-fA-F]{64}", digest) is None:
            return None
        if state != "uploaded" or isinstance(size, bool) or not isinstance(size, int) or size <= 0:
            return None
        if name.startswith("codex-package-") and name.endswith(".tar.gz"):
            size_limit = 1_073_741_824
        elif name in {"install.sh", "install.ps1"}:
            size_limit = 4_194_304
        elif name in {"codex-package_SHA256SUMS", "installer_SHA256SUMS"}:
            size_limit = 1_048_576
        else:
            size_limit = 1_073_741_824
        if size > size_limit:
            return None
        digests[name] = digest[len("sha256:") :].lower()
    if not required_assets.issubset(digests):
        return None
    channel = "pre-release" if prerelease else "stable"
    if kind == "exact" and version != requested_version:
        return None
    if kind != "exact" and channel != kind:
        return None
    return version, parsed, tag, channel, digests["install.sh"], digests["installer_SHA256SUMS"]


metadata_path = Path(metadata_dir)
try:
    if kind == "exact":
        documents = [
            json.loads(
                (metadata_path / "exact.json").read_text(encoding="utf-8"),
                object_pairs_hook=reject_duplicate_keys,
            )
        ]
    else:
        documents = []
        seen_inventory_releases = set()
        for path in sorted(metadata_path.glob("page-*.json")):
            page = json.loads(
                path.read_text(encoding="utf-8"),
                object_pairs_hook=reject_duplicate_keys,
            )
            if not isinstance(page, list):
                raise ValueError("inventory root is not an array")
            page_release_fingerprints = {
                json.dumps(
                    release,
                    ensure_ascii=False,
                    separators=(",", ":"),
                    sort_keys=True,
                )
                for release in page
            }
            if seen_inventory_releases.intersection(page_release_fingerprints):
                raise ValueError("duplicate release record across inventory pages")
            seen_inventory_releases.update(page_release_fingerprints)
            documents.extend(page)
except (OSError, UnicodeError, json.JSONDecodeError, ValueError) as error:
    print(f"Could not parse bounded Electivus release metadata: {error}", file=sys.stderr)
    raise SystemExit(1)

candidates = [item for release in documents if (item := candidate(release)) is not None]
if not candidates:
    if kind == "stable":
        print(
            "No complete stable Electivus release is published. "
            "To opt into pre-releases, run installer-v1.sh --release pre-release.",
            file=sys.stderr,
        )
    elif kind == "pre-release":
        print("No complete Electivus pre-release is published.", file=sys.stderr)
    else:
        print(
            f"Electivus release electivus-v{requested_version} is not published, valid, and complete.",
            file=sys.stderr,
        )
    raise SystemExit(1)

candidates.sort(key=functools.cmp_to_key(compare_versions))
selected = candidates[-1]
for item in candidates[:-1]:
    if compare_versions(item, selected) == 0 and item[0] != selected[0]:
        print(
            f"Electivus release inventory contains ambiguous equal-precedence versions {item[0]} and {selected[0]}.",
            file=sys.stderr,
        )
        raise SystemExit(1)
print("\n".join((selected[0], selected[2], selected[3], selected[4], selected[5])))
PY
  then
    return 1
  fi

  resolved_version="$(sed -n '1p' "$selection_path")"
  resolved_tag="$(sed -n '2p' "$selection_path")"
  resolved_channel="$(sed -n '3p' "$selection_path")"
  installer_digest="$(sed -n '4p' "$selection_path")"
  manifest_digest="$(sed -n '5p' "$selection_path")"
  [ -n "$resolved_version" ] && [ -n "$resolved_tag" ] &&
    [ -n "$resolved_channel" ] && [ -n "$installer_digest" ] &&
    [ -n "$manifest_digest" ] || {
    echo "Electivus release resolver returned incomplete output." >&2
    return 1
  }
}

file_sha256() {
  sha256sum "$1" | awk '{print $1}'
}

verify_digest() {
  path="$1"
  expected="$2"
  description="$3"
  actual="$(file_sha256 "$path")"
  [ "$actual" = "$expected" ] || {
    echo "$description SHA-256 mismatch: expected $expected, got $actual." >&2
    return 1
  }
}

manifest_installer_digest() {
  awk '
    $2 == "install.sh" && length($1) == 64 && $1 !~ /[^0-9a-fA-F]/ {
      digest = tolower($1)
      found++
    }
    END {
      if (found != 1) exit 1
      print digest
    }
  ' "$1"
}

download_and_verify_installer() {
  release_base="$GITHUB_RELEASE_BASE/$resolved_tag"
  manifest_path="$tmp_dir/installer_SHA256SUMS"
  installer_path="$tmp_dir/install.sh"
  download_file "$release_base/installer_SHA256SUMS" "$manifest_path" "$MANIFEST_MAX_BYTES"
  verify_digest "$manifest_path" "$manifest_digest" "Installer checksum manifest"
  manifest_install_digest="$(manifest_installer_digest "$manifest_path")" || {
    echo "installer_SHA256SUMS does not contain exactly one valid install.sh digest." >&2
    return 1
  }
  [ "$manifest_install_digest" = "$installer_digest" ] || {
    echo "SHA-256 digest disagreement for install.sh between GitHub release metadata and installer_SHA256SUMS." >&2
    return 1
  }
  download_file "$release_base/install.sh" "$installer_path" "$INSTALLER_MAX_BYTES"
  verify_digest "$installer_path" "$installer_digest" "Verified Electivus installer"
}

delegate() {
  delegate_starting=true
  CODEX_NON_INTERACTIVE=1 \
    CODEX_RELEASE="$resolved_version" \
    CODEX_UPDATE_CHANNEL="$resolved_channel" \
    CODEX_INSTALLER_PROTOCOL="$PROTOCOL" \
    CODEX_INSTALLER_DIGEST="$installer_digest" \
    python3 - "$tmp_dir/install.sh" \
    --release "$resolved_version" \
    --channel "$resolved_channel" \
    --installer-protocol "$PROTOCOL" \
    --installer-digest "$installer_digest" <<'PY' &
import os
import signal
import subprocess
import sys

installer_path, *arguments = sys.argv[1:]
child = None
signal_status = None
forwarded_signals = {
    signal.SIGHUP: signal.SIGHUP,
    signal.SIGINT: signal.SIGINT,
    signal.SIGUSR1: signal.SIGINT,
    signal.SIGTERM: signal.SIGTERM,
}


def force_child_exit(_received_signal, _frame):
    if child is not None:
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


def stop_child(received_signal, _frame):
    global signal_status
    forwarded_signal = forwarded_signals[received_signal]
    if signal_status is None:
        signal_status = 128 + forwarded_signal
    if child is None:
        raise SystemExit(signal_status)
    try:
        os.killpg(child.pid, forwarded_signal)
    except ProcessLookupError:
        pass
    signal.alarm(1)


signal.signal(signal.SIGALRM, force_child_exit)
for received_signal in forwarded_signals:
    signal.signal(received_signal, stop_child)

blocked_signals = set(forwarded_signals)
signal.pthread_sigmask(signal.SIG_BLOCK, blocked_signals)
try:
    child = subprocess.Popen(
        ["/bin/sh", installer_path, *arguments],
        start_new_session=True,
    )
finally:
    signal.pthread_sigmask(signal.SIG_UNBLOCK, blocked_signals)

returncode = child.wait()
signal.alarm(0)
if signal_status is not None:
    try:
        os.killpg(child.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    raise SystemExit(signal_status)
if returncode < 0:
    returncode = 128 - returncode
raise SystemExit(returncode)
PY
  delegate_pid=$!
  delegate_starting=false
  if [ -n "$pending_signal_name" ]; then
    handle_signal "$pending_signal_status" "$pending_signal_name"
  fi

  delegate_status=0
  wait "$delegate_pid" || delegate_status=$?
  delegate_pid=""
  return "$delegate_status"
}

parse_args "$@"
normalize_selector
case "$(uname -s)" in
  Linux) ;;
  Darwin)
    echo "Electivus Installer protocol v1 does not support macOS because no verified standalone macOS artifact is published." >&2
    exit 1
    ;;
  *)
    echo "Electivus Installer protocol v1 does not support this operating system." >&2
    exit 1
    ;;
esac
require_command curl
require_command python3
require_command sha256sum
require_command mktemp
require_command head
require_command mkfifo
tmp_dir="$(mktemp -d)"
cleanup() {
  [ "$cleanup_done" = false ] || return 0
  cleanup_done=true
  trap '' EXIT HUP INT TERM
  stop_active_delegate TERM
  stop_active_download
  rm -rf "$tmp_dir"
  trap - EXIT HUP INT TERM
}
stop_active_delegate() {
  signal_name="$1"
  [ -n "$delegate_pid" ] || return 0
  case "$signal_name" in
    INT) supervisor_signal=USR1 ;;
    HUP | TERM) supervisor_signal="$signal_name" ;;
    *) supervisor_signal=TERM ;;
  esac
  kill -s "$supervisor_signal" "$delegate_pid" 2>/dev/null || true
  wait "$delegate_pid" 2>/dev/null || true
  delegate_pid=""
}
handle_signal() {
  signal_status="$1"
  signal_name="$2"
  if [ "$delegate_starting" = true ]; then
    if [ -z "$pending_signal_name" ]; then
      pending_signal_status="$signal_status"
      pending_signal_name="$signal_name"
    fi
    return
  fi
  trap '' HUP INT TERM
  stop_active_delegate "$signal_name"
  cleanup
  exit "$signal_status"
}
trap cleanup EXIT
trap 'handle_signal 129 HUP' HUP
trap 'handle_signal 130 INT' INT
trap 'handle_signal 143 TERM' TERM

fetch_metadata
resolve_release
download_and_verify_installer
delegate
