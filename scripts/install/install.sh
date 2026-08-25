#!/bin/sh

set -eu

RELEASE="${CODEX_RELEASE:-stable}"
UPDATE_CHANNEL="${CODEX_UPDATE_CHANNEL:-}"
INSTALLER_PROTOCOL="${CODEX_INSTALLER_PROTOCOL:-direct}"
INSTALLER_DIGEST="${CODEX_INSTALLER_DIGEST:-}"
NON_INTERACTIVE="${CODEX_NON_INTERACTIVE:-false}"
PUBLISHER="Electivus"
REPOSITORY="Electivus/electivus-codex"
TAG_PREFIX="electivus-v"
GITHUB_API_BASE="https://api.github.com/repos/$REPOSITORY"
GITHUB_RELEASE_BASE="https://github.com/$REPOSITORY/releases/download"
METADATA_MAX_BYTES=1048576
MANIFEST_MAX_BYTES=1048576
INSTALLER_MAX_BYTES=4194304
PACKAGE_MAX_BYTES=1073741824
MAX_RELEASE_PAGES=4

BIN_DIR="${CODEX_INSTALL_DIR:-$HOME/.local/bin}"
BIN_PATH="$BIN_DIR/codex"
CODE_MODE_HOST_BIN_PATH="$BIN_DIR/codex-code-mode-host"
CODEX_HOME_DIR="${CODEX_HOME:-$HOME/.codex}"
STANDALONE_ROOT="$CODEX_HOME_DIR/packages/standalone"
RELEASES_DIR="$STANDALONE_ROOT/releases/$PUBLISHER/electivus-codex"
CURRENT_LINK="$STANDALONE_ROOT/current"
LOCK_FILE="$STANDALONE_ROOT/install.lock"
LOCK_PATH="$STANDALONE_ROOT/install.lock.d"
LOCK_STALE_AFTER_SECS=600

path_action="already"
path_profile=""
conflict_manager=""
lock_kind=""
lock_owner_file=""
tmp_dir=""
download_pid=""
active_reclaim_marker=""
active_reclaim_guard=""

step() {
  printf '==> %s\n' "$1"
}

warn() {
  printf 'WARNING: %s\n' "$1" >&2
}

validate_version() {
  version="$1"
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

version_is_prerelease() {
  case "${1%%+*}" in
    *-*) return 0 ;;
    *) return 1 ;;
  esac
}

validate_channel() {
  case "$1" in
    stable | pre-release) ;;
    *)
      echo "Invalid Electivus Update channel: $1. Expected stable or pre-release." >&2
      return 1
      ;;
  esac
}

validate_installer_protocol() {
  case "$1" in
    direct) ;;
    *)
      if ! printf '%s\n' "$1" | grep -Eq '^installer-v[1-9][0-9]*$'; then
        echo "Invalid Installer protocol: $1. Expected direct or installer-vN." >&2
        return 1
      fi
      ;;
  esac
}

parse_args() {
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --release)
        if [ "$#" -lt 2 ]; then
          echo "--release requires a value." >&2
          exit 1
        fi
        RELEASE="$2"
        shift
        ;;
      --channel)
        if [ "$#" -lt 2 ]; then
          echo "--channel requires a value." >&2
          exit 1
        fi
        UPDATE_CHANNEL="$2"
        shift
        ;;
      --installer-protocol)
        if [ "$#" -lt 2 ]; then
          echo "--installer-protocol requires a value." >&2
          exit 1
        fi
        INSTALLER_PROTOCOL="$2"
        shift
        ;;
      --installer-digest)
        if [ "$#" -lt 2 ]; then
          echo "--installer-digest requires a SHA-256 digest." >&2
          exit 1
        fi
        INSTALLER_DIGEST="$2"
        shift
        ;;
      --help | -h)
        cat <<EOF
Usage: install.sh [--release stable|pre-release|VERSION] [--channel CHANNEL]

Environment:
  CODEX_RELEASE          stable, pre-release, bare SemVer, or electivus-v tag.
  CODEX_UPDATE_CHANNEL   Persisted channel for an exact install.
  CODEX_INSTALLER_PROTOCOL
                         Installer protocol recorded in the receipt.
  CODEX_INSTALLER_DIGEST Verified SHA-256 of the executing installer, supplied
                         by an immutable Installer protocol bootstrap.
  CODEX_NON_INTERACTIVE  Set to 1, true, or yes to skip prompts.
EOF
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

download_file() {
  url="$1"
  output="$2"
  max_bytes="${3:-$PACKAGE_MAX_BYTES}"

  require_command head
  require_command mkfifo
  download_pipe="$tmp_dir/download.$$.fifo"
  rm -f "$download_pipe"
  mkfifo "$download_pipe"

  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --proto '=https' --proto-redir '=https' --tlsv1.2 \
      --connect-timeout 10 --max-time 300 "$url" >"$download_pipe" &
  elif command -v wget >/dev/null 2>&1; then
    wget -q -t 1 -T 300 --https-only --secure-protocol=TLSv1_2 \
      -O "$download_pipe" "$url" &
  else
    rm -f "$download_pipe"
    echo "curl or wget is required to install Electivus Codex." >&2
    exit 1
  fi

  download_pid=$!
  head_status=0
  head -c $((max_bytes + 1)) "$download_pipe" >"$output" || head_status=$?
  if [ "$head_status" -ne 0 ]; then
    kill "$download_pid" 2>/dev/null || true
    wait "$download_pid" 2>/dev/null || true
    download_pid=""
    rm -f "$download_pipe" "$output"
    return 1
  fi
  downloaded_bytes="$(wc -c <"$output" | tr -d ' ')"
  if [ "$downloaded_bytes" -gt "$max_bytes" ]; then
    kill "$download_pid" 2>/dev/null || true
    wait "$download_pid" 2>/dev/null || true
    download_pid=""
    rm -f "$download_pipe"
    rm -f "$output"
    echo "Download from $url exceeded the $max_bytes-byte safety limit." >&2
    return 1
  fi

  transport_status=0
  wait "$download_pid" || transport_status=$?
  download_pid=""
  rm -f "$download_pipe"
  if [ "$transport_status" -ne 0 ]; then
    rm -f "$output"
    return 1
  fi
}

download_text() {
  url="$1"
  output="$tmp_dir/metadata.json"
  download_file "$url" "$output" "$METADATA_MAX_BYTES" || return 1
  cat "$output"
}

