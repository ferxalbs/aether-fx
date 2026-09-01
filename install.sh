#!/bin/sh
set -eu

repository='ferxalbs/aether-fx'
version=''
install_dir="${HOME}/.local/bin"

fail() {
    printf 'AETHER installer: %s\n' "$1" >&2
    exit 1
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            [ "$#" -ge 2 ] || fail '--version requires a value'
            version=$2
            shift 2
            ;;
        --dir)
            [ "$#" -ge 2 ] || fail '--dir requires a value'
            install_dir=$2
            shift 2
            ;;
        --)
            shift
            [ "$#" -eq 0 ] || fail 'unexpected arguments after --'
            ;;
        *)
            fail "unsupported argument: $1"
            ;;
    esac
done

[ -n "$version" ] || fail 'an explicit release version is required; use --version v0.1.0-alpha-03'
case "$version" in
    v*.*.*) ;;
    *) fail "invalid release version: $version" ;;
esac
case "$version" in
    *[!A-Za-z0-9._-]*) fail "invalid release version: $version" ;;
esac

os=$(uname -s)
architecture=$(uname -m)
case "$os:$architecture" in
    Darwin:x86_64)
        platform='macos-x86_64'
        ;;
    Darwin:arm64|Darwin:aarch64)
        platform='macos-aarch64'
        ;;
    Linux:x86_64|Linux:amd64)
        platform='linux-x86_64-gnu'
        ;;
    Linux:aarch64|Linux:arm64)
        platform='linux-aarch64-gnu'
        ;;
    *)
        fail "unsupported platform: ${os} ${architecture}; supported platforms are macOS x86_64/aarch64 and Linux x86_64/aarch64"
        ;;
esac

archive="aether-${version}-${platform}.tar.gz"
release_url="https://github.com/${repository}/releases/download/${version}"
temporary_directory=$(mktemp -d "${TMPDIR:-/tmp}/aether-install.XXXXXXXX") || fail 'unable to create a temporary directory'

cleanup() {
    rm -rf "$temporary_directory"
}
trap cleanup EXIT HUP INT TERM

curl -fsSL --retry 3 "${release_url}/${archive}" -o "${temporary_directory}/${archive}"
curl -fsSL --retry 3 "${release_url}/SHA256SUMS" -o "${temporary_directory}/SHA256SUMS"

expected_hash=$(awk -v asset="$archive" '
    $2 == asset {
        if (found++) exit 2
        print $1
    }
    END {
        if (!found) exit 3
    }
' "${temporary_directory}/SHA256SUMS") || fail "no unique checksum found for ${archive}"

if command -v sha256sum >/dev/null 2>&1; then
    actual_hash=$(sha256sum "${temporary_directory}/${archive}" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
    actual_hash=$(shasum -a 256 "${temporary_directory}/${archive}" | awk '{print $1}')
else
    fail 'sha256sum or shasum -a 256 is required to verify the download'
fi

[ "$actual_hash" = "$expected_hash" ] || fail "checksum verification failed for ${archive}"

extracted_directory="${temporary_directory}/extracted"
mkdir -p "$extracted_directory"
tar -xzf "${temporary_directory}/${archive}" -C "$extracted_directory"
[ -f "${extracted_directory}/aether" ] || fail 'release archive does not contain aether'

mkdir -p "$install_dir"
cp "${extracted_directory}/aether" "${install_dir}/aether"
chmod 0755 "${install_dir}/aether"

installed_version=$("${install_dir}/aether" --version)
[ "$installed_version" = "${version#v}" ] || fail "installed binary reported ${installed_version}, expected ${version#v}"

printf 'Installed AETHER Fx %s at %s\n' "${version#v}" "${install_dir}/aether"
case ":${PATH:-}:" in
    *":${install_dir}:"*) ;;
    *) printf 'Add this directory to PATH for future shells: export PATH="%s:$PATH"\n' "$install_dir" ;;
esac
