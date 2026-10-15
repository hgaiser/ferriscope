#!/usr/bin/env bash
# Build and install the ferriscope Vulkan WSI layer.
#
#   tools/install-wsi.sh [--prefix PREFIX] [--profile release|debug]
#
# Installs, under PREFIX (default /usr/local; DESTDIR is honoured):
#   lib/ferriscope/libferriscope_wsi.so
#   share/vulkan/implicit_layer.d/VkLayer_FERRISCOPE_wsi.x86_64.json
#
# The manifest carries the final library path (PREFIX, without DESTDIR). The
# layer is enabled by ENABLE_FERRISCOPE_WSI=1 and disabled by
# DISABLE_FERRISCOPE_WSI=1. Only x86_64 is built: a 32-bit game skips the
# layer and presents through Xwayland, as it does under gamescope.
#
# The build goes to CARGO_TARGET_DIR when it is set, and to the workspace's
# target/ otherwise.
#
# Run it as your own user: the layer is built first, and only the install step
# uses sudo, and only when the destination is not writable. Running the whole
# script under sudo is refused, because it would build as root (a root-owned
# target/ and cargo cache) with root's toolchain.
#
# To try the layer without installing into /usr, install into a scratch prefix
# and point the loader at it:
#   VK_ADD_IMPLICIT_LAYER_PATH=PREFIX/share/vulkan/implicit_layer.d
set -euo pipefail

prefix=/usr/local
profile=release
while [ $# -gt 0 ]; do
	case $1 in
	--prefix) prefix=$2; shift 2 ;;
	--profile) profile=$2; shift 2 ;;
	-h | --help) sed -n '2,/^set -euo/p' "$0" | sed 's/^# \{0,1\}//; /^set -euo/d'; exit 0 ;;
	*) echo "unknown argument: $1" >&2; exit 2 ;;
	esac
done

if [ "$(id -u)" = 0 ] && [ -n "${SUDO_USER:-}" ]; then
	echo "run $0 without sudo; it asks for sudo itself, only to install" >&2
	exit 1
fi

root=$(cd "$(dirname "$0")/.." && pwd)
destdir=${DESTDIR:-}
template=$root/tools/wsi/VkLayer_FERRISCOPE_wsi.json.in
target_dir=${CARGO_TARGET_DIR:-$root/target}
cargo_profile=(--release)
[ "$profile" = release ] || cargo_profile=()

# Run "$@" directly if the install destination is writable, otherwise via sudo.
# The nearest existing ancestor of the destination decides.
dest=$destdir$prefix
probe=$dest
while [ ! -e "$probe" ]; do probe=$(dirname "$probe"); done
if [ -w "$probe" ]; then
	as_installer() { "$@"; }
else
	echo "$dest is not writable; using sudo to install" >&2
	as_installer() { sudo "$@"; }
fi

# Build first, as the invoking user, so a build failure installs nothing and
# sudo is only asked for once the layer exists.
echo "building ferriscope-wsi" >&2
(cd "$root" && CARGO_TARGET_DIR=$target_dir cargo build -p ferriscope-wsi "${cargo_profile[@]}")

built=$target_dir/$profile/libferriscope_wsi.so
library=$prefix/lib/ferriscope/libferriscope_wsi.so
manifest_dir=$destdir$prefix/share/vulkan/implicit_layer.d
as_installer install -Dm755 "$built" "$destdir$library"
sed -e "s|@ARCH@|x86_64|g" -e "s|@LIBRARY_PATH@|$library|g" "$template" |
	as_installer install -Dm644 /dev/stdin "$manifest_dir/VkLayer_FERRISCOPE_wsi.x86_64.json"
# What an install before x86_64-only builds left behind: the i386 layer.
as_installer rm -f "$destdir$prefix/lib32/ferriscope/libferriscope_wsi.so" \
	"$manifest_dir/VkLayer_FERRISCOPE_wsi.i686.json"
echo "installed $destdir$library" >&2