parse_release_metadata() {
  # Bound awk's record size so compact, single-line JSON stays fast on every
  # supported awk implementation. JSON strings cannot contain literal newlines,
  # so the record boundaries inserted by fold do not change the document.
  LC_ALL=C fold -b -w 4096 | LC_ALL=C awk '
    function reset_release() {
      release_tag = ""
      release_draft = ""
      release_prerelease = ""
      release_published_at = ""
    }

    function save_value(value, value_type) {
      if (object_depth == release_object_depth) {
        if (key == "tag_name") {
          release_tag = value_type == "string" ? value : ""
        } else if (key == "draft") {
          release_draft = value_type == "boolean" ? value : ""
        } else if (key == "prerelease") {
          release_prerelease = value_type == "boolean" ? value : ""
        } else if (key == "published_at") {
          release_published_at = value_type == "string" ? value : ""
        }
      } else if (object_depth == asset_object_depth) {
        if (key == "name") {
          asset_name = value_type == "string" ? value : ""
        } else if (key == "digest") {
          asset_digest = value_type == "string" ? value : ""
        } else if (key == "state") {
          asset_state = value_type == "string" ? value : ""
        } else if (key == "size") {
          asset_size = value_type == "number" ? value : ""
        }
      }
      expecting_value = 0
      primitive = ""
      key = ""
    }

    function finish_primitive() {
      if (expecting_value && primitive != "") {
        if (primitive ~ /^(true|false|null|-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?)$/) {
          if (primitive == "true" || primitive == "false") {
            primitive_type = "boolean"
          } else if (primitive == "null") {
            primitive_type = "null"
          } else {
            primitive_type = "number"
          }
          save_value(primitive, primitive_type)
        } else {
          invalid = 1
        }
      }
    }

    {
      for (i = 1; i <= length($0); i++) {
        char = substr($0, i, 1)

        if (root_closed && char !~ /[[:space:]]/) {
          invalid = 1
        }

        if (root_kind == "" && char !~ /[[:space:]]/) {
          if (char == "[") {
            root_kind = "array"
          } else if (char == "{") {
            root_kind = "object"
          } else {
            invalid = 1
          }
        }

        if (!in_string && root_kind == "array" && array_depth == 1 &&
            object_depth == 0 && char !~ /[[:space:]]/ &&
            char != "{" && char != "]" && char != ",") {
          invalid = 1
        }

        if (in_string) {
          if (escaped) {
            token = token "\\" char
            escaped = 0
          } else if (char == "\\") {
            escaped = 1
          } else if (char == "\"") {
            in_string = 0
            if (string_is_value) {
              save_value(token, "string")
            } else {
              pending_key = token
            }
          } else {
            token = token char
          }
          continue
        }

        if (char == "\"") {
          in_string = 1
          token = ""
          escaped = 0
          string_is_value = expecting_value
        } else if (char == ":" && pending_key != "") {
          key = pending_key
          pending_key = ""
          expecting_value = 1
          primitive = ""
        } else if (char == "{") {
          object_depth++
          if (release_object_depth == 0 &&
              ((root_kind == "object" && object_depth == 1 && array_depth == 0) ||
               (root_kind == "array" && object_depth == 1 && array_depth == 1))) {
            release_object_depth = object_depth
            reset_release()
          } else if (assets_array_depth != 0 &&
              array_depth == assets_array_depth &&
              asset_object_depth == 0) {
            asset_object_depth = object_depth
            asset_name = ""
            asset_digest = ""
            asset_state = ""
            asset_size = ""
          }
          expecting_value = 0
          primitive = ""
          key = ""
        } else if (char == "}") {
          finish_primitive()
          if (object_depth == asset_object_depth) {
            print "asset|" asset_name "|" asset_digest "|" asset_state "|" asset_size
            asset_object_depth = 0
            asset_name = ""
            asset_digest = ""
            asset_state = ""
            asset_size = ""
          }
          if (object_depth == release_object_depth) {
            print "release|" release_tag "|" release_draft "|" release_prerelease "|" release_published_at
            print "end"
            release_object_depth = 0
          }
          object_depth--
          if (root_kind == "object" && object_depth == 0) {
            root_closed = 1
          }
          expecting_value = 0
          primitive = ""
          key = ""
          pending_key = ""
        } else if (char == "[") {
          array_depth++
          if (expecting_value && key == "assets" &&
              object_depth == release_object_depth) {
            assets_array_depth = array_depth
          }
          expecting_value = 0
          primitive = ""
          key = ""
        } else if (char == "]") {
          finish_primitive()
          if (array_depth == assets_array_depth) {
            assets_array_depth = 0
          }
          array_depth--
          if (root_kind == "array" && array_depth == 0) {
            root_closed = 1
          }
          expecting_value = 0
          primitive = ""
          key = ""
          pending_key = ""
        } else if (char == ",") {
          finish_primitive()
          expecting_value = 0
          primitive = ""
          key = ""
          pending_key = ""
        } else if (expecting_value && char !~ /[[:space:]]/) {
          primitive = primitive char
        }
      }
    }

    END {
      if (invalid || root_kind == "" || !root_closed || in_string || object_depth != 0 ||
          array_depth != 0 || release_object_depth != 0) {
        exit 1
      }
    }
  '
}

release_url_for_asset() {
  asset="$1"
  release_tag="$2"

  printf '%s/%s/%s\n' "$GITHUB_RELEASE_BASE" "$release_tag" "$asset"
}

release_metadata_url() {
  release_tag="$1"

  printf '%s/releases/tags/%s\n' "$GITHUB_API_BASE" "$release_tag"
}

semver_compare() {
  LC_ALL=C awk -v left="$1" -v right="$2" '
    function numeric_compare(a, b) {
      sub(/^0+/, "", a)
      sub(/^0+/, "", b)
      if (a == "") a = "0"
      if (b == "") b = "0"
      if (length(a) != length(b)) return length(a) > length(b) ? 1 : -1
      if (a == b) return 0
      return a > b ? 1 : -1
    }
    function parse(version, core, pre, plus, dash) {
      plus = index(version, "+")
      if (plus) version = substr(version, 1, plus - 1)
      dash = index(version, "-")
      parsed_core = dash ? substr(version, 1, dash - 1) : version
      parsed_pre = dash ? substr(version, dash + 1) : ""
    }
    BEGIN {
      parse(left); left_core = parsed_core; left_pre = parsed_pre
      parse(right); right_core = parsed_core; right_pre = parsed_pre
      split(left_core, left_parts, ".")
      split(right_core, right_parts, ".")
      for (i = 1; i <= 3; i++) {
        comparison = numeric_compare(left_parts[i], right_parts[i])
        if (comparison) { print comparison; exit }
      }
      if (left_pre == "" && right_pre == "") { print 0; exit }
      if (left_pre == "") { print 1; exit }
      if (right_pre == "") { print -1; exit }
      left_count = split(left_pre, left_ids, ".")
      right_count = split(right_pre, right_ids, ".")
      count = left_count > right_count ? left_count : right_count
      for (i = 1; i <= count; i++) {
        if (i > left_count) { print -1; exit }
        if (i > right_count) { print 1; exit }
        left_numeric = left_ids[i] ~ /^[0-9]+$/
        right_numeric = right_ids[i] ~ /^[0-9]+$/
        if (left_numeric && right_numeric) {
          comparison = numeric_compare(left_ids[i], right_ids[i])
        } else if (left_numeric != right_numeric) {
          comparison = left_numeric ? -1 : 1
        } else if (left_ids[i] == right_ids[i]) {
          comparison = 0
        } else {
          comparison = left_ids[i] > right_ids[i] ? 1 : -1
        }
        if (comparison) { print comparison; exit }
      }
      print 0
    }
  '
}

asset_digest_in_metadata() {
  lookup_asset="$1"
  lookup_metadata="$2"

  printf '%s\n' "$lookup_metadata" | awk -F '|' -v asset="$lookup_asset" '
    $1 == "asset" && $2 == asset {
      count++
      digest = $3
    }
    END {
      if (count != 1 || digest !~ /^sha256:[0-9a-fA-F]{64}$/) {
        exit 1
      }
      sub(/^sha256:/, "", digest)
      print tolower(digest)
    }
  '
}

release_assets_are_complete() {
  complete_metadata="$1"

  printf '%s\n' "$complete_metadata" | LC_ALL=C awk -F '|' \
    -v package_limit="$PACKAGE_MAX_BYTES" \
    -v manifest_limit="$MANIFEST_MAX_BYTES" \
    -v installer_limit="$INSTALLER_MAX_BYTES" '
      $1 == "asset" {
        count++
        name = $2
        digest = $3
        state = $4
        size = $5
        if (name == "" || length(name) > 256 || seen[name]++) invalid = 1
        if (digest !~ /^sha256:[0-9a-fA-F]{64}$/) invalid = 1
        if (state != "uploaded" || size !~ /^[0-9]+$/ || size == 0) invalid = 1
        limit = package_limit
        if (name == "install.sh" || name == "install.ps1") {
          limit = installer_limit
        } else if (name == "codex-package_SHA256SUMS" ||
            name == "installer_SHA256SUMS") {
          limit = manifest_limit
        }
        if (size > limit) invalid = 1
      }
      END {
        if (invalid || count == 0 || count > 64) exit 1
      }
    ' || return 1

  for required_asset in \
    codex-package-aarch64-pc-windows-msvc.tar.gz \
    codex-package-aarch64-unknown-linux-musl.tar.gz \
    codex-package-x86_64-pc-windows-msvc.tar.gz \
    codex-package-x86_64-unknown-linux-musl.tar.gz \
    codex-package_SHA256SUMS \
    install.sh \
    install.ps1 \
    installer_SHA256SUMS; do
    asset_digest_in_metadata "$required_asset" "$complete_metadata" >/dev/null 2>&1 || return 1
  done
}

consider_release() {
  c_tag="$1"
  c_draft="$2"
  c_prerelease_flag="$3"
  c_published_at="$4"
  c_assets="$5"

  [ "$c_draft" = "false" ] || return 0
  [ -n "$c_published_at" ] && [ "$c_published_at" != "null" ] || return 0
  case "$c_tag" in
    "$TAG_PREFIX"*) c_version="${c_tag#"$TAG_PREFIX"}" ;;
    *) return 0 ;;
  esac
  validate_version "$c_version" >/dev/null 2>&1 || return 0

  if version_is_prerelease "$c_version"; then
    [ "$c_prerelease_flag" = "true" ] || return 0
    c_channel="pre-release"
  else
    [ "$c_prerelease_flag" = "false" ] || return 0
    c_channel="stable"
  fi

  release_assets_are_complete "$c_assets" || return 0
  case "$requested_kind" in
    exact)
      [ "$c_version" = "$requested_version" ] || return 0
      ;;
    stable)
      [ "$c_channel" = "stable" ] || return 0
      ;;
    pre-release)
      if [ "$c_channel" != "pre-release" ]; then
        [ "$c_channel" = "stable" ] || return 0
        [ "$installed_managed_channel" = "pre-release" ] || return 0
        version_is_prerelease "$installed_managed_version" || return 0
        installed_core="${installed_managed_version%%+*}"
        installed_core="${installed_core%%-*}"
        candidate_core="${c_version%%+*}"
        [ "$candidate_core" = "$installed_core" ] || return 0
        [ "$(semver_compare "$c_version" "$installed_managed_version")" -gt 0 ] || return 0
      fi
      ;;
  esac

  if [ -n "$selected_version" ]; then
    c_comparison="$(semver_compare "$c_version" "$selected_version")"
    if [ "$c_comparison" -lt 0 ]; then
      return 0
    fi
    if [ "$c_comparison" -eq 0 ] && [ "$c_version" != "$selected_version" ]; then
      echo "Electivus release inventory contains ambiguous equal-precedence versions $selected_version and $c_version." >&2
      return 1
    fi
  fi

  selected_version="$c_version"
  selected_tag="$c_tag"
  selected_channel="$c_channel"
  selected_release_metadata="$c_assets"
}

select_release_from_records() {
  parsed_records="$1"
  pending_assets=""
  record_tag=""
  record_draft=""
  record_prerelease=""
  record_published_at=""

  while IFS='|' read -r record_kind record_one record_two record_three record_four; do
    case "$record_kind" in
      asset)
        pending_assets="${pending_assets}${pending_assets:+
}$record_kind|$record_one|$record_two|$record_three|$record_four"
        ;;
      release)
        record_tag="$record_one"
        record_draft="$record_two"
        record_prerelease="$record_three"
        record_published_at="$record_four"
        ;;
      end)
        consider_release \
          "$record_tag" \
          "$record_draft" \
          "$record_prerelease" \
          "$record_published_at" \
          "$pending_assets" || return 1
        pending_assets=""
        record_tag=""
        ;;
    esac
  done <<EOF
$parsed_records
EOF
}

parse_release_document() {
  document="$1"
  description="$2"
  expected_root="$3"
  root_char="$(printf '%s' "$document" | LC_ALL=C awk '
    {
      for (i = 1; i <= length($0); i++) {
        char = substr($0, i, 1)
        if (char !~ /[[:space:]]/) {
          print char
          exit
        }
      }
    }
  ')"
  if [ "$root_char" != "$expected_root" ]; then
    case "$expected_root" in
      '[') expected_description="a JSON array" ;;
      *) expected_description="a JSON object" ;;
    esac
    echo "$description must be $expected_description." >&2
    return 1
  fi
  if ! parsed_document="$(printf '%s\n' "$document" | parse_release_metadata)"; then
    echo "Could not parse $description as bounded GitHub release metadata." >&2
    return 1
  fi
  printf '%s\n' "$parsed_document"
}

normalize_release_request() {
  validate_installer_protocol "$INSTALLER_PROTOCOL"
  if [ -n "$UPDATE_CHANNEL" ]; then
    validate_channel "$UPDATE_CHANNEL"
  fi

  case "$RELEASE" in
    stable | pre-release)
      requested_kind="$RELEASE"
      if [ -n "$UPDATE_CHANNEL" ] && [ "$UPDATE_CHANNEL" != "$requested_kind" ]; then
        echo "--release $RELEASE conflicts with Update channel $UPDATE_CHANNEL." >&2
        return 1
      fi
      requested_channel="$requested_kind"
      requested_version=""
      ;;
    latest | rust-v* | v* | "")
      echo "Invalid Electivus release selector: ${RELEASE:-<empty>}. Use stable, pre-release, bare SemVer, or an electivus-v... tag." >&2
      return 1
      ;;
    "$TAG_PREFIX"*)
      requested_kind="exact"
      requested_version="${RELEASE#"$TAG_PREFIX"}"
      validate_version "$requested_version"
      ;;
    *)
      requested_kind="exact"
      requested_version="$RELEASE"
      validate_version "$requested_version"
      ;;
  esac

  if [ "$requested_kind" = "exact" ]; then
    if [ -n "$UPDATE_CHANNEL" ]; then
      requested_channel="$UPDATE_CHANNEL"
    elif version_is_prerelease "$requested_version"; then
      requested_channel="pre-release"
    else
      requested_channel="stable"
    fi
    if [ "$requested_channel" = "stable" ] && version_is_prerelease "$requested_version"; then
      echo "Stable Update channel cannot install pre-release version $requested_version." >&2
      return 1
    fi
  fi
}

resolve_release() {
  normalize_release_request
  selected_version=""
  selected_tag=""
  selected_channel=""
  selected_release_metadata=""

  if [ "$requested_kind" = "exact" ]; then
    requested_tag="$TAG_PREFIX$requested_version"
    metadata_url="$(release_metadata_url "$requested_tag")"
    if ! release_json="$(download_text "$metadata_url")"; then
      echo "Could not fetch published Electivus release metadata for $requested_tag." >&2
      return 1
    fi
    records="$(parse_release_document "$release_json" "$requested_tag" '{')" || return 1
    select_release_from_records "$records" || return 1
  else
    page=1
    records=""
    inventory_count=0
    terminal_page=false
    while [ "$page" -le "$MAX_RELEASE_PAGES" ]; do
      metadata_url="$GITHUB_API_BASE/releases?per_page=100&page=$page"
      if ! release_json="$(download_text "$metadata_url")"; then
        echo "Could not fetch bounded Electivus release inventory page $page." >&2
        return 1
      fi
      page_records="$(parse_release_document "$release_json" "Electivus release inventory page $page" '[')" || return 1
      page_count="$(printf '%s\n' "$page_records" | awk -F '|' '$1 == "release" { count++ } END { print count + 0 }')"
      if [ "$page_count" -gt 100 ]; then
        echo "Electivus release inventory page $page exceeds 100 releases." >&2
        return 1
      fi
      inventory_count=$((inventory_count + page_count))
      if [ "$inventory_count" -gt $((MAX_RELEASE_PAGES * 100)) ]; then
        echo "Electivus release inventory exceeds the bounded 400-release safety limit." >&2
        return 1
      fi
      if [ -n "$page_records" ]; then
        records="${records}${records:+
}$page_records"
      fi
      if [ "$page_count" -lt 100 ]; then
        terminal_page=true
        break
      fi
      page=$((page + 1))
    done
    if [ "$terminal_page" != true ]; then
      echo "Electivus release inventory exceeds the $MAX_RELEASE_PAGES-page safety limit." >&2
      return 1
    fi
    select_release_from_records "$records" || return 1
  fi

  if [ -z "$selected_version" ]; then
    if [ "$requested_kind" = "stable" ]; then
      echo "No complete stable Electivus release is published. To opt into pre-releases, run install.sh --release pre-release." >&2
    elif [ "$requested_kind" = "pre-release" ]; then
      echo "No complete Electivus pre-release is published." >&2
    else
      echo "Electivus release $TAG_PREFIX$requested_version is not published, valid, and complete." >&2
    fi
    return 1
  fi

  resolved_version="$selected_version"
  release_tag="$selected_tag"
  release_metadata="$selected_release_metadata"
  resolved_channel="$requested_channel"
  if [ "$requested_kind" = "pre-release" ] && [ "$selected_channel" = "stable" ]; then
    resolved_channel="stable"
  fi
  select_release_assets
}

release_asset_digest_or_empty() {
  asset="$1"
  asset_digest_in_metadata "$asset" "$release_metadata"
}

release_asset_exists() {
  asset="$1"

  release_asset_digest_or_empty "$asset" >/dev/null 2>&1
}

release_asset_digest() {
  asset="$1"

  digest="$(release_asset_digest_or_empty "$asset" || true)"
  if [ -z "$digest" ]; then
    echo "Could not find one valid SHA-256 digest for Electivus release asset $asset." >&2
    exit 1
  fi

  printf '%s\n' "$digest"
}

select_release_assets() {
  package_asset="codex-package-$vendor_target.tar.gz"
  checksum_asset="codex-package_SHA256SUMS"
  installer_asset="install.sh"
  installer_checksum_asset="installer_SHA256SUMS"
  install_layout="package"
  asset="$package_asset"
  download_url="$(release_url_for_asset "$asset" "$release_tag")"
  checksum_url="$(release_url_for_asset "$checksum_asset" "$release_tag")"
  installer_checksum_url="$(release_url_for_asset "$installer_checksum_asset" "$release_tag")"
}

package_archive_digest() {
  asset="$1"
  manifest_path="$2"

  digest="$(awk -v asset="$asset" '
    $2 == asset && length($1) == 64 && $1 !~ /[^0-9a-fA-F]/ {
      digest = tolower($1)
      found++
    }
    END {
      if (found != 1) {
        exit 1
      }
      print digest
    }
  ' "$manifest_path" 2>/dev/null || true)"

  if [ -z "$digest" ]; then
    echo "Could not find SHA-256 digest for $asset in codex-package_SHA256SUMS." >&2
    return 1
  fi

  printf '%s\n' "$digest"
}

verify_manifest_assets() {
  manifest_path="$1"
  shift

  for manifest_asset in "$@"; do
    manifest_digest="$(package_archive_digest "$manifest_asset" "$manifest_path")" || return 1
    metadata_digest="$(release_asset_digest "$manifest_asset")"
    if [ "$manifest_digest" != "$metadata_digest" ]; then
      echo "SHA-256 digest disagreement for $manifest_asset between GitHub release metadata and $(basename "$manifest_path")." >&2
      return 1
    fi
  done
}

write_installation_receipt() {
  receipt_path="$1"
  receipt_package_digest="$2"
  receipt_installer_digest="$3"

  # This schema is intentionally explicit and shared with the Windows
  # installer and Rust installation-context consumers.
  cat >"$receipt_path" <<EOF
{
  "publisher": "$PUBLISHER",
  "repository": "$REPOSITORY",
  "tag": "$release_tag",
  "update_channel": "$resolved_channel",
  "target": "$vendor_target",
  "package_digest": "$receipt_package_digest",
  "installer_digest": "$receipt_installer_digest",
  "installer_protocol": "$INSTALLER_PROTOCOL"
}
EOF
}

receipt_string_field() {
  receipt_path="$1"
  receipt_key="$2"

  sed -n "s/.*\"$receipt_key\"[[:space:]]*:[[:space:]]*\"\([^\"]*\)\".*/\1/p" \
    "$receipt_path" | head -n 1
}

load_current_managed_receipt() {
  installed_managed_version=""
  installed_managed_channel=""
  receipt_path="$CURRENT_LINK/installation-receipt.json"
  [ -f "$receipt_path" ] || return 0

  receipt_publisher="$(receipt_string_field "$receipt_path" publisher)"
  receipt_repository="$(receipt_string_field "$receipt_path" repository)"
  receipt_tag="$(receipt_string_field "$receipt_path" tag)"
  receipt_channel="$(receipt_string_field "$receipt_path" update_channel)"
  receipt_target="$(receipt_string_field "$receipt_path" target)"
  receipt_package_digest="$(receipt_string_field "$receipt_path" package_digest)"
  receipt_installer_digest="$(receipt_string_field "$receipt_path" installer_digest)"
  receipt_protocol="$(receipt_string_field "$receipt_path" installer_protocol)"

  [ "$receipt_publisher" = "$PUBLISHER" ] &&
    [ "$receipt_repository" = "$REPOSITORY" ] &&
    [ "$receipt_target" = "$vendor_target" ] || return 0
  case "$receipt_tag" in
    "$TAG_PREFIX"*) receipt_version="${receipt_tag#"$TAG_PREFIX"}" ;;
    *) return 0 ;;
  esac
  validate_version "$receipt_version" >/dev/null 2>&1 || return 0
  validate_channel "$receipt_channel" >/dev/null 2>&1 || return 0
  validate_installer_protocol "$receipt_protocol" >/dev/null 2>&1 || return 0
  printf '%s\n' "$receipt_package_digest" | grep -Eq '^[0-9a-fA-F]{64}$' || return 0
  printf '%s\n' "$receipt_installer_digest" | grep -Eq '^[0-9a-fA-F]{64}$' || return 0
  if version_is_prerelease "$receipt_version"; then
    [ "$receipt_channel" = "pre-release" ] || return 0
  fi
  receipt_binary_version="$(current_installed_version)"
  [ "$receipt_binary_version" = "$receipt_version" ] || return 0

  installed_managed_version="$receipt_version"
  installed_managed_channel="$receipt_channel"
}

bind_installer_provenance() {
  metadata_installer_digest="$1"
  if [ ! -f "$0" ] || [ ! -r "$0" ]; then
    echo "Cannot prove the executing install.sh bytes at $0; refusing to write installer provenance." >&2
    return 1
  fi
  executing_installer_digest="$(file_sha256 "$0")"

  case "$INSTALLER_PROTOCOL" in
    direct)
      if [ -n "$INSTALLER_DIGEST" ]; then
        echo "A verified installer digest requires an immutable installer-vN protocol." >&2
        return 1
      fi
      ;;
    *)
      if [ -z "$INSTALLER_DIGEST" ]; then
        echo "Installer protocol $INSTALLER_PROTOCOL requires the exact verified installer digest from its bootstrap." >&2
        return 1
      fi
      printf '%s\n' "$INSTALLER_DIGEST" | grep -Eq '^[0-9a-fA-F]{64}$' || {
        echo "Invalid verified installer digest: expected 64 hexadecimal SHA-256 characters." >&2
        return 1
      }
      INSTALLER_DIGEST="$(printf '%s\n' "$INSTALLER_DIGEST" | tr 'A-F' 'a-f')"
      if [ "$INSTALLER_DIGEST" != "$metadata_installer_digest" ]; then
        echo "Verified installer digest disagrees with the selected Electivus release metadata." >&2
        return 1
      fi
      ;;
  esac

  if [ "$executing_installer_digest" != "$metadata_installer_digest" ]; then
    echo "The executing install.sh digest does not match the selected Electivus release metadata." >&2
    return 1
  fi
  if [ -n "$INSTALLER_DIGEST" ] && [ "$executing_installer_digest" != "$INSTALLER_DIGEST" ]; then
    echo "The executing install.sh digest does not match the bootstrap-verified digest." >&2
    return 1
  fi
  resolved_installer_digest="$executing_installer_digest"
}

receipt_matches() {
  release_dir="$1"
  expected_receipt="$2"

  [ -f "$release_dir/installation-receipt.json" ] &&
    cmp -s "$release_dir/installation-receipt.json" "$expected_receipt"
}

file_sha256() {
  path="$1"

  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$path" | awk '{print $1}'
    return
  fi

  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$path" | awk '{print $1}'
    return
  fi

  if command -v openssl >/dev/null 2>&1; then
    openssl dgst -sha256 "$path" | sed 's/^.*= //'
    return
  fi

  echo "sha256sum, shasum, or openssl is required to verify the Codex download." >&2
  exit 1
}

verify_archive_digest() {
  archive_path="$1"
  expected_digest="$2"
  actual_digest="$(file_sha256 "$archive_path")"

  if [ "$actual_digest" != "$expected_digest" ]; then
    echo "Downloaded Codex archive checksum did not match expected digest." >&2
    echo "expected: $expected_digest" >&2
    echo "actual:   $actual_digest" >&2
    return 1
  fi
}

require_command() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "$1 is required to install Codex." >&2
    exit 1
  fi
}

pick_profile() {
  # Use the same shell-specific split Homebrew documents because there is no
  # universal startup file across macOS/Linux login and interactive shells.
  case "$os:${SHELL:-}" in
    darwin:*/zsh)
      printf '%s\n' "$HOME/.zprofile"
      ;;
    darwin:*/bash)
      printf '%s\n' "$HOME/.bash_profile"
      ;;
    linux:*/zsh)
      printf '%s\n' "$HOME/.zshrc"
      ;;
    linux:*/bash)
      printf '%s\n' "$HOME/.bashrc"
      ;;
    *)
      printf '%s\n' "$HOME/.profile"
      ;;
  esac
}

add_to_path() {
  path_action="already"
  path_profile=""

  case ":$PATH:" in
    *":$BIN_DIR:"*)
      if [ -z "$conflict_manager" ]; then
        return
      fi
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

fallback_lock_is_stale() {
  stale_lock="$1"
  stale_threshold="$2"
  if [ -d "$stale_lock" ]; then
    stale_pid="$(cat "$stale_lock/pid" 2>/dev/null || true)"
    stale_started_at="$(cat "$stale_lock/started_at" 2>/dev/null || true)"
  elif [ -f "$stale_lock" ]; then
    stale_pid="$(sed -n '1p' "$stale_lock" 2>/dev/null || true)"
    stale_started_at="$(sed -n '2p' "$stale_lock" 2>/dev/null || true)"
  else
    return 1
  fi
  case "$stale_pid" in
    '' | *[!0-9]*) return 1 ;;
  esac
  if [ -n "$stale_pid" ] && kill -0 "$stale_pid" 2>/dev/null; then
    return 1
  fi
  if [ "$stale_threshold" -eq 0 ]; then
    return 0
  fi
  case "$stale_started_at" in
    '' | *[!0-9]*) return 1 ;;
  esac
  stale_now="$(date +%s 2>/dev/null || printf '0')"
  if [ "$stale_now" -eq 0 ]; then
    return 1
  fi
  [ $((stale_now - stale_started_at)) -ge "$stale_threshold" ]
}

try_claim_fallback_lock() {
  try_owner="$1"
  try_lock="$2"

  ln "$try_owner" "$try_lock" 2>/dev/null || return 1
  if [ -f "$try_lock" ] && cmp -s "$try_lock" "$try_owner"; then
    return 0
  fi

  # POSIX ln treats an existing directory as a destination directory. Remove
  # the hard link it created there and keep waiting for that legacy lock.
  rm -f "$try_lock/$(basename "$try_owner")" 2>/dev/null || true
  return 1
}

cleanup_stale_reclaim_markers() {
  cleanup_lock="$1"
  for cleanup_marker in "$cleanup_lock".reclaim.*; do
    [ -f "$cleanup_marker" ] || continue
    [ "$cleanup_marker" = "$cleanup_lock.reclaim.guard" ] && continue
    if fallback_lock_is_stale "$cleanup_marker" 0; then
      rm -f "$cleanup_marker" 2>/dev/null || true
    fi
  done
}

reclaim_barrier_exists() {
  barrier_candidate_lock="$1"
  for barrier_candidate in "$barrier_candidate_lock".reclaim.*; do
    [ -f "$barrier_candidate" ] && return 0
  done
  return 1
}

wait_for_reclaim_barrier() {
  barrier_lock="$1"
  while :; do
    cleanup_stale_reclaim_markers "$barrier_lock"
    barrier_guard="$barrier_lock.reclaim.guard"
    if [ -f "$barrier_guard" ] && fallback_lock_is_stale "$barrier_guard" 0; then
      echo "Stale reclaim guard at $barrier_guard requires manual removal; refusing an unsafe automatic takeover." >&2
      return 1
    fi
    reclaim_barrier_exists "$barrier_lock" || return 0
    sleep 1
  done
}

publish_reclaim_marker() {
  publish_lock="$1"
  publish_prepare="$(mktemp "$publish_lock.reclaim-prepare.XXXXXX")"
  publish_suffix="${publish_prepare##*.}"
  published_marker="$publish_lock.reclaim.$publish_suffix"
  {
    printf '%s\n' "$$"
    date +%s 2>/dev/null || printf '0\n'
  } >"$publish_prepare"
  mv "$publish_prepare" "$published_marker"
  printf '%s\n' "$published_marker"
}

acquire_reclaim_guard() {
  guard_lock="$1"
  guard_marker="$2"
  reclaim_guard="$guard_lock.reclaim.guard"
  while ! ln "$guard_marker" "$reclaim_guard" 2>/dev/null; do
    if [ -f "$reclaim_guard" ] && fallback_lock_is_stale "$reclaim_guard" 0; then
      echo "Stale reclaim guard at $reclaim_guard requires manual removal; refusing an unsafe automatic takeover." >&2
      return 1
    fi
    sleep 1
  done
  if [ ! -f "$reclaim_guard" ] || ! cmp -s "$reclaim_guard" "$guard_marker"; then
    return 1
  fi
  active_reclaim_guard="$reclaim_guard"
}

release_reclaim_guard() {
  guard_marker="$1"
  if [ -n "$active_reclaim_guard" ] &&
    [ -f "$active_reclaim_guard" ] &&
    cmp -s "$active_reclaim_guard" "$guard_marker"; then
    rm -f "$active_reclaim_guard" 2>/dev/null || true
  fi
  active_reclaim_guard=""
}

reclaim_fallback_lock() {
  reclaim_lock="$1"
  reclaim_owner_prefix="$2"
  reclaim_stale_threshold="$3"
  active_reclaim_marker="$(publish_reclaim_marker "$reclaim_lock")" || return 1
  reclaim_suffix="${active_reclaim_marker##*.}"
  if ! acquire_reclaim_guard "$reclaim_lock" "$active_reclaim_marker"; then
    rm -f "$active_reclaim_marker" 2>/dev/null || true
    active_reclaim_marker=""
    return 1
  fi

  if [ -d "$reclaim_lock" ]; then
    if fallback_lock_is_stale "$reclaim_lock" "$reclaim_stale_threshold"; then
      reclaimed_lock="$reclaim_lock.stale.$reclaim_suffix"
      if mv "$reclaim_lock" "$reclaimed_lock" 2>/dev/null; then
        rm -rf "$reclaimed_lock"
      fi
    fi
  elif [ -f "$reclaim_lock" ]; then
    reclaimed_lock="$reclaim_lock.snapshot.$reclaim_suffix"
    if ln "$reclaim_lock" "$reclaimed_lock" 2>/dev/null; then
      if fallback_lock_is_stale "$reclaimed_lock" "$reclaim_stale_threshold"; then
        reclaimed_owner="$(sed -n '3p' "$reclaimed_lock" 2>/dev/null || true)"
        rm -f "$reclaim_lock" 2>/dev/null || true
        case "$reclaimed_owner" in
          "$reclaim_owner_prefix"*) rm -f "$reclaimed_owner" 2>/dev/null || true ;;
        esac
      fi
      rm -f "$reclaimed_lock" 2>/dev/null || true
    fi
  fi
  release_reclaim_guard "$active_reclaim_marker"
  rm -f "$active_reclaim_marker" 2>/dev/null || true
  active_reclaim_marker=""
}

acquire_fallback_lock() {
  claim_owner="$1"
  claim_lock="$2"
  claim_owner_prefix="$3"
  claim_stale_threshold="$4"
  claim_description="$5"

  while :; do
    wait_for_reclaim_barrier "$claim_lock"
    if try_claim_fallback_lock "$claim_owner" "$claim_lock"; then
      wait_for_reclaim_barrier "$claim_lock"
      if [ -f "$claim_lock" ] && cmp -s "$claim_lock" "$claim_owner"; then
        return
      fi
      continue
    fi
    if fallback_lock_is_stale "$claim_lock" "$claim_stale_threshold"; then
      warn "Removing stale $claim_description lock at $claim_lock"
      reclaim_fallback_lock \
        "$claim_lock" "$claim_owner_prefix" "$claim_stale_threshold" || true
      continue
    fi
    sleep 1
  done
}

acquire_install_lock() {
  mkdir -p "$STANDALONE_ROOT"

  if [ "$os" = "darwin" ] && command -v lockf >/dev/null 2>&1; then
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

  lock_owner_file="$(mktemp "$STANDALONE_ROOT/install.lock.owner.XXXXXX")"
  {
    printf '%s\n' "$$"
    date +%s 2>/dev/null || printf '0\n'
    printf '%s\n' "$lock_owner_file"
  } >"$lock_owner_file"

  acquire_fallback_lock \
    "$lock_owner_file" \
    "$LOCK_PATH" \
    "$STANDALONE_ROOT/install.lock.owner." \
    "$LOCK_STALE_AFTER_SECS" \
    installer
  lock_kind="hardlink"
}

release_install_lock() {
  if [ "$lock_kind" = "flock" ] || [ "$lock_kind" = "lockf" ]; then
    exec 9>&- 2>/dev/null || true
  fi
  if [ -n "$lock_owner_file" ]; then
    if [ -f "$LOCK_PATH" ] && cmp -s "$LOCK_PATH" "$lock_owner_file"; then
      rm -f "$LOCK_PATH" 2>/dev/null || true
    fi
    rm -f "$lock_owner_file" 2>/dev/null || true
  fi
  if [ -n "$active_reclaim_guard" ]; then
    rm -f "$active_reclaim_guard" 2>/dev/null || true
    active_reclaim_guard=""
  fi
  if [ -n "$active_reclaim_marker" ]; then
    rm -f "$active_reclaim_marker" 2>/dev/null || true
    active_reclaim_marker=""
  fi
  lock_kind=""
  lock_owner_file=""
}

cleanup_stale_install_artifacts() {
  mkdir -p "$RELEASES_DIR" "$STANDALONE_ROOT"

  find "$RELEASES_DIR" -mindepth 1 -maxdepth 1 -name '.staging.*' -exec rm -rf {} +
  find "$STANDALONE_ROOT" -mindepth 1 -maxdepth 1 -name '.current.*' -exec rm -f {} +

  if [ -d "$BIN_DIR" ]; then
    find "$BIN_DIR" -mindepth 1 -maxdepth 1 -name '.codex.*' -exec rm -f {} +
  fi
}

replace_path_with_symlink() {
  link_path="$1"
  link_target="$2"
  tmp_link="$3"

  rm -f "$tmp_link"
  ln -s "$link_target" "$tmp_link"

  if mv -Tf "$tmp_link" "$link_path" 2>/dev/null; then
    return
  fi

  if mv -hf "$tmp_link" "$link_path" 2>/dev/null; then
    return
  fi

  rm -f "$link_path"
  mv -f "$tmp_link" "$link_path"
}

version_from_binary() {
  codex_path="$1"

  if [ ! -x "$codex_path" ]; then
    return 1
  fi

  "$codex_path" --version 2>/dev/null | sed -n 's/.* \([0-9][0-9A-Za-z.+-]*\)$/\1/p' | head -n 1
}

current_installed_version() {
  version="$(version_from_binary "$CURRENT_LINK/bin/codex" || true)"
  if [ -n "$version" ]; then
    printf '%s\n' "$version"
    return 0
  fi

  version="$(version_from_binary "$CURRENT_LINK/codex" || true)"
  if [ -n "$version" ]; then
    printf '%s\n' "$version"
    return 0
  fi

  return 0
}

resolve_existing_codex() {
  command -v codex 2>/dev/null || true
}

classify_existing_codex() {
  existing_path="$1"

  if [ -z "$existing_path" ] || [ "$existing_path" = "$BIN_PATH" ]; then
    return 1
  fi

  case "$existing_path" in
    /opt/homebrew/* | /usr/local/*)
      if [ "$os" = "darwin" ]; then
        printf 'brew\n'
        return 0
      fi
      ;;
  esac

  if [ -f "$existing_path" ] && grep -F "#!/usr/bin/env node" "$existing_path" >/dev/null 2>&1; then
    case "$existing_path" in
      *".bun"*)
        printf 'bun\n'
        ;;
      *)
        printf 'npm\n'
        ;;
    esac
    return 0
  fi

  return 1
}

prompt_yes_no() {
  prompt="$1"

  case "$NON_INTERACTIVE" in
    1 | [Tt][Rr][Uu][Ee] | [Yy][Ee][Ss])
      return 1
      ;;
  esac

  if ( : </dev/tty ) 2>/dev/null; then
    printf '%s [y/N] ' "$prompt" >/dev/tty
    if ! IFS= read -r answer </dev/tty; then
      return 1
    fi
  elif [ -t 0 ]; then
    printf '%s [y/N] ' "$prompt"
    if ! IFS= read -r answer; then
      return 1
    fi
  else
    return 1
  fi

  case "$answer" in
    y | Y | yes | YES)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

print_launch_instructions() {
  case "$path_action" in
    added)
      step "Current terminal: export PATH=\"$BIN_DIR:\$PATH\" && codex"
      step "Future terminals: open a new terminal and run: codex"
      step "PATH was added to $path_profile"
      ;;
    updated)
      step "Current terminal: export PATH=\"$BIN_DIR:\$PATH\" && codex"
      step "Future terminals: open a new terminal and run: codex"
      step "PATH was updated in $path_profile"
      ;;
    configured)
      step "Current terminal: export PATH=\"$BIN_DIR:\$PATH\" && codex"
      step "Future terminals: open a new terminal and run: codex"
      step "PATH is already configured in $path_profile"
      ;;
    *)
      step "Current terminal: codex"
      step "Future terminals: open a new terminal and run: codex"
      ;;
  esac
}

maybe_launch_codex_now() {
  if prompt_yes_no "Start Codex now?"; then
    step "Launching Codex"
    "$BIN_PATH"
  fi
}

detect_conflicting_install() {
  existing_path="$(resolve_existing_codex)"
  manager="$(classify_existing_codex "$existing_path" || true)"

  if [ -z "$manager" ]; then
    return
  fi

  conflict_manager="$manager"
  step "Detected existing $manager-managed Codex at $existing_path"
  warn "Multiple managed Codex installs can be ambiguous because PATH order decides which one runs."
}

handle_conflicting_install() {
  if [ -z "$conflict_manager" ]; then
    return
  fi

  case "$conflict_manager" in
    brew)
      uninstall_cmd="brew uninstall --cask codex"
      ;;
    bun)
      uninstall_cmd="bun remove -g @openai/codex"
      ;;
    *)
      uninstall_cmd="npm uninstall -g @openai/codex"
      ;;
  esac

  if prompt_yes_no "Uninstall the existing $conflict_manager-managed Codex now?"; then
    step "Running: $uninstall_cmd"
    if ! sh -c "$uninstall_cmd"; then
      warn "Failed to uninstall the existing $conflict_manager-managed Codex. Continuing with the standalone install."
    fi
  else
    warn "Leaving the existing $conflict_manager-managed Codex installed. PATH order will determine which codex runs."
  fi
}

install_package_release() {
  release_dir="$1"
  archive_path="$2"
  receipt_path="$3"
  stage_release="$RELEASES_DIR/.staging.$(basename "$release_dir").$$"

  mkdir -p "$RELEASES_DIR" "$(dirname "$release_dir")"
  rm -rf "$stage_release"
  mkdir -p "$stage_release"
  tar -xzf "$archive_path" -C "$stage_release"
  chmod 0755 \
    "$stage_release/bin/codex" \
    "$stage_release/bin/codex-code-mode-host" \
    "$stage_release/codex-path/rg"
  if [ -f "$stage_release/codex-resources/bwrap" ]; then
    chmod 0755 "$stage_release/codex-resources/bwrap"
  fi
  ln -sf "bin/codex" "$stage_release/codex"
  cp "$receipt_path" "$stage_release/installation-receipt.json"

  if [ -e "$release_dir" ] || [ -L "$release_dir" ]; then
    rm -rf "$release_dir"
  fi
  mv "$stage_release" "$release_dir"
}

release_dir_is_complete() {
  release_dir="$1"
  expected_version="$2"
  expected_target="$3"
  layout="$4"
  expected_receipt="$5"

  [ -d "$release_dir" ] &&
    [ "$(basename "$release_dir")" = "$expected_target" ] &&
    [ "$(basename "$(dirname "$release_dir")")" = "$expected_version" ] ||
    return 1

  case "$layout" in
    package)
      [ -f "$release_dir/codex-package.json" ] &&
        [ -x "$release_dir/bin/codex" ] &&
        [ -x "$release_dir/bin/codex-code-mode-host" ] &&
        [ -x "$release_dir/codex" ] &&
        [ -x "$release_dir/codex-path/rg" ] ||
        return 1
      ;;
    *)
      return 1
      ;;
  esac

  case "$layout:$expected_target" in
    package:*linux*)
      [ -x "$release_dir/codex-resources/bwrap" ] || return 1
      ;;
  esac

  receipt_matches "$release_dir" "$expected_receipt" || return 1

  installed_version="$(version_from_binary "$release_dir/bin/codex" || version_from_binary "$release_dir/codex" || true)"
  [ "$installed_version" = "$expected_version" ]
}

save_activation_path() {
  saved_path="$1"
  saved_name="$2"
  saved_type_path="$tmp_dir/$saved_name.type"
  saved_value_path="$tmp_dir/$saved_name.value"

  if [ -L "$saved_path" ]; then
    printf 'link\n' >"$saved_type_path"
    readlink "$saved_path" >"$saved_value_path"
  elif [ -f "$saved_path" ]; then
    printf 'file\n' >"$saved_type_path"
    cp -p "$saved_path" "$saved_value_path"
  else
    printf 'absent\n' >"$saved_type_path"
  fi
}

restore_activation_path() {
  restored_path="$1"
  restored_name="$2"
  restored_type="$(cat "$tmp_dir/$restored_name.type")"

  rm -f "$restored_path"
  case "$restored_type" in
    link)
      ln -s "$(cat "$tmp_dir/$restored_name.value")" "$restored_path"
      ;;
    file)
      cp -p "$tmp_dir/$restored_name.value" "$restored_path"
      ;;
    absent) ;;
  esac
}

activate_release() {
  release_dir="$1"
  save_activation_path "$CURRENT_LINK" current
  save_activation_path "$BIN_PATH" visible-codex
  save_activation_path "$CODE_MODE_HOST_BIN_PATH" visible-code-mode-host

  if update_current_link "$release_dir" &&
    update_visible_command "$release_dir" &&
    verify_visible_command; then
    return 0
  fi

  warn "Activation failed; restoring the previous runnable installation."
  restore_activation_path "$CURRENT_LINK" current
  restore_activation_path "$BIN_PATH" visible-codex
  restore_activation_path "$CODE_MODE_HOST_BIN_PATH" visible-code-mode-host
  return 1
}

update_current_link() {
  release_dir="$1"
  tmp_link="$STANDALONE_ROOT/.current.$$"

  replace_path_with_symlink "$CURRENT_LINK" "$release_dir" "$tmp_link"
}

release_codex_relative_path() {
  release_dir="$1"

  if [ -x "$release_dir/bin/codex" ]; then
    printf 'bin/codex\n'
  else
    printf 'codex\n'
  fi
}

update_visible_command() {
  release_dir="$1"
  mkdir -p "$BIN_DIR"
  tmp_link="$BIN_DIR/.codex.$$"
  codex_relative_path="$(release_codex_relative_path "$release_dir")"

  replace_path_with_symlink "$BIN_PATH" "$CURRENT_LINK/$codex_relative_path" "$tmp_link"

  if [ "$os" = "darwin" ] && [ -x "$release_dir/bin/codex-code-mode-host" ]; then
    replace_path_with_symlink \
      "$CODE_MODE_HOST_BIN_PATH" \
      "$CURRENT_LINK/bin/codex-code-mode-host" \
      "$tmp_link"
  elif [ "$(readlink "$CODE_MODE_HOST_BIN_PATH" 2>/dev/null || true)" = \
    "$CURRENT_LINK/bin/codex-code-mode-host" ]; then
    rm -f "$CODE_MODE_HOST_BIN_PATH"
  fi
}

verify_visible_command() {
  "$BIN_PATH" --version >/dev/null || return 1
  if [ "$os" = "darwin" ] && [ "$install_layout" = "package" ]; then
    [ -x "$CODE_MODE_HOST_BIN_PATH" ]
  fi
}

parse_args "$@"

require_command mktemp
require_command tar
require_command cmp

case "$(uname -s)" in
  Darwin)
    echo "Electivus does not yet publish or validate standalone macOS artifacts. This installer will not fall back to OpenAI Codex." >&2
    exit 1
    ;;
  Linux)
    os="linux"
    ;;
  *)
    echo "install.sh supports Linux only. Use the Electivus install.ps1 on Windows." >&2
    exit 1
    ;;
esac

case "$(uname -m)" in
  x86_64 | amd64)
    arch="x86_64"
    ;;
  arm64 | aarch64)
    arch="aarch64"
    ;;
  *)
    echo "Unsupported architecture: $(uname -m)" >&2
    exit 1
    ;;
esac

if [ "$arch" = "aarch64" ]; then
  vendor_target="aarch64-unknown-linux-musl"
  platform_label="Linux (ARM64)"
else
  vendor_target="x86_64-unknown-linux-musl"
  platform_label="Linux (x64)"
fi

tmp_dir="$(mktemp -d)"
cleanup() {
  if [ -n "$download_pid" ]; then
    kill "$download_pid" 2>/dev/null || true
    wait "$download_pid" 2>/dev/null || true
    download_pid=""
  fi
  release_install_lock
  if [ -n "$tmp_dir" ]; then
    rm -rf "$tmp_dir"
  fi
}
trap cleanup EXIT INT TERM

load_current_managed_receipt
resolve_release
release_dir="$RELEASES_DIR/$resolved_version/$vendor_target"
package_metadata_digest="$(release_asset_digest "$package_asset")"
installer_metadata_digest="$(release_asset_digest "$installer_asset")"
bind_installer_provenance "$installer_metadata_digest"
if [ -n "$installed_managed_version" ] &&
  [ "$(semver_compare "$resolved_version" "$installed_managed_version")" -lt 0 ]; then
  echo "Refusing to downgrade managed Electivus Codex from $installed_managed_version to $resolved_version." >&2
  exit 1
fi
expected_receipt="$tmp_dir/installation-receipt.json"
write_installation_receipt \
  "$expected_receipt" \
  "$package_metadata_digest" \
  "$resolved_installer_digest"
current_version="$(current_installed_version)"

if [ -n "$current_version" ] && [ "$current_version" != "$resolved_version" ]; then
  step "Updating Codex CLI from $current_version to $resolved_version"
elif [ -n "$current_version" ]; then
  step "Updating Codex CLI"
else
  step "Installing Codex CLI"
fi
step "Detected platform: $platform_label"
step "Resolved version: $resolved_version"
step "Update channel: $resolved_channel"

detect_conflicting_install

acquire_install_lock
cleanup_stale_install_artifacts

if ! release_dir_is_complete "$release_dir" "$resolved_version" "$vendor_target" "$install_layout" "$expected_receipt"; then
  if [ -e "$release_dir" ] || [ -L "$release_dir" ]; then
    warn "Found incomplete or provenance-mismatched Electivus release at $release_dir; reinstalling."
  fi

  archive_path="$tmp_dir/$asset"
  checksum_path="$tmp_dir/$checksum_asset"
  installer_checksum_path="$tmp_dir/$installer_checksum_asset"

  step "Downloading Electivus checksum manifests"
  checksum_digest="$(release_asset_digest "$checksum_asset")"
  download_file "$checksum_url" "$checksum_path" "$MANIFEST_MAX_BYTES"
  verify_archive_digest "$checksum_path" "$checksum_digest"
  verify_manifest_assets \
    "$checksum_path" \
    codex-package-aarch64-pc-windows-msvc.tar.gz \
    codex-package-aarch64-unknown-linux-musl.tar.gz \
    codex-package-x86_64-pc-windows-msvc.tar.gz \
    codex-package-x86_64-unknown-linux-musl.tar.gz

  installer_checksum_digest="$(release_asset_digest "$installer_checksum_asset")"
  download_file "$installer_checksum_url" "$installer_checksum_path" "$MANIFEST_MAX_BYTES"
  verify_archive_digest "$installer_checksum_path" "$installer_checksum_digest"
  verify_manifest_assets "$installer_checksum_path" install.sh install.ps1

  expected_digest="$(package_archive_digest "$asset" "$checksum_path")"
  step "Downloading Electivus Codex CLI"
  download_file "$download_url" "$archive_path" "$PACKAGE_MAX_BYTES"
  verify_archive_digest "$archive_path" "$expected_digest"

  step "Installing Electivus standalone package to $release_dir"
  install_package_release "$release_dir" "$archive_path" "$expected_receipt"
fi
if ! release_dir_is_complete "$release_dir" "$resolved_version" "$vendor_target" "$install_layout" "$expected_receipt"; then
  echo "Installed Electivus Codex command or receipt did not match expected release $resolved_version." >&2
  exit 1
fi
activate_release "$release_dir"
add_to_path
release_install_lock
handle_conflicting_install

case "$path_action" in
  added)
    print_launch_instructions
    ;;
  updated)
    print_launch_instructions
    ;;
  configured)
    print_launch_instructions
    ;;
  *)
    step "$BIN_DIR is already on PATH"
    print_launch_instructions
    ;;
esac

printf 'Electivus Codex CLI %s installed successfully.\n' "$resolved_version"
maybe_launch_codex_now
